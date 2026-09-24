# Decision Records 一覧

Status は各 DR ファイルの `Status:` 行が正本。ここに載るのは今立っている DR だけ。

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
| [DR-0017](DR-0017-debug-tap-endpoint.md) | ✅ 実装済 | デバッグ用 tap endpoint (購読時のみ動く観測口、本文 opt-in、loopback 直結限定) |
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
