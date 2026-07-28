# Bedrock の API キーは IAM のサービス固有クレデンシャルで発行する

## 判明した事実

- Bedrock の Anthropic 互換エンドポイントに載せるキーは、**IAM ユーザに紐づく
  サービス固有クレデンシャル** (`aws iam create-service-specific-credential`、
  `--service-name bedrock.amazonaws.com`)
- 値は `ServiceCredentialSecret` (`ABSK` で始まる base64)。これが gateway の
  `payload.api_key` に入る
- **期限がある**。`--credential-age-days` で指定し、応答の `ExpirationDate` が
  実際の失効時刻。これが `payload.expired` に入る
- **1 IAM ユーザにつき 1 キー**。複数持つならユーザを分ける
- 必要なポリシーは `AmazonBedrockLimitedAccess` と
  `AmazonBedrockMarketplaceAccess` の 2 つ
- **IAM 操作に region 指定は要らない** (IAM はグローバル)。region が要るのは
  推論時で、gateway では `config.toml` の `url` に埋まっている

## 実用的な示唆

### キーは region 非依存

キーは IAM ユーザに紐づくので、同じキーで複数 region を叩ける (ポリシーが
許す範囲で)。region ごとに credential を分ける場合、**同じキーを各 json に
入れればよい**。

```toml
[credentials.bedrock-us-east-1]
type = "claude_bedrock"
url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"

[credentials.bedrock-ap-northeast-1]
type = "claude_bedrock"
url = "https://bedrock-mantle.ap-northeast-1.api.aws/anthropic"
```

region ごとに使えるモデルと受理される beta フラグが違いうるので、
credential を分けると discovery と `denied_beta` (DR-0003) が独立に働く。

### 期限切れの扱い

`llm-gateway login` は OAuth 専用なので、Bedrock のキーは作れない。期限が
切れたら上記の手順で再発行し、json を手で更新する。gateway は OAuth でない
credential の更新を試みず、「新しいキーを発行して保存し直してください」と言う。

### ユーザ名に発行時刻を入れておく

`BedrockApiKey-$(date +%Y%m%dT%H%M%S%z)-${AWS_PROFILE}` の形にしておくと、
IAM ユーザ一覧を見ただけでいつ発行したどのプロファイル向けかが分かる。
期限切れの掃除がしやすい。

## 手順

```bash
AWS_PROFILE=<プロファイル>
u="BedrockApiKey-$(date +%Y%m%dT%H%M%S%z)-${AWS_PROFILE}"

# ユーザ作成
AWS_PROFILE=$AWS_PROFILE aws iam create-user --user-name "$u"

# ポリシーをアタッチ
AWS_PROFILE=$AWS_PROFILE aws iam attach-user-policy --user-name "$u" \
  --policy-arn arn:aws:iam::aws:policy/AmazonBedrockLimitedAccess
AWS_PROFILE=$AWS_PROFILE aws iam attach-user-policy --user-name "$u" \
  --policy-arn arn:aws:iam::aws:policy/AmazonBedrockMarketplaceAccess

# キー発行 (応答の ServiceCredentialSecret と ExpirationDate を使う)
AWS_PROFILE=$AWS_PROFILE aws iam create-service-specific-credential \
  --user-name "$u" --service-name bedrock.amazonaws.com --credential-age-days 30
```

発行したキーは credential json に入れる。

```json
{
  "type": "claude_bedrock",
  "priority": 5,
  "payload": {
    "api_key": "<ServiceCredentialSecret>",
    "expired": "<ExpirationDate>"
  }
}
```

### 後始末

```bash
# キー削除
AWS_PROFILE=$AWS_PROFILE aws iam delete-service-specific-credential \
  --user-name "$u" --service-specific-credential-id <ServiceSpecificCredentialId>

# ユーザ削除 (ポリシーを先にデタッチする)
AWS_PROFILE=$AWS_PROFILE aws iam detach-user-policy --user-name "$u" \
  --policy-arn arn:aws:iam::aws:policy/AmazonBedrockLimitedAccess
AWS_PROFILE=$AWS_PROFILE aws iam detach-user-policy --user-name "$u" \
  --policy-arn arn:aws:iam::aws:policy/AmazonBedrockMarketplaceAccess
AWS_PROFILE=$AWS_PROFILE aws iam delete-user --user-name "$u"
```

## 未確認

- `--credential-age-days` の上限。30 で発行できることは確認済み
- `AmazonBedrockLimitedAccess` が全 region を許可するか (= 同じキーで
  複数 region を叩けるか) は未検証
- 期限切れ時に upstream が返すステータスと本文
