# DR-0032: 設定の読み直しを `daemon reload` で行う (監督者経由 + unit ごとの制御 socket)

- Status: Accepted (kawaz 裁定 2026-09-30)。決定 1〜6 は実装済み (`daemon reload`、unit ごとの制御 socket、MANUAL の契機の表)
- Date: 2026-09-30

## 文脈

設定を変えた後、走っている台に反映する手段は `daemon restart` (DR-0028 決定 4 の rolling) しかない。routing / ns / aliases / cache 戦略 / pricing / ns 認証のどれを直しても、台を落として上げ直している。

restart は反映の手段として重い:

- 台が持っている走行状態 (会話と経路の結びつき、経路ごとの締め出しと枠の観測) がすべて消える。直したのが alias 1 行でも、進行中の会話は結びつきを失って設定順に落ちる
- 検証の手順が restart を挟む。経路を 1 本に固定して挙動を見る検証では、固定する・戻すたびに上げ直すことになる

一方、設定以外の入力は既に restart 無しで読み直している。契機はそれぞれ違う:

| 入力 | 読み直す契機 | 正本 |
|---|---|---|
| credential | 周期的に版 (mtime) を見張り、変わっていれば読み直す | DR-0010 / DR-0022 |
| 鍵束 (`keys_file`) | JWT を検証するたびに stat し、mtime が変わっていれば読み直す (読めなければ前の束を保つ) | DR-0030 §5 |
| 設定 (config) | 無い (restart のみ) | — |

設定だけが restart を要する。本 DR は設定の読み直しの契機・経路・原子性を決める。

## 決定

### 1. 契機は明示の `daemon reload [--unit …]`。設定ファイルの mtime は見張らない

```
llm-gateway daemon reload <unit>|--all
```

人 (または人の代わりのセッション) が「書き終えた」と言った時にだけ読み直す。設定ファイルの変化を見張って自動で読み直すことはしない。

keys_file が mtime で読み直せて設定ができない理由は、書き換えの単位が違うことにある。鍵束は 1 行 1 鍵で、追記・削除のどの途中を読んでも「一部の鍵がある束」として意味が通る。設定は違う:

- エディタの保存途中 (切り詰めてから書く、一時ファイルを経由する) を拾うと、読めない・途中までの設定を読む
- `extends` (DR-0013) は土台と派生の複数ファイルからなり、両方を順に書き換える途中は「新しい土台 + 古い派生」の組み合わせになる。各ファイル単体は正しくても、組み合わせは誰も書いていない設定になる

どこまで書けば「書き終えた」かは書いた本人にしか分からない。だから契機は本人の明示に置く。

`--unit` と `--all` の扱いは DR-0028 決定 6 と `Which` の既存の規則に従う。reload は台を動かす命令なので、名前も `--all` も無ければ断る (`Which::choose(_, false)`)。

### 2. 経路は 2 段。CLI → 監督者 → 子

1. **CLI → 監督者**: DR-0028 決定 3 の既存の unix socket (`~/.local/state/llm-gateway/daemon/supervisor.sock`) に `Request::Reload(Which)` を送る。CLI が子へ直接繋ぐ経路は持たない (`start` / `stop` と同じく、監督者不在なら `supervisor_not_running`)
2. **監督者 → 子**: 子は **unit ごとの unix socket** を開いて待ち、監督者だけがそこへ繋いで読み直しを命じる。子は結果を 1 行の JSON で返す

子の制御 socket の置き場は、監督者の socket と登録簿が既に住んでいる `~/.local/state/llm-gateway/daemon/` の下に置く:

```
~/.local/state/llm-gateway/daemon/
  supervisor.sock           監督者 (DR-0028 決定 3)
  units/<unit>.toml         登録簿 (DR-0028 決定 2)
  control/<unit>.sock       子の制御口 (本 DR)
```

- 登録簿の `units/` には混ぜない。登録簿は `units/` の `.toml` だけを数える (`crates/gateway-core/src/daemon/registry.rs:151`) ので混ぜても壊れはしないが、「登録簿のディレクトリ = unit の定義だけ」を保つ
- パスは状態ディレクトリと unit 名から決まる (`protocol::log_path` と同じ形の関数を置く)。子に置き場を引数や設定で教えない — DR-0028 決定 10 がログの置き場を子に教えない理由と同じ
- 子の HTTP (`[server] listen`) とは分ける。制御口は外 (Caddy 越し) から届かない場所にだけ置く

### 3. 原子性: `check` と同じ経路で全体を読み、通った時だけ差し替える

子は命じられたら、起動時と同じ `Config::load` (`extends::resolve` で畳み、`validate` まで通す。`crates/llm-gateway/src/config.rs:1479`) で設定全体を読む。`llm-gateway check` が使う経路と同じである。

- 読めて検証を通った時だけ、走っている設定を新しいものに差し替える
- 読めない・検証に落ちた時は、何も差し替えず旧設定のまま走り続け、理由をエラーとして返す
- 走行中のリクエストは、始まった時に掴んだ設定で最後まで走る。差し替えは次に来たリクエストから効く

部分的に反映する経路 (通った欄だけ差し替える) は持たない。

### 4. reload で変えられない欄は、変わっていれば断る

以下の欄は走っているプロセスが起動時に掴んだもので、読み直しでは変えられない:

- `[server] listen` (待ち受けている socket そのもの)
- store の置き場 (`[store]` の dir) と stats の置き場 (`[stats]` の dir)
- 監督者の unit 定義 (`[server] binary_path` など、監督者が子を起こす時に使うもの)

新しい設定でこれらが旧設定と違っていれば、**読み直し全体を断り**、旧設定のまま「restart が要る」ことをエラーとして返す。黙って旧値のまま他の欄だけ差し替えることはしない — それをすると、ファイルに書いてある値と走っている値が食い違ったまま、どちらが効いているかを誰も言えなくなる。

### 5. 答えは JSON `{unit, ok, error}`

子は監督者に、監督者は CLI に、台ごとの結果を返す。DR-0028 決定 8 の流儀 (単発は JSON、エラーは JSON を stderr・exit 非 0) に従う。

```
llm-gateway daemon reload --all
{"units": [{"unit": "stable",   "ok": true},
           {"unit": "unstable", "ok": false,
            "error": {"kind": "restart_required",
                      "message": "[server] listen changed (127.0.0.1:11301 -> 127.0.0.1:11303); restart the unit"}}]}
```

1 台でも `ok: false` があれば exit は非 0。`--all` は 1 台が失敗しても残りの台への読み直しを続け、全台の結果を並べる (読み直しは台の生死を動かさないので、rolling restart のように前の台の回復を待つ理由が無い)。

### 6. 契機の一覧を MANUAL に 1 表で置く

文脈の表 (credential = 周期監視、keys_file = 検証時の stat、config = 明示の `daemon reload`) を利用者向けに MANUAL へ置く。どの入力を書き換えたら何をすれば効くか、を 1 か所で引けるようにする。

## 却下した案

- **子の HTTP に `POST /llm-gateway/reload` を置く**: 子の HTTP は Caddy 越しに外から届く面である。そこに設定を変える操作を置くと、認証を付けるにせよ付けないにせよ、外から台の挙動を変えられる口が生まれる。制御は外から届かない unix socket に置き、監督者だけが繋ぐ
- **SIGHUP で読み直させる**: シグナルは結果を返せない。読み直しが通ったのか、検証に落ちて旧設定のまま走っているのかを、命じた側はログを読むまで知れない。決定 3 の「失敗は旧設定 + エラー」を命じた人に届ける経路が無い
- **設定ファイルの mtime を見張って自動で読み直す**: 決定 1 のとおり、書きかけと `extends` の書き換え途中を拾う。keys_file は 1 行 1 鍵なので途中を読んでも意味が通るが、設定はそうではない

## 未確定

- **`daemon status` に設定の差を出すか**: 置いてある設定 (`on_disk`) と走っている設定 (`running`) の食い違いを、DR-0028 決定 9 の版の並べ方と同じ形で出すか。後続で決める

## 実装時の判断 (統括 2026-09-30)

- **走行状態の引き継ぎ規則**: 経路の状態 (affinity / spend_down / pace_cap の観測) は「経路名 + その経路が指す credential と provider」が前後で同一の時だけ引き継ぐ。名前が消えた経路の状態は捨てる。同名でも credential か provider が変わった経路は新規扱い (観測は枠に紐づくもので、枠が変われば意味を失う)
- **`Request::Reload` の語**: 既存の引数なし `Request::Reload` (登録簿の読み直し) は `Request::Reload(Which)` に畳む。監督者は reload を受けたら先に登録簿を読み直し、それから指された unit の設定を読み直させる。「reload = ディスク上の変更を拾う」という利用者の語で 1 つにし、別の語を増やさない
- **起動時に写し取った値**: 第一段では全部「変えられない欄」(決定 4) に入れて、変わっていたら `restart_required` で断る (discovery の間隔、webhook、upstream status の設定、keepalive の上限、passthrough の設定)。読み直しで作り直す側へ移すのは、必要になった欄から後続で

## 影響

### 実物照合: `Config` の持たれ方

走っている設定は `Arc<Config>` 1 本で共有されていない。`Config` は値で clone されて各所に配られ、さらに起動時に値を写し取っている箇所がある。差し替えに耐える形かどうかを実物で並べる:

| 箇所 | 持ち方 | 差し替え |
|---|---|---|
| `Gateway.config` (`crates/llm-gateway/src/gateway.rs:38`, 格納は `:147`) | `Config` を値で持つ (`config.clone()`) | 耐えない。`Gateway` は `Arc<Gateway>` として axum の State と裏の仕事に配られている (`crates/llm-gateway-cli/src/daemon/run.rs:75-76`, `:118-122`, `:140-145`) ので、`Gateway` ごと作り直すと走行状態も作り直しになる。差し替え可能な持ち方 (`ArcSwap<Config>` 等) に変える必要がある |
| `Router.config` と `Router.presets` (`crates/llm-gateway/src/router.rs:257`, `:260`, 構築は `:287-305`) | `Config` を値で持ち、経路ごとの `Preset` (経路の状態 `RouteState` を持つ、`crates/llm-gateway/src/provider.rs:210-221`) を設定から組み立てる | 耐えない。`presets` は「経路の状態を持つので作り直さない」と明記された構造 (`router.rs:258-259`)。設定を差し替えると presets の組み直しと状態の引き継ぎ (実装時の判断の節) が同時に要る。**最大の難所** |
| `Router.affinity` (`router.rs:276`) | 値は `Binding { route: Arc<Route> }` (`router.rs:281-284`) で、`Route` は `Arc<Preset>` を抱える (`router.rs:40-42`) | 旧 presets を指したまま残る。引き継ぐなら経路名で新しい `Preset` へ付け替える必要がある |
| `Gateway` の間隔 `refresh_interval` / `watch_interval` (`gateway.rs:42-44`, 写しは `:139-140`) | 起動時に `Duration` へ写す。`keep_models_fresh` がこれで回る (`gateway.rs:354`) | 写し。回っている仕事は値を読み直さない |
| `Stats` / `QuotaStore` (`gateway.rs:143-146`, `:151-154`) | stats の置き場と `listen` (書き手の名前) を起動時に渡す | 決定 4 の「変えられない欄」(stats の dir、listen) だけに依存するので、断る規則で守られる |
| `Keepalive` (`gateway.rs:131-136`) | stats の置き場と `config.stats.uncached_limits()` を写す | 置き場は決定 4 で守られる。上限は写し |
| `Passthrough` (`gateway.rs:158`, 構造は `crates/llm-gateway/src/passthrough.rs:121-133`) | 行き先・秘密の置き場・秘密の枠・バケットを構築時に持つ | 写し。バケット (レート制限の窓) は走行状態でもある |
| `status::Manager` (`gateway.rs:159`, `crates/llm-gateway/src/status.rs:126-137`) | `StatusConfig` と経路の一覧を写し、観測を持つ | 写し。観測は走行状態 |
| webhook の送り先 (`run.rs:127-138`) | `config.webhook.clone()` を起動時に spawn した仕事へ渡す | 写し。仕事ごと作り直す必要がある |
| ns 認証の鍵束 (`crates/llm-gateway/src/config.rs:330-335`, `JwtAuth::from_file`) | `Config::load` の中で鍵束ファイルを読み、`KeyRing` (`Arc` の内側に束を持つ) として設定に入る | 耐える。新しい `Config` を読めば鍵束も読み直され、旧設定を掴んだリクエストは旧束で完走する |
| 監督者 (`crates/gateway-core/src/daemon/supervisor.rs:539-550`) | 子を `<binary_path> daemon run <unit>` で起こす。設定を解釈しない (DR-0028 決定 3) | 監督者側に差し替える設定は無い。足すのは socket の中継だけ |

### `keepalive/pause` も同じ整理の候補

`POST /llm-gateway/keepalive/pause` (`crates/llm-gateway-server/src/lib.rs:60`, 処理は `:320-326`) は無認証で、子の HTTP に置かれている。台の挙動を変える操作が外から届く面にある、という点で決定 2 が HTTP に reload を置かない理由と同じ構図である。制御 socket ができたら、監督者経路へ寄せる候補になる (本 DR では決めない)。

### 版の混在

監督者と子は別の binary でありうる (DR-0028 決定 2)。制御口を持たない版の子が走っていれば、監督者は繋げない。その台は `ok: false` とし、`error.kind` で「この台の版は reload を受けない、restart が要る」と返す (繋げないことを失敗として隠さない)。

### 実装の段分け案

| 段 | 中身 | 完了条件 |
|---|---|---|
| 1 | 設定を差し替え可能な持ち方にする (`Gateway` / `Router` の `Config` を 1 か所から引く形へ)。起動時の写しを「作り直す」か「変えられない欄」かに仕分ける (実装時の判断の節: 第一段は全部「変えられない欄」) | 試験で、差し替え前に掴んだリクエストが旧設定で完走し、差し替え後のリクエストが新設定を見る |
| 2 | 走行状態の引き継ぎ (presets の組み直し、affinity の付け替え)。規則は実装時の判断の節 | 試験で、同名経路の締め出し・枠の観測・結びつきが差し替えを跨いで残り、消えた経路の状態が捨てられる |
| 3 | 子の制御 socket (`daemon/control/<unit>.sock`)。`Config::load` → 決定 4 の比較 → 差し替え、結果を `{unit, ok, error}` で返す | 試験で、検証に落ちる設定・変えられない欄を変えた設定のどちらも旧設定のまま `ok: false` が返る |
| 4 | 監督者の `Request::Reload(Which)` (既存の `Request::Reload` との名前の整理を含む) と CLI の `daemon reload`。`--help`・実装・zsh completion を揃える (cli-design-preferences) | `daemon reload --all` が全台の結果を並べ、1 台の失敗で exit 非 0、残りの台は読み直されている |
| 5 | MANUAL に契機の表 (決定 6) | MANUAL から credential / keys_file / config の反映手段が 1 表で引ける |

## 関連

- docs/issue/2026-09-30-daemon-reload-through-the-supervisor-and-per-unit-control-socket.md (本 DR の元)
- DR-0028 (監督者と unix socket。決定 3 / 6 / 8 / 9 の流儀を本 DR が拡張する)
- DR-0030 §5 (keys_file の mtime による読み直し。契機の使い分けの対比)
- DR-0013 (`extends`。決定 1 の「書き換え途中」の理由)
- DR-0010 / DR-0022 (credential の版と見張り)
- claude-rules-personal の reference `cli-daemon-subcommands` (`reload` を要件に応じて足してよいとする体系の正本)
