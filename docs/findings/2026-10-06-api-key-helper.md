# Claude Code の apiKeyHelper で ns 認証の JWT を都度発行する

- Date: 2026-10-06
- 対象: Claude Code 2.1.291 / llm-gateway 0.66.0 (stable / unstable とも)

## 判明した事実

### 構成

- helper は `llm-gateway auth sign --key <keys_file> --kid <kid> --sub <sub> --ttl 1h` を exec するだけの sh スクリプト。状態ファイルを持たない。署名は ~0.01s (初回の cold start で 0.4s)
- `auth sign` はトークンを組み立て終えてから 1 回で print する。失敗時 (binary 無し / 鍵束が無い・読めない / kid 不明 / 引数不足) は全て非 0 終了・stdout 0 バイト
- Ed25519 署名は決定的で、claims が `sub` / `iat` / `exp` だけなので、同じ秒に発行した token は同一文字列になる。どちらでも検証は通るので問題はない
- helper 用の kid を鍵束に `>>` で 1 行足すと、stable / unstable とも restart 無しで受理する (`KeyRing::current` が verify のたびに mtime を見て読み直す。v0.60.0 から)
- client の token は ns 認証と events の `subject` / `kid` にだけ使われ、upstream へは渡らない。session の割り当ても token に依らないので、5 分ごとに token が変わっても session の継続性は崩れない

### Claude Code 側のキャッシュの実装 (2.1.291 のバンドルを読んだ結果、実測と一致)

- 既定のキャッシュ寿命は 300000ms (`CLAUDE_CODE_API_KEY_HELPER_TTL_MS` で変更)、出力は printable ASCII 16384 文字まで
- 寿命内はキャッシュを返す。寿命を過ぎると**古い値を返しつつ裏で helper を走らせる** (stale-while-revalidate)。そのため helper の呼び出しは寿命切れ後の最初の request の直後に記録される
- 裏の helper が失敗したら、古い値の timestamp を今に更新して使い続ける。古い JWT がまだ有効なら request は通り、エラーは stderr の `apiKeyHelper failed:` だけ
- キャッシュが JWT で、`exp - 30s` を過ぎていたら、request の前に helper を同期で呼び直す。ただし「キャッシュした時刻が既に `exp - 30s` 以降」(= TTL 30 秒以下の token) なら同期の呼び直しをしない
- 同期の呼び直しが失敗すると、キャッシュを placeholder (空白 1 文字) に置き換える。gateway はこれを `malformed` で 401 にする
- 401 を受けると、キャッシュを捨て (epoch を進めて走行中の helper の結果も捨てる) helper を呼び直して再送する。helper も失敗すると 3 回で `Your apiKeyHelper script is failing` になる
- helper の呼び出しは 1 プロセス内では 1 本に束ねられる (inflight を共有)。プロセス間では共有しない (各自が独立に発行)

### 検証の表

| 項目 | 期待 | 実測 | 判定 |
|---|---|---|---|
| helper 20 本同時起動 | 全部有効 | 20/20 rc=0、stdout 1 行、stderr 0、stable / unstable とも 200 | OK |
| helper 60 本同時起動 (秒をまたぐ) | 全部有効、値が違っても可 | 60/60 rc=0・200。iat が 2 種類で token 2 種類、どちらも受理 | OK |
| 鍵束への `>>` 追記と並行の署名 (コピー上、200 行追記) | 失敗しない | 66 回署名、失敗 0 | OK |
| 鍵束の in-place 書き換え (`>` で上書き) と並行の署名 (コピー上) | - | 289 回中 247 回失敗 (`the key ring has no keys`: 切り詰め直後の空ファイルを読む) | 注意点 |
| 鍵束の一時ファイル + `mv` での置き換えと並行の署名 (コピー上) | 失敗しない | 471 回署名、失敗 0 | OK |
| `claude -p` 3 本同時 (helper のみ、stable 経由) | 全部通る | 3/3 成功、helper 各 1 回、events は全て helper の kid | OK |
| キャッシュ寿命 20 秒で 4 ターン (25 秒間隔) | 呼び直されて通り続ける | 寿命切れ後の各ターンで helper 呼び出し、全ターン成功 | OK |
| JWT 寿命 90 秒・キャッシュ寿命 30 分で期限をまたぐ | 期限切れを送らない | `exp - 30s` 経過後のターンで helper を同期で呼び直し、401 無しで成功 | OK |
| JWT 寿命 20 秒・キャッシュ寿命 30 分で期限 + 60 秒 (gateway の skew) をまたぐ | 401 から回復 | 先回りの呼び直し無し。gateway が `expired` で 401、helper を呼び直して同じターン内で成功 | OK (401 が 1 往復増える) |
| helper が初回に gateway の知らない kid の JWT を返す | 401 から回復 | gateway `unknown_kid` で 401、helper を呼び直して同じターン内で成功 | OK |
| キャッシュ寿命切れの裏の helper が 1 回失敗 (JWT はまだ有効) | 通り続ける | stderr に `apiKeyHelper failed:` が出るだけで全ターン成功、gateway の拒否なし | OK |
| 裏の helper が失敗し続け、JWT が `exp - 30s` を過ぎる | 失敗が見える | そのターン以降 `Your apiKeyHelper script is failing` (placeholder で 401 を 3 回) | 想定通り |
| 上の後で helper が直る | 回復する | 次のターンで placeholder の 401 を 1 回挟んで helper を呼び直し成功 | OK |
| 起動時に helper が失敗 (binary 無し / 鍵束が読めない) | 失敗が見える、値を漏らさない | 2〜3 秒で `Your apiKeyHelper script is failing`、exit 1。stderr に helper の stderr (パスと理由) | OK |
| 使い捨て kid の行を削除 → 戻す (本番の鍵束、一時ファイル + `mv`) | restart 無しで 401 → 200 | 削除直後から stable / unstable とも 401 (`unknown_kid`)、戻すと 200。helper kid の token は終始 200、既存行のバイト列と 600 は不変、プロセスの起動時刻も不変 | OK |
| 動いている session の settings.json を env token → helper に書き換え | - | 書き換え後のターンも env の token のまま (helper は呼ばれない) | 既存 session は再起動まで旧 token |
| 親プロセスの env に `ANTHROPIC_AUTH_TOKEN` があり、settings には helper だけ | - | helper は呼ばれるが request は env の token で送られる | 注意点 |
| 同上で settings の env に `"ANTHROPIC_AUTH_TOKEN": ""` を置く | helper が使われる | request は helper の token | 対策 |

## 実用的な示唆 / ベストプラクティス

- helper の TTL は 1 時間を推す。Claude Code は `exp - 30s` から同期で呼び直すので、TTL が 30 秒を十分超えていれば期限切れの token は送られない。1 時間なら、裏の呼び直しが失敗し続けても約 55 分は古い token で動き続ける (失敗が表に出るまでの猶予)。30 秒以下にすると先回りの呼び直しが効かず、gateway の skew 60 秒を過ぎてからの 401 頼みになる
- 鍵束の行を消すときは、一時ファイルに書いて `mv` で置き換える (`grep -vF '"kid":"<kid>"' f > f.tmp && mv f.tmp f`、umask 077)。`>` での上書きや、切り詰めてから書く editor は、空ファイルの瞬間に helper の署名を失敗させる。gateway 側も空や不正な鍵束は採らずに前の束を保つので、最後の 1 行を消しても失効しない (`jwt.rs` の `current`)
- `ANTHROPIC_AUTH_TOKEN` は apiKeyHelper より優先され、親プロセスの env からも効く。settings の env から外すだけだと、旧 token の入った env を継いだシェル (旧設定の session の Bash tool から起動した `claude` 等) で helper が使われない。settings の env に空文字を置いて上書きする
- settings の env に token を置く形は、Claude Code が起動するすべての子プロセス (Bash tool、hook、MCP server) に token を配る。helper にすると token は Claude Code のプロセス内にだけある

## 未確認

- 対話モード (TUI) での表示。`-p` と stream-json でだけ見た
- macOS のスリープを跨いだ時の挙動。コード上は `Date.now()` で `exp - 30s` を判定するので、復帰後の最初の request で同期の呼び直しになるはず
- settings の env の `ANTHROPIC_AUTH_TOKEN: ""` が対話モードで auth conflict の警告を出さないか

## 参考

- https://code.claude.com/docs/en/settings-reference (apiKeyHelper: 寿命切れ / 401・403 / 期限切れの JWT で呼び直す)
- https://code.claude.com/docs/en/errors (Your apiKeyHelper script is failing、Automatic retries)
- https://code.claude.com/docs/en/authentication (Authentication precedence)
- `crates/gateway-core/src/ns/jwt.rs` (`KeyRing::current`、`SKEW_SECS`)、`crates/llm-gateway-cli/src/auth.rs` (`sign`)
