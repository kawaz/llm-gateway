# 組織ごと OAuth を断られている間も `/api/oauth/profile` は読め、サブスクリプションの状態を返す

- Date: 2026-11-13
- 対象: Anthropic の OAuth 照会口 `GET https://api.anthropic.com/api/oauth/usage` と `GET https://api.anthropic.com/api/oauth/profile`
- 観測したアカウント: 2 つ。サブスクリプションの支払いが滞って停止中のアカウント (llm-gateway が `org_not_allowed` と判定していた経路) と、正常に使えているアカウント
- 送ったヘッダ: `authorization: Bearer <OAuth access token>`、`anthropic-beta: oauth-2025-04-20`、`anthropic-version: 2023-06-01` (両方の口で同じ)

## 判明した事実

- 停止中のアカウントでは `/api/oauth/usage` は **403** を返し、本文は転送 (`/v1/messages`) で受ける組織ごとの断りと同じ形で、機械向けの符号 `details.error_code: "oauth_not_allowed_for_organization"` を持つ
- 同じアカウント・同じヘッダで `/api/oauth/profile` は **200** を返す (停止中でも読める)
- profile の `organization.subscription_status` は `"past_due"` だった
- profile の `organization.payment_auth_hosted_invoice_url` は `"https://claude.ai/settings/billing?action=pay_overdue_invoice"` だった
- profile の `organization.billing_type` は `"stripe_subscription"`、`organization.plan_display_name` は `"Ultra"` だった
- profile には個人と組織を識別する欄 (`account.email` / `account.full_name` / `organization.name`) も載る
- 正常なアカウントの profile は `organization.subscription_status` が `"active"`、`organization.payment_auth_hosted_invoice_url` が `null` だった (`billing_type` / `plan_display_name` は停止中のアカウントと同じ値)
- 2 つのアカウントで違ったのは `subscription_status` と `payment_auth_hosted_invoice_url` の 2 欄で、支払いの滞りはこの 2 欄に出る

## 実用的な示唆 / ベストプラクティス

- 組織ごとの断り (`org_not_allowed`) の理由は、断りの応答からは分からないが、profile の `subscription_status` で手掛かりが取れる。llm-gateway は締め出しを付けたときに 1 度だけ profile を読み、`/llm-gateway/usage` の `auth.subscription_status` に上流の語のまま出し、`past_due` なら `auth.hint` で未払いの請求を払う先を示す
- 対処を言えるのは `past_due` (支払いの滞り) だけ。`active` は正常時の値だが、組織ごと断られている間に `active` が返るなら原因は契約の外にあるので、対処は言わず推定の hint に任せる。他の値 (未観測) も決め打ちせず、状態をそのまま見せる
- 断りの判定は `details.error_code` を優先して読む方が文言の変更に強い。符号の無い本文 (2026-09-09 の観測) もあるので、符号が無ければ文言で見る形を残す
- profile の応答の識別子の欄 (メールアドレス・氏名・組織名) は gateway の出力にもログにも出さない。理由の判断に要る欄だけを読む

## 検証の詳細

### `/api/oauth/usage` (停止中のアカウント)

| 項目 | 結果 |
|---|---|
| 状態コード | 403 |
| `error.type` | `permission_error` |
| `error.message` | `OAuth authentication is currently not allowed for this organization.` |
| `error.details.error_visibility` | `user_facing` |
| `error.details.error_code` | `oauth_not_allowed_for_organization` |

本文 (`request_id` は省略):

```json
{"type":"error","error":{"type":"permission_error","message":"OAuth authentication is currently not allowed for this organization.","details":{"error_visibility":"user_facing","error_code":"oauth_not_allowed_for_organization"}}}
```

考察: 2026-09-09 に転送で観測した本文 (DR-0009) は `details` を持っていなかった。今回は照会口の応答だが、同じ断りに符号が付く形があることが分かった。

### `/api/oauth/profile` (停止中のアカウント)

| 項目 | 結果 |
|---|---|
| 状態コード | 200 |
| `organization.subscription_status` | `past_due` |
| `organization.payment_auth_hosted_invoice_url` | `https://claude.ai/settings/billing?action=pay_overdue_invoice` |
| `organization.billing_type` | `stripe_subscription` |
| `organization.plan_display_name` | `Ultra` |
| `account.email` / `account.full_name` / `organization.name` | 値あり (記録しない) |

考察: 断られている間も読める口なので、締め出しの理由を添える用途に使える。モデルを呼ばない GET で入出力のトークンを消費しないので、締め出し 1 回につき 1 度読む程度なら負担は無視できる (枠への影響は未確認)。

### `/api/oauth/profile` (正常なアカウント)

| 項目 | 結果 |
|---|---|
| 状態コード | 200 |
| `organization.subscription_status` | `active` |
| `organization.payment_auth_hosted_invoice_url` | `null` |
| `organization.billing_type` | `stripe_subscription` |
| `organization.organization_type` | `claude_max` |
| `organization.plan_display_name` | `Ultra` |

### 正常時と支払いの滞りの対比

| 欄 | 正常なアカウント | 停止中のアカウント |
|---|---|---|
| `/api/oauth/profile` の状態コード | 200 | 200 |
| `organization.subscription_status` | `active` | `past_due` |
| `organization.payment_auth_hosted_invoice_url` | `null` | `https://claude.ai/settings/billing?action=pay_overdue_invoice` |
| `organization.billing_type` | `stripe_subscription` | `stripe_subscription` |
| `organization.plan_display_name` | `Ultra` | `Ultra` |

考察: 停止中のアカウントの `organization_type` は記録していないので、対比に入れていない。支払いの滞り以外の止まり方 (解約・管理者の設定など) で何が返るかは未観測。
