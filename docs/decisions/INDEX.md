# Decision Records 一覧

## Active

- [DR-0001](./DR-0001-scope-and-architecture.md) — スコープとアーキテクチャ (v1 で作るもの / 作らないもの)
- [DR-0002](./DR-0002-component-architecture.md) — コンポーネント構成と段階リリース (DR-0001 の前提 2 点を実測で改訂)
- [DR-0003](./DR-0003-beta-flag-negotiation.md) — upstream が拒否する beta フラグを credential 単位で学習する
- [DR-0004](./DR-0004-credential-axes.md) — credential の軸を「認証情報の形」と「話す API」に分ける (Bedrock は composition provider)
- [DR-0005](./DR-0005-distribute.md) — 配布する (GH Release + Homebrew tap + notarize)。DR-0002 の「配布しない」を無効にする
- [DR-0006](./DR-0006-namespace-routing.md) — 既定 namespace を特別扱いせず、`/v1` を `/ns-default` へ内部ルーティングする
- [DR-0007](./DR-0007-usage-visibility.md) — 全 credential の利用量 (5h/7d 使用率・リセット時刻) を /llm-gateway/usage で一括表示する
- [DR-0008](./DR-0008-user-facing-language.md) — プログラムが出す文言 (JSON 値 / error / help) は英語にする。新規から適用
- [DR-0009](./DR-0009-credential-denial-fallback.md) — 401/403/429/529 はこの経路に断られたとみなして次の経路を試す。全滅時は最後の応答を透過し、affinity は 2xx かつ namespace 単位で覚える
- [DR-0010](./DR-0010-credential-cross-process-lock.md) — 認証情報の書き換えを `.lock` サイドカーの flock でプロセス間排他し、控えは版 (mtime) の照合で鮮度を保つ
- [DR-0011](./DR-0011-daily-usage-stats.md) — 応答本文の usage を relay の外の tap で覗き、credential × モデル × 日で積んで writer 毎の日次ファイルに残す (/llm-gateway/stats)
- [DR-0012](./DR-0012-request-events.md) — 転送のたびに起きたことを SSE で流す (/llm-gateway/events)。prompt cache の 5 分を外から数えられるようにする
- [DR-0013](./DR-0013-config-extends.md) — 設定は `extends` で土台の上に重ねる (表は鍵ごとにマージ、配列は置換、消す手段は持たない)
- [DR-0014](./DR-0014-target-architecture-provider-preset.md) — 目標アーキテクチャ: 三境界 (ingress/egress/exchange) と provider = 小 trait の束 (Auth/Wire/Metering/QuotaApi)。core は provider の名前を 1 つも知らない
- [DR-0015](./DR-0015-routing-priority-and-reset-aware-ordering.md) — routing のネストグループ (同格プール) と 7d リセット期限優先の動的順序。provider 非依存
- [DR-0016](./DR-0016-ns-thinking-display-override.md) — ns 単位の thinking.display 強制上書き (CC #49268 の workaround、opt-in)
- [DR-0017](./DR-0017-debug-tap-endpoint.md) — デバッグ用 tap endpoint (購読時のみ動く観測口、本文 opt-in、loopback 直結限定)
- [DR-0018](./DR-0018-spend-down-priority.md) — リセット間際の枠を優先して使い切る (`spend_down_within`、最長周期枠のみ、affinity が上位)
- [DR-0019](./DR-0019-pace-cap.md) — 借りる枠は経過した時間ぶんまで (`pace_cap` の階段予算、按分線を超えたら次段まで控える)
- [DR-0020](./DR-0020-denial-reason-visibility.md) — 外した理由を出力に載せる (events に `skipped`、usage に現在の `denials`。optional 追加のみ)

## Archived

<!-- 現役の文脈を汚す古い DR は decisions/archive/ に退避し、ここに記載 -->

## Moved to research/

<!-- 判断記録の体を成さなくなり research/ に降格した DR -->

## Superseded

<!-- 後続 DR に上書きされた DR (Status: Superseded by DR-XXXX) -->
