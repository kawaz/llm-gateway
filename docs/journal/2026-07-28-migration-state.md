# 移行作業の現在地 (2026-07-28)

セッションが再起動しても続きから拾えるようにするための記録。

## いま動いているもの

| ポート | 中身 | バイナリ | launchd |
|---|---|---|---|
| 8317 | llm-gateway | 開発ツリーの `target/release` | `com.kawaz.llm-gateway` |
| 8318 | cpa 業務面 | Homebrew の cliproxyapi | (業務面の launchd) |
| 8320 | cpa 個人面 (gpt 転送先) | 同上 (`config-relay.yml`) | `com.kawaz.cliproxyapi-relay` |
| 8401 | llm-gateway stable | `~/.local/libexec/llm-gateway-stable` | `com.kawaz.llm-gateway-stable` |
| 8402 | llm-gateway unstable | 開発ツリーの `target/release` | `com.kawaz.llm-gateway-unstable` |

`~/.claude-personal/settings.json` が `http://127.0.0.1:8317` を指しており、
**個人面の Claude Code 全部 (16 セッション前後) がここを通る**。

## 触ってはいけないもの

- **8317 / 8318 のプロセス** (kawaz 指示。放置)
- 8317 は launchd が開発ツリーの `target/release` を指しているので、
  **クラッシュすると私がビルドしたバイナリで再起動する**。今生きているのは
  古い inode を掴んだままのプロセス。kawaz の判断で放置と決定

## 決まっている方針

1. **llm-gateway のリリースを作り Homebrew で入れられるようにする**
2. **stable は Homebrew のバイナリで起動** (私のビルドと物理的に無関係になる)
3. **unstable はローカル main ws の release ビルド**
4. **Caddy のフォールバックは unstable → stable の 2 段**
   (`https://llm-gateway.kawaz-mbp16-20211217.kawaz.jp`)
5. Caddy 経由で動作確認できたら、kawaz が `settings.json` を書き換えて
   **このセッションのプロセスを実験台として再起動する**

### 却下した案とその理由

- **Caddy 経由に一本化** — ローカルのループバック通信に Route53 + ACME +
  1Password MFA の依存を持ち込むことになる。Caddy 自身も SPOF。
  巻き添え源が llm-gateway から Caddy に移るだけ (canddy の指摘)
- **8401 → 8402 → 8317 → 8318 の 4 段フォールバック** —
  8402 と 8317 は同一バイナリ・同一 config なので冗長にならない。
  8318 は業務面なので越境になる (canddy の指摘)
- **cpa を 8403 へ移設** — 番号が揃う以外の実利がない

## 未着手 / 残作業

- **リリース基盤** (今ここ)。前例は `kawaz/hyoui` の
  `.github/workflows/release.yml` (Rust workspace + Cargo.toml が version 正本)
- **DR-0002 の「配布しない」を撤回する改訂**。Homebrew 配布するので前提が変わった
- **ns の設定を書く**。実装済みだが `config.toml` に `[ns.*]` を書いていない。
  kawaz の想定は `/ns-personal` `/ns-<業務面>` + `ANTHROPIC_AUTH_TOKEN`
- **DR-0002 / README が discovery・namespace の実装前の記述のまま**
- **OpenAI 変換 (Phase 2)**。今は gpt を 8320 の cpa へ転送している
- **Bedrock の fable が `thinking.type: "enabled"` で 400**。
  クライアントが再試行して通るが 1 往復無駄。`adaptive` なら通ることは確認済み

## 連絡先

- kawaz との 1on1: ccmsg room `r79`
- canddy (Caddy 担当) との相談: ccmsg room `r80`
