# 走っている 2 台を `service register` 1 本に移す

launchd に直接載っている 2 つの plist を降ろし、`llm-gateway service register` が載せる
監督者 1 つに移す (DR-0028)。`serve` は消えているので、**この移行を終えるまで plist は
起動に失敗する**。

## 今の姿

| label | binary | config | ログ |
|---|---|---|---|
| `com.kawaz.llm-gateway-stable` | `/opt/homebrew/bin/llm-gateway` | `~/.config/llm-gateway/config-11302-stable.toml` | `logs/stable.log` / `stable.err.log` |
| `com.kawaz.llm-gateway-unstable` | `<repo>/main/target/release/llm-gateway` | `~/.config/llm-gateway/config-11301-unstable-new.toml` | `logs/unstable.log` / `unstable.err.log` |

どちらの plist も `<binary> serve --config <path>` を `RunAtLoad` + `KeepAlive` で走らせている。
Caddy は 11301 を優先し、落ちていれば 11302 に回す。

移った後は、OS に載るのは `jp.kawaz.llm-gateway.supervise` (= `llm-gateway daemon supervise`)
だけになり、2 台は登録簿 (`~/.local/state/llm-gateway/daemon/units/*.toml`) の中身になる。

## 移る前に

- **`just ci` が走っていないこと**。`cargo build --release` が
  `target/release/llm-gateway` を差し替えている最中に 11301 を起こすと起動に失敗する
  (2026-09-06 の実事故、10 分ダウン)
- 監督者にするのは **brew の binary** (`/opt/homebrew/bin/llm-gateway`)。`service register` は
  「今の自分」の絶対パスを plist に焼くので、repo の `target/release` から register すると
  ビルドのたびに監督者そのものが差し替わる
- `~/.config` は dotfiles (`~/.dotfiles`) の working copy。config を触ったら向こうで
  コミットする

## 手順

### (a) 台ごとの binary を config に書く

登録簿は `[server] binary_path` を正として焼き込む。書かないと登録した時点の自分自身が
入ってしまい、2 台が同じビルドで走る (= stable と unstable を分けている前提が消える)。

```toml
# ~/.config/llm-gateway/config-11302-stable.toml
[server]
binary_path = "/opt/homebrew/bin/llm-gateway"

# ~/.config/llm-gateway/config-11301-unstable-new.toml
[server]
binary_path = "/Users/kawaz/.local/share/repos/github.com/kawaz/llm-gateway/main/target/release/llm-gateway"
```

```bash
llm-gateway check --config ~/.config/llm-gateway/config-11302-stable.toml
llm-gateway check --config ~/.config/llm-gateway/config-11301-unstable-new.toml
```

### (b) 2 台を登録簿に足す

名前は config のファイル名から作られる (`config-11302-stable`) が、長いので明示する。
以後 `stable` / `unstable` がそのまま unit 名になり、ログも `logs/<unit>.log` になる。

```bash
llm-gateway daemon add --name stable   ~/.config/llm-gateway/config-11302-stable.toml
llm-gateway daemon add --name unstable ~/.config/llm-gateway/config-11301-unstable-new.toml
llm-gateway daemon list
```

`list` の各行の `binary_path` が (a) で書いたものになっているか見る。ここが違うまま進むと、
brew の binary が 11301 を持つ (= 壊れた変更を 11301 に閉じ込める運用が崩れる)。

なお新しい `logs/stable.log` は旧 plist の `StandardOutPath` と同じファイルである。
両方が生きている (c)〜(d) の間は 1 つのファイルに 2 プロセスが書く。追記なので壊れないが、
読むときは混ざる。

### (c) 何が起きるかを見る

```bash
/opt/homebrew/bin/llm-gateway service register --dry-run
```

`contents` の `ProgramArguments` が `/opt/homebrew/bin/llm-gateway daemon supervise` の 3 語で、
`EnvironmentVariables` に `XDG_STATE_HOME` / `XDG_CONFIG_HOME` が入っていることを確かめる
(OS が起こす監督者は shell を通らないので、これが無いと別の状態ディレクトリを見る)。
`commands` は `launchctl bootstrap gui/<uid> <plist>` の 1 本だけ。

### (d) 1 台ずつ入れ替える

Caddy が 11301 を優先しているので、**先に 11302 (stable) を明け渡す**。どの瞬間も
どちらか一方は待ち受けている。

```bash
# 1. 旧 stable を降ろす (この間 Caddy は 11301 に流れる)
launchctl bootout gui/$(id -u)/com.kawaz.llm-gateway-stable

# 2. 監督者を載せる。RunAtLoad で上がり、登録簿の 2 台を起こしにいく。
#    11301 はまだ旧 unstable が持っているので、unstable の子は起動に失敗して
#    backoff に入る (想定どおり。3 で明ける)
/opt/homebrew/bin/llm-gateway service register
curl -sS http://127.0.0.1:11302/llm-gateway/healthz   # 新しい stable が答える

# 3. 旧 unstable を降ろす (この間 Caddy は 11302 = 新 stable に流れる)
launchctl bootout gui/$(id -u)/com.kawaz.llm-gateway-unstable

# 4. 監督者に unstable を起こさせる (backoff を待たずに今すぐ起こす)
llm-gateway daemon start unstable
curl -sS http://127.0.0.1:11301/llm-gateway/healthz
```

確かめる:

```bash
llm-gateway service status     # registered / running / service.pid / instances
llm-gateway daemon status --all  # 2 台とも running: true、restarts が増え続けていない
llm-gateway daemon log unstable | tail
```

起動には model 一覧の取得で 5 秒ほどかかる。`healthz` が返らないうちは待つ。

### (e) 旧いものを片付ける

`daemon status --all` が 2 台とも `running: true` で落ち着いてから。

```bash
rm ~/Library/LaunchAgents/com.kawaz.llm-gateway-stable.plist
rm ~/Library/LaunchAgents/com.kawaz.llm-gateway-unstable.plist
rm ~/.config/llm-gateway/config.toml   # 待ち受けないダミー (DR-0028 決定 6 で不要になった)
```

`config.toml` は `usage` / `stats` / `upstream status` が `--config` 無しで宛先を作るために
置いてあったもの。今はこれらが登録簿から宛先を引くので要らない。消した後に
`llm-gateway usage --unit unstable` が答えることを確かめる。

dotfiles 側で config の変更 (a) と `config.toml` の削除をコミットする。

## 戻し方

`service` を降ろして、旧 plist を載せ直す。`serve` を持つ binary が要るので、
**移行前の版に戻すか、旧 plist の `ProgramArguments` を
`daemon run <unit>` に書き換える**かのどちらかになる。

```bash
llm-gateway service unregister          # 監督者を降ろす (子も畳まれる)
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.kawaz.llm-gateway-stable.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.kawaz.llm-gateway-unstable.plist
```

(e) まで進んで plist を消してしまった後なら、`git`/`jj` の履歴ではなく
`llm-gateway service register --dry-run` の形を参考に書き直すことになる。plist を消すのは
2 台が新体制で落ち着いてからにする。

登録簿だけを畳みたい場合は `llm-gateway daemon remove <unit>`。

## 関連

- DR-0028 (`daemon` / `service` の体系)
- justfile の旧 `install` / `uninstall` / `restart` / `status` / `logs` recipe と
  `dist/com.kawaz.llm-gateway.plist.in` は削除済み。稼働機の登録・切替は
  `llm-gateway service register` / `daemon` サブコマンドで行う
