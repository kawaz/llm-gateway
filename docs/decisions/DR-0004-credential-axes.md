# DR-0004: credential の軸を「認証情報の形」と「話す API」に分ける

## Context

DR-0003 の実装で credential json を `type` + 運用設定 + `payload` に分けたが、
`type` が 2 つのことを同時に表している。

| `type` | 認証方式 | upstream |
|---|---|---|
| `claude_oauth` | OAuth | Anthropic 公式 |
| `claude_bedrock` | API key | Bedrock |
| `codex_oauth` | OAuth | ChatGPT |
| `relay` | (なし) | 別 gateway |

`claude_oauth` と `claude_bedrock` は**認証方式が全く違う**のに `claude` で
括られ、`claude_oauth` と `codex_oauth` は**同じ OAuth** なのに別物に見える。
軸が交差している。

さらに Bedrock は 1 つのホストで 2 つの API を出し分けることが分かった。

```
https://bedrock-mantle.{region}.api.aws/anthropic   ← Anthropic Messages API
https://bedrock-mantle.{region}.api.aws/v1          ← OpenAI 互換 API
```

モデル ID の prefix が経路を決める (`anthropic.*` / `openai.*`)。同じ API キーで
両方叩ける。`claude_bedrock` という 1 つの名前では表せない。

## Decision

### 1. credential の軸を 3 つに分ける

```json
{
  "type": "bedrock_service_specific_credential",
  "provider": "bedrock",
  "region": "us-east-1",
  "priority": 5,
  "payload": {
    "ServiceSpecificCredential": {
      "ServiceCredentialSecret": "...",
      "ExpirationDate": "2026-08-27T11:09:17+00:00",
      "ServiceSpecificCredentialId": "ACCA...",
      "UserName": "BedrockApiKey-...",
      "Status": "Active"
    }
  }
}
```

| 軸 | 意味 | 決めるもの |
|---|---|---|
| `type` | **認証情報の形** | payload の構造 |
| `provider` | **話す API** | backend adapter と URL のパス |
| 範囲 (`region` 等) | 資格が有効な範囲 | provider ごとに要るものが違う |

`type` の対応:

| `type` | `provider` |
|---|---|
| `claude_oauth` | `claude` |
| `codex_oauth` | `openai` |
| `bedrock_service_specific_credential` | `bedrock` |
| `relay` | — |

### 2. payload はプロバイダの応答をそのまま入れる

加工して詰め直さない。Bedrock なら
`aws iam create-service-specific-credential` の応答の `ServiceSpecificCredential`
をラップごと入れる。

**ラップを残す理由**: Bedrock には長期キー (サービス固有クレデンシャル) と
短期キー (12 時間有効、発行したプリンシパルの権限を継ぎ、発行 region のみ) が
あり、形式が違う。ラップ名が「どの発行経路で作られたか」を表す。短期キーを
足すときは別のキー名でぶら下がるので、payload を見れば判別できる。

**生のまま持つ利点**: `ServiceSpecificCredentialId` と `UserName` が
**キーの削除に要る**。抜き出した 2 フィールドだけだと、捨てるときに AWS を
調べ直すことになる。`Status` / `CreateDate` も状態の判断に使える。

### 3. Bedrock は composition provider

モデル ID の prefix で内部の provider に振り分ける。

```
BedrockProvider
├── anthropic.* → BedrockAnthropicProvider → /anthropic (Messages API)
└── openai.*    → BedrockOpenAIProvider    → /v1 (Responses API)
```

credential は 1 つ。キーが両方で使えるので分ける理由がない。

### 4. provider は「API 実装 + 認証と名前空間のラッパー」

現行の `Bedrock` は Anthropic Messages API の実装をそのまま使い、4 点だけ
差し替えている。

| 差分 | 内容 |
|---|---|
| `authorize` | `Authorization: Bearer` → `x-api-key` |
| `base_url` | `/anthropic` を指す |
| `beta_policy` | 拒否リストを持つ (DR-0003) |
| `adapt` | `model` を `anthropic.*` に書き換え |

OpenAI 側も同じ形になる。`OpenAIProvider` (Responses API) ができたら、
認証と名前空間だけ差し替えてラップする。小細工が要らなければ直接移譲でよい。

### 5. `BedrockOpenAIProvider` は空実装で配線だけ通す

`OpenAIProvider` がまだ無いのでラップできない。`Error::NotImplemented` を
返すだけの実装を置き、**なぜ空なのかをコメントに書く**。「対応しない方針」と
「まだ書いていない」を読み分けられるようにする。

**discovery は当面 `openai.*` を拾わない。** 拾うと `/v1/models` に載って
クライアントのモデル選択に現れ、選ぶと失敗する。使えるものだけ一覧に出すのが
gateway の既定の振る舞い (`filter.exclude` も同じ思想)。変換層ができたら
discovery 側の 1 行を変えて有効化する。

## Alternatives Considered

**`type` に upstream を含めたまま** — `claude_bedrock` に OpenAI 経路を足すと
`claude_bedrock_openai` のような名前になり、`claude` が意味を失う。Phase 2 で
確実に破綻する。

**Bedrock の credential を provider ごとに分ける** — キーが同じなので 2 つ持つ
意味がない。region ごとに分けるのとは違う (region はキーの有効範囲ではなく
推論先の話で、キー自体は region 非依存)。

**payload に必要なフィールドだけ抜き出す** — 削除に要る ID が落ちる。
「プロバイダの生要素をそのまま」という DR-0003 の原則にも反する。

**`openai.*` を一覧に出して `not_implemented` を返す** — 動かないものを
選択肢に見せることになる。

## Consequences

- credential json の再作成が要る (`type` の値と `payload` の構造が変わる)。
  `login` があるので OAuth 系は再ログインで済む。Bedrock は手書き
- `config.toml` の `type` も追従が要る。`Kind::from_config_type` /
  `config_type` の対応表を更新する
- `url` を config に書かなくてよくなる (`provider` + `region` から組み立てる)。
  上書きが要る場面のために任意フィールドとしては残す
- `provider` と `type` の組み合わせは自由ではない
  (`bedrock_service_specific_credential` × `provider: claude` は無い)。
  型で表現するか実行時に検証するかは実装時に決める
- Phase 2 (OpenAI 変換) に入る前にこの整理を済ませる。後からだと credential の
  再作成がもう一度要る

## 関連

- DR-0002 — OpenAI 変換を Phase 2 に置いた判断。1,758 行の変換が要る
- DR-0003 — payload 分離と `denied_beta`。本 DR はその軸をさらに割る
- `docs/findings/2026-07-28-bedrock-api-key.md` — キーの発行手順
