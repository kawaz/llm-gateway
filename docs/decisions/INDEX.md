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

## Archived

<!-- 現役の文脈を汚す古い DR は decisions/archive/ に退避し、ここに記載 -->

## Moved to research/

<!-- 判断記録の体を成さなくなり research/ に降格した DR -->

## Superseded

<!-- 後続 DR に上書きされた DR (Status: Superseded by DR-XXXX) -->
