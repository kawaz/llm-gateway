# DR-0028: プロセスの起動と常駐を `daemon` / `service` の 2 系統に分ける

- Status: Accepted
- Date: 2026-09-09

## 文脈

CLI は今、待ち受けを始める口を 1 つしか持っていない (`llm-gateway --help` より):

```
commands:
  serve       start listening
  check       read the configuration and verify it (without starting)
  models      list the models written in the configuration
  usage       list usage per credential (asks the server)
  status      show configured upstream service status (asks the server)
  stats       list token usage and USD cost per credential x model x day
  login       authorize in a browser and save the credential to <name>.json

options:
  --config <path>   configuration file (default: $XDG_CONFIG_HOME/llm-gateway/config.toml)
```

この形は運用の実態と噛み合っていない。

**常駐は CLI の外に置かれている**。実際に走っている 2 台は launchd の plist が直接
`serve --config` を叩いて生かしており、CLI からは見えない:

| label | binary | config |
|---|---|---|
| `com.kawaz.llm-gateway-unstable` | `~/.local/share/repos/github.com/kawaz/llm-gateway/main/target/release/llm-gateway` | `~/.config/llm-gateway/config-11301-unstable-new.toml` |
| `com.kawaz.llm-gateway-stable` | `/opt/homebrew/bin/llm-gateway` | `~/.config/llm-gateway/config-11302-stable.toml` |

台数ぶん plist があり、増減も再起動も `launchctl` を人が直接叩く。CLI は「何が登録されて
いて、何が動いているか」を答えられない。

**問い合わせ系のコマンドが宛先を持てない**。`usage` / `status` / `stats` は稼働中の
gateway に HTTP で聞く作りだが、宛先は `--config` で渡された設定の `[server] listen` から
組み立てる。毎回 `--config` を打たずに済ませるため、`~/.config/llm-gateway/config.toml` は
待ち受けないダミーとして置いてある:

```toml
# --config を指定せずに CLI (usage / stats / models) を使うための設定。
#
# 待ち受けはしない。問い合わせ先は listen の値から組み立てられるので、
# 普段使いの面 (11301 = unstable) を指しておく。

[server]
listen = "127.0.0.1:11301"
disabled = true
```

設定ファイルが「宛先の記憶」を代行しており、面を切り替えるには手で書き換えるしかない。

**語彙が衝突している**。`llm-gateway status` は upstream (Anthropic / OpenAI) の障害状況を
出す (DR-0021)。プロセスの生死を出す口を足そうとすると、同じ語が 2 つの意味を持つ。

結果として、文脈を持たないセッションが「今どっちが動いているか」「片方を上げ直す」を
やろうとすると、plist を読み、config を読み、`launchctl` の label を知る必要がある。

## 決定

kawaz が全ツール共通で使う `daemon` / `service` のサブコマンド体系を採る。体系そのものの
正本は claude-rules-personal の `knowledge` skill 内 `reference/cli-daemon-subcommands.md`
で、本 DR はそれを llm-gateway に当てはめた結果を書く。

体系の骨子はこう:

- `daemon` は **この instance のプロセス操作**、`service` は **OS への常駐登録**
- 引数なし / `--help` はテキストの help。それ以外の出力は JSON、追従は JSONL。
  エラーも JSON で stderr、exit は非 0
- 子を持つレベルと必須引数のあるコマンドは、引数なしなら help を出す

### 1. `serve --config` を `daemon run <unit>` に置き換える

`serve` は消す。別名は残さない (自作ツールに互換層を作らない)。走っている 2 台は plist を
書き換える移行手順を runbook に置く。

```
llm-gateway daemon run [unit]        foreground で 1 台走らせる (unit 省略時は既定)
llm-gateway daemon supervise         foreground の監督者 (登録された unit を子として抱える)
llm-gateway daemon add <config>      unit を登録簿に足す (--name <name>)
llm-gateway daemon remove <unit>     登録簿から外す
llm-gateway daemon list              [{id,unit,running,pid}]
llm-gateway daemon start <unit>|--all
llm-gateway daemon stop <unit>|--all
llm-gateway daemon restart <unit>|--all
llm-gateway daemon status [<unit>]|--all
llm-gateway daemon log [<unit>]|--all  [--follow]
```

### 2. unit = 設定ファイル 1 つ。登録簿は状態ディレクトリに置く

`~/.local/state/llm-gateway/daemon/units/<name>.toml` に 1 unit 1 ファイル。`name` は
`daemon add --name <name> <config-path>` で与え、省略時は設定ファイルの basename から拡張子を
落としたもの (`config-11301-unstable-new.toml` → `config-11301-unstable-new`)。

unit が持つのは設定ファイルのパス、`enabled` (後述の desired state)、そして
**実行する binary のパス**。binary は設定側の `[server] binary_path` を正とし、書かれて
いなければ登録した時点の自分自身の絶対パスを焼き込む。

binary を unit ごとに持つのは、いま走っている 2 台が **別のビルドだから**である
(11302 = brew の stable、11301 = repo の release build)。1 つの監督者が両方を抱えるには、
監督者の binary と子の binary が別でありうる必要がある。

### 3. `supervise` は exec するだけの監督者

登録された unit ごとに `<binary_path> daemon run <unit>` を子プロセスとして起動し、落ちたら
backoff を置いて上げ直す。SIGTERM を受けたら子を順に止めて終わる。監督者自身は HTTP を
持たず、設定の解釈もしない — 解釈するのは子の `daemon run` である。

`start` / `stop` は子を直接叩かず、**登録簿の `enabled` (= desired state) を書き換えて
監督者に知らせる**。知らせ方は unix socket か、signal + 登録簿の読み直し。定期的に舐めて
差分を見つける作りにはしない (ポーリングは間隔に根拠が無く、往復を取りこぼす)。

### 4. `restart --all` は 1 台ずつ (rolling)

登録の逆順 (11302 → 11301) に 1 台ずつ落として上げ、`/llm-gateway/healthz` が戻ってから
次へ進む。Caddy は 11301 を優先し、落ちていれば 11302 に回す構成なので、順に上げ直せば
外から見た断は生じない。全台を同時に落とす経路は持たない。

### 5. `status` を `upstream status` に移す

DR-0021 の upstream 障害状況は `llm-gateway upstream status` になる。別名は残さない。
`daemon status` はプロセスの状態 (`{id,unit,running,pid,version,...}`)、
`service status` は登録の状態、と 3 者の意味が語で分かれる。

### 6. 稼働中に問い合わせる系は、登録簿から宛先を引く

`usage` / `stats` / `upstream status`、および HTTP でしか叩けない keepalive の pause
(DR-0027 が維持する `POST /llm-gateway/keepalive/pause`) を CLI から呼ぶ場合も同じ:

- `--config` は必須にしない。登録簿の unit から `[server] listen` を読んで宛先を作る
- どの unit に聞くかは `--unit <name>`
- 登録が 1 つならそれを使う。複数あって未指定ならエラーにし、名前か `--all` を求める

これで `~/.config/llm-gateway/config.toml` の「待ち受けないダミー」は不要になり、削除する。
`--config` は `daemon add` と `check` のように**ファイルそのものを対象にするコマンド**にだけ
残る。

### 7. OS への登録は監督者 1 つだけ

```
llm-gateway service register|unregister
llm-gateway service start|stop
llm-gateway service status     {registered,running,pid,service:{loaded,running,pid,last_exit},instances:[...]}
llm-gateway service log [--follow]
```

`register` が OS に載せるのは `llm-gateway daemon supervise` **1 つ**。macOS は
`~/Library/LaunchAgents/<label>.plist`、Linux は systemd の user unit。label は
`jp.kawaz.llm-gateway` 系にして、既存の `com.kawaz.llm-gateway-{stable,unstable}` と
衝突させない (移行の途中で両方が載っても、片方だけを外せる)。

既存 2 plist は runbook の手順で `launchctl bootout` してからファイルを消す。

### 8. 出力は JSON、help はテキスト

単発の結果は JSON、`log --follow` のような追従は JSONL。エラーは JSON を stderr に出し、
exit を非 0 にする。help だけはテキストで、引数なしでも出す。

`--help` の文面・実装・zsh completion の 3 者を揃えることを受け入れ条件に入れる
(cli-design-preferences)。

## 却下した案

- **plist を 2 本のまま置き、CLI からは触らない**: 台数を増やすたびに人が plist を書く。
  「今何が動いているか」を CLI が答えられない状態が続き、決定 6 の宛先解決も作れない
- **`serve` を `daemon run` の別名として残す**: 同じことをする口が 2 つ増える。移行対象は
  自分の plist 2 本だけなので、互換のために語彙を濁す理由がない
- **binary を 1 系統に統一して `binary_path` を持たない**: stable (brew) と unstable
  (repo build) を同時に走らせて、壊れた変更を 11301 に閉じ込めるのが今の運用そのもの。
  統一すると運用の前提が消える
- **監督者が登録簿を定期的に舐めて差分を取る**: 間隔に根拠が無く、間隔の内側で起きた
  往復 (stop → start) を取りこぼす
- **`daemon restart --all` を同時再起動にする**: 短くても全断が出る。rolling で避けられる
  ものを受け入れる理由がない

## 未確定 (実装前に確かめる)

- **子のログの置き場と回転**。今は plist が `~/.local/state/llm-gateway/logs/{stable,unstable}.log`
  へ流している。監督者が受けて unit 名で分けるのか、子が自分で書くのか。回転は誰が持つのか
- **監督者が落ちた時に子をどうするか**。道連れにするのか、生かしたまま次の監督者が拾うのか。
  拾うなら pid の引き継ぎ方が要る
- **systemd 側の検証環境**。手元は macOS のみで、user unit の登録は書けても実機で確かめ
  られていない

## 影響

- `serve` が消え、`status` が `upstream status` に動く。README / MANUAL / zsh completion の
  該当箇所が変わる
- `~/.config/llm-gateway/config.toml` (待ち受けないダミー) を削除する
- launchd の 2 plist を外し、`service register` 1 本に移す runbook が要る
- DR-0021 の endpoint (`/llm-gateway/status`) はそのまま。変わるのは CLI の語だけ
- `[server]` に `binary_path` が増える

## 関連

- docs/issue/2026-09-09-daemon-service-subcommands.md (本 DR の元)
- claude-rules-personal の `knowledge` skill `reference/cli-daemon-subcommands.md`
  (`daemon` / `service` 体系の正本)
- DR-0021 (upstream の状態表示、CLI の語が `upstream status` に移る)
- DR-0027 (keepalive の pause API、決定 6 の宛先解決の対象)
