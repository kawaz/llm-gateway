# Decision Records 一覧

Status は各 DR ファイルの `Status:` 行が正本。ここに載るのは今立っている DR だけで、置き換えられた DR は [archive/INDEX.md](archive/INDEX.md) にある。

状態は絵文字とラベルだけ。日付・Phase・裁定の内訳は各 DR 本文に書く。

- `💭 提案`: Decision がまだ裁定されていない。実装に着手しない (本文の `Status: Proposed`)
- `✅ 実装済`: Decision の全部に実装エビデンスがある
- `🟡 部分実装`: 一部のみ実装
- `⬜ 未実装`: 裁定済みで設計のみ、実装エビデンスなし (着手してよい)
- `🚧 進行中`: 実装の途中
- `N/A`: 実装対象でない (命名・思想・プロセス等)
- `❌ 撤退`: 撤退判断済

| DR | 状態 | 説明 |
|---|---|---|
| [DR-0001](DR-0001-scope-and-architecture.md) | ✅ 実装済 | スコープとアーキテクチャ (v1 で作るもの / 作らないもの) |
| [DR-0002](DR-0002-component-architecture.md) | 🟡 部分実装 | コンポーネント構成と段階リリース |
| [DR-0003](DR-0003-beta-flag-negotiation.md) | ✅ 実装済 | upstream が拒否する beta フラグを credential 単位で学習する |
| [DR-0004](DR-0004-credential-axes.md) | 🟡 部分実装 | credential の軸を「認証情報の形」と「話す API」に分ける (Bedrock は composition provider) |
| [DR-0005](DR-0005-distribute.md) | ✅ 実装済 | 配布する (GH Release + Homebrew tap + notarize) |
| [DR-0006](DR-0006-namespace-routing.md) | ✅ 実装済 | 既定 namespace を特別扱いせず、`/v1` を `/ns-default` へ内部ルーティングする |
| [DR-0007](DR-0007-usage-visibility.md) | ✅ 実装済 | 全 credential の利用量 (5h/7d 使用率・リセット時刻) を /llm-gateway/usage で一括表示する |
| [DR-0008](DR-0008-user-facing-language.md) | N/A | プログラムが出す文言 (JSON 値 / error / help) は英語にする |
| [DR-0009](DR-0009-credential-denial-fallback.md) | ✅ 実装済 | 401/403/429/529 はこの経路に断られたとみなして次の経路を試す。全滅時は最後の応答を透過し、affinity は 2xx かつ namespace 単位で覚える |
| [DR-0010](DR-0010-credential-cross-process-lock.md) | ✅ 実装済 | 認証情報の書き換えを `.lock` サイドカーの flock でプロセス間排他し、控えは版 (mtime) の照合で鮮度を保つ |
| [DR-0011](DR-0011-daily-usage-stats.md) | ✅ 実装済 | 応答本文の usage を relay の外の tap で覗き、credential × モデル × 日で積んで writer 毎の日次ファイルに残す (/llm-gateway/stats) |
| [DR-0012](DR-0012-request-events.md) | ✅ 実装済 | 転送のたびに起きたことを SSE で流す (/llm-gateway/events)。prompt cache の 5 分を外から数えられるようにする |
| [DR-0013](DR-0013-config-extends.md) | ✅ 実装済 | 設定は `extends` で土台の上に重ねる (表は鍵ごとにマージ、配列は置換、消す手段は持たない) |
| [DR-0014](DR-0014-target-architecture-provider-preset.md) | 🟡 部分実装 | 目標アーキテクチャ: 三境界 (ingress/egress/exchange) と provider = 小 trait の束 (Auth/Wire/Metering/QuotaApi)。core は provider の名前を 1 つも知らない |
| [DR-0015](DR-0015-routing-priority-and-reset-aware-ordering.md) | ✅ 実装済 | routing のネストグループ (同格プール) と 7d リセット期限優先の動的順序。provider 非依存 |
| [DR-0016](DR-0016-ns-thinking-display-override.md) | ✅ 実装済 | ns 単位の thinking.display 強制上書き (CC #49268 の workaround、opt-in) |
| [DR-0018](DR-0018-spend-down-priority.md) | ✅ 実装済 | リセット間際の枠を優先して使い切る (`spend_down_within`、最長周期枠のみ、affinity が上位) |
| [DR-0019](DR-0019-pace-cap.md) | ✅ 実装済 | 借りる枠は経過した時間ぶんまで (`pace_cap` の階段予算、按分線を超えたら次段まで控える) |
| [DR-0020](DR-0020-denial-reason-visibility.md) | ✅ 実装済 | 外した理由を出力に載せる (events に `skipped`、usage に現在の `denials`) |
| [DR-0021](DR-0021-upstream-service-status.md) | ✅ 実装済 | upstream の公式状態と gateway の実測状態を `/llm-gateway/status` で一括表示し、529 時に background refresh する |
| [DR-0022](DR-0022-credential-update-triggers-discovery.md) | ✅ 実装済 | 認証情報ファイルの版 (mtime) を見張り、更新に気づいたら `refresh_secs` を待たずに model catalog を取り直す |
| [DR-0023](DR-0023-web-login-endpoint.md) | ✅ 実装済 | Web 経由の OAuth 再認証口 (`/llm-gateway/login`、claude_oauth のみ、手動コード貼り付けフロー) |
| [DR-0024](DR-0024-cache-strategy-and-keepalive.md) | ✅ 実装済 | prompt cache 戦略を ns × モデル glob × main/sub で設定し、keepalive を仕掛ける条件と止める口を決める |
| [DR-0025](DR-0025-responses-ingress.md) | ✅ 実装済 | Responses 形式の受け口を無変換パススルーで生やす (認証だけ差し替え、運べる形は経路が答える、origin=codex、cache 戦略は当てない) |
| [DR-0026](DR-0026-discovery-client-version.md) | ✅ 実装済 | discovery が codex backend に名乗る `client_version` は codex CLI の版を定数で持つ |
| [DR-0027](DR-0027-keepalive-by-replay.md) | 🟡 部分実装 | keepalive は gateway が最後に転送した本文を自送信して cache の TTL を延ばす (本文はファイルに置き flock で 1 台に絞る、sub も対象にできる、cache に乗った 1 本だけ控える) |
| [DR-0028](DR-0028-daemon-service-subcommands.md) | 🟡 部分実装 | プロセスの起動と常駐を `daemon` (instance の操作) / `service` (OS への登録) に分ける |
| [DR-0029](DR-0029-stats-origin-axis.md) | ✅ 実装済 | 日次集計の鍵にモデルの下の「出した側」(origin) を足す (keepalive の自送信を本数・トークン・USD で分けて読む) |
| [DR-0030](DR-0030-general-purpose-auth-gateway.md) | 🟡 部分実装 | 汎用の認証 gateway を crate として下に敷き、LLM をその上の 1 利用者にする (任意 API への認証差し替えパススルー、自主レート制限、ns の allowlist と JWT 認証) |
| [DR-0031](DR-0031-store-layer.md) | 🟡 部分実装 | 永続化の器を一貫性の意味論 (単一 writer の更新 / リース / 合算可能なカウンタ / LWW スナップショット) で 4 つの trait に切り、file backend をその 1 実装にする |
| [DR-0032](DR-0032-daemon-reload.md) | ✅ 実装済 | 設定の読み直しを `daemon reload` で明示的に行う (CLI → 監督者 → unit ごとの制御 socket、`check` と同じ経路で検証して通った時だけ差し替え、変えられない欄の変更は restart を求めて断る) |
| [DR-0035](DR-0035-judge-thinking-crossing-by-session-sources.md) | ✅ 実装済 | thinking の跨ぎ (生成と別のモデル、account 束縛モデル (設定、既定は Sonnet 5.5) なら別の account) を thinking の連なり (session と、subagent ならその agent id) ごとの出所 tuple (account, model) で判定し、`on_thinking_crossing` (`stay` / `drop_thinking` / `thinking_as_text`、namespace ごと、最上位は既定) で振る舞いを選ぶ。対象は履歴に thinking を持つ 1 本だけ。出所は `[stats] dir` の共有ファイルに永続化し (寿命は `seen` から 24h)、`thinking_as_text` で跨いだ session は以後 thinking を改行を保った本文だけの text として送る |
| [DR-0036](DR-0036-low-priority-requests-wait-instead-of-switching.md) | ✅ 実装済 | クライアントが低優先度で送った 1 本 (`anthropic-usage-limit: slow`、Claude Code の `/low-priority`) は DR-0009 の例外にし、429 / 529 を経路を替えず印も付けずに透過する。全経路が印で塞がっていても router の 429 を組まず設定順に当て、pace_cap (DR-0019) にも掛けない。401 / 403 / 5xx と組織ごとの断りは従来どおり。events の `request` に `low_priority` |
| [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) | ✅ 実装済 | ns 認証 `jwt` の鍵を各 unit の daemon が入れ替え (`[stats] dir` の `jwt-keys/<ns>.jwks.jsonl`、`.lock` の flock、印のある自動の鍵だけを後継 + `max_token_ttl` + 1h で消す、周期は `key_rotation` 既定 7d)、検証は手の鍵束と自動の鍵束の両方を見る。発行は読むだけの `auth token` (最新の自動の鍵、無ければ `--kid`、TTL 既定 1h) |
| [DR-0038](DR-0038-timed-request-dump.md) | ✅ 実装済 | リクエストと応答の本文を `dump start --ns <ns>` で時限と容量の上限つきで一時的にダンプする。設計の一般形は claude-rules-personal の reference `proxy-dump-design` が正本で、DR は llm-gateway 固有の点だけ (制御 socket 経由の最上位 `dump start\|stop\|list\|prune` と `--follow`、一致条件は `--ns` だけ、`[dump]` に歯止めと既定値だけ (`timeout` 5m / `max_bytes` 200MiB / `max_timeout` 24h / 全体 2GiB / fs 空き 1GiB / 片付け 48h)、置き場は `[stats] dir` の `dumps/<unit>/<ns>/<dump_id>/` で `dumps/` 全体を unit 起動時と start 時に掃く、`seq` は events と同じ番号、`store` は `raw` だけ、試行ごとの `request_rewrites` に gateway の書き換え、認証系ヘッダは `is_authorization`、答えは `{unit, ok, …}` で時刻は Unix ミリ秒、`/self` と `daemon status` の `dump`、tap の廃止) |
| [DR-0039](DR-0039-credential-events-and-llm-request.md) | 💭 提案 | 締め出しの印の遷移を events の語彙に足し (`credential_limited {route, credential?, reason, model?, until, by, ns?, fallback?}` / `credential_recovered {route, credential?, reason, model?, by}`、scope ごとの有効な印の無→有 / 有→無で 1 回だけ、`until` の経過は timer、`paced` は対象外)、refresh token の無効化を `credential_refresh_failed {credential, reason, routes, login_path?}` (`record_auth` の状態遷移で 1 回、再ログインで `credential_recovered`)、秘密の寿命はファイルの `expires_at` (人が書く) と `issued_at` (gateway が入れる) を 1 時間ごとに読んで `credential_expiring` / `secret_expiring` / `*_expired` を (id, expires_at) ごとに 1 回 (`[notice] expiry_ratio` 0.9 / `expiry_before` 7d、推定しない) として足し、転送の知らせを `request` → `llm_request` に改名して `type` を持たせ、全種で `credential` = 認証情報名 / `route` = 経路名に分ける (`llm_request` / `response` / `skipped` も)。`credential_limited` / `recovered` は DR-0042 の pull で代替可、`llm_request` 改名と `refresh_failed` / 寿命通知は残る候補。DR-0042 の後に再評価 |
| [DR-0040](DR-0040-hooks-deliver-events-to-urls.md) | 💭 提案 | 選んだイベント (語彙は DR-0039) を `[[hooks]]` (`on` + top-level 欄の一致 `match = { ns = "…" }` (`*` glob、AND)、`url` / `method` / `headers` / `body` / `timeout` / `follow_redirects`) でプアなテンプレート (`{{field}}` / `{{a.b}}` / `{{event}}` / `{{secret:<id>}}`、JSON 文脈なら文字列中はエスケープ・裸なら JSON 値、無い欄はその 1 件を発火せず log) で整形して配る。hooks は events の購読者の 1 つで events は変えない。配達は hook ごとの別タスク + bounded queue で落とした数は `/self` の `hooks.dropped`、失敗は log のみ (通信失敗 / 5xx / timeout だけ 1 回送り直し)。`[webhook]` は廃止し `check` が書き換え例を出して断る。ccmsg 連携は DR-0042 の pull に移り、本 DR の用途は bark 等 push でしか届かない相手に限る。DR-0042 / 0043 の後に再評価 |
| [DR-0041](DR-0041-hooks-sign-deliveries-with-ephemeral-jwks.md) | 💭 提案 | `[[hooks]]` の配達を gateway の短命の鍵で JWT 署名し、受け口は gateway の JWKS で検証する (方式は reference `auth-patterns/webhook-jws-jwks`)。鍵は hooks 専用で DR-0037 の ns の鍵は使わず、`[stats] dir` の `hooks/signing.jwks.jsonl` を stable / unstable が flock で共有して 3 状態 (行の `active_from` / `lifetime` 1h 固定) を回し、どの unit も同じ束を `GET /llm-gateway/hooks/jwks` で出す (Caddy の fail over 下で前提 P4 を満たす)。`[[hooks]]` に `sign` (既定 false) / `aud` (既定 `url`) を足し、署名時は `application/jwt` で claims の `event` に `body` の展開結果、試行ごとに署名し直す。`{{secret:…}}` の header とは併用可、受け口の `jwks_url` は Caddy 越しの FQDN (https)、`iss` は入れない。ccmsg 連携は DR-0042 の pull に移り、本 DR の用途は bark 等 push でしか届かない相手に限る。DR-0042 / 0043 の後に再評価 |
| [DR-0042](DR-0042-session-keepalive-state-endpoint.md) | 💭 提案 | 会話 (session) 単位の cache / keepalive の状態を問う読み取り専用の口 `GET /llm-gateway/sessions[/{session_id}]` (`?ns=` で絞る一覧)。ccmsg との連携は ccmsg が Claude Code の hook のたびに取りに来る pull だけ (gateway から押す経路は使わない)。答えは keepalive の控え (DR-0027) の写しで、系列 (session_id, prefix) ごとに `prefix` / `ns` / `model` / `route` / `cache_notice` / `cache_expires_at` / `cache_since` / `cache_count` / `next_keepalive_at` / `cache_until` / `cache_until_count` (欄名と意味は DR-0012 を借りる) と、その面とモデルで試される経路の枠の印 `routes[] {route, credential?, limited?: {until}}` (`limited` だけ、`busy` は載せない。DR-0039 の `credential_limited` / `recovered` を代替)。控えに無いもの (context のトークン数、cache の結果の語、`paused`、`agent_id`、分岐時間) は欄を持たず、控えの無い会話は `series: []` で 200。呼び手の認証は DR-0043 (無認証の段階は持たない)、ns の下には置かない、書き込みは足さず pause は `keepalive/pause` のまま、呼び手は `ANTHROPIC_BASE_URL` の末尾の `ns-<name>` を落とした根に `/llm-gateway/…` を足す。events の SSE は curl の確認用に残す |
| [DR-0043](DR-0043-authenticate-operations-callers-by-their-jwks.md) | 💭 提案 | 運用系 HTTP の呼び手を呼び手の JWKS で認証する (方式は reference `auth-patterns/webhook-jws-jwks` の S / R を逆に読む。ccmsg が最初の呼び手)。設定は `[callers.<name>]` (`jwks_url` (https) か `jwks_file` のどちらか 1 つ、`iss` 既定は名前で必須、`aud` の欄は無し)。署名を求めるのは `sessions*` / `usage` / `status` / `stats` / `events` / `keepalive/pause`、求めないのは `healthz` / `version` / `self` (監督者が問う) / `login*` (ブラウザ) / `hooks/jwks`。token は `Authorization: Bearer` の JWT (`typ: request+jwt`、`alg` は `Ed25519` / `EdDSA`、claims は `iss` / `aud` = `<METHOD> <path>` / `exp` (呼び手は +5 秒、gateway の上限 10 秒、ずれの猶予は足さない) / 任意の `jti`、`iat` は無し、query と body は覆わない)。検証は reference の順序、401 / 400 の分け方も同じ、`jti` の集合は unit ごと (兄弟と共有しない)。鍵キャッシュは専用の小さな HTTP client で https のみ (interval 30 秒 / 定期 10 分 / stale 66 分)、`jwks_file` は mtime で読み直す。`[callers]` が 1 つでもあれば署名必須 (無ければ無認証のまま)。CLI の `usage` / `stats` / `upstream status` は `jwks_file` の呼び手として `--caller` で署名する |
