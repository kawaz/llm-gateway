# Bedrock の region 追加手順 (mantle 経由で claude 系を使えるようにする)

llm-gateway の `claude_bedrock` credential が指す mantle エンドポイント
(`https://bedrock-mantle.<region>.api.aws/anthropic`) で Anthropic モデルを
使えるようにするまでの手順。**API key とは別に、region ごとに model agreement
が必要**。

## 1. AWS 側: model agreement の作成 (region ごと)

```bash
export AWS_REGION=ap-northeast-1  # 対象 region を明示する (default region に効く事故を防ぐ)

# anthropic 使うために必要
models=(
  anthropic.claude-haiku-4-5
  anthropic.claude-sonnet-5
  anthropic.claude-opus-4-7
  anthropic.claude-opus-4-8
  anthropic.claude-opus-5
  anthropic.claude-fable-5
)
for model in "${models[@]}"; do
  j=$(aws bedrock get-foundation-model-availability --model-id "$model" | jq . -c)
  jq . -c <<< "$j"
  if [[ $(jq -r .agreementAvailability.status <<< "$j") == NOT_AVAILABLE ]]; then
    offerToken=$(aws bedrock list-foundation-model-agreement-offers --model-id "$model" | jq -r '.offers[0].offerToken')
    aws bedrock create-foundation-model-agreement --model-id "$model" --offer-token "$offerToken"
  fi
done

# fable 使うために必要 (data retention の provider 共有)
aws bedrock put-account-data-retention --mode provider_data_share
```

## 2. 反映の確認 (mantle 側)

agreement 作成から mantle の一覧・invoke に反映されるまでラグがありうる。
確認は key を画面に出さず shell 変数経由で:

```bash
KEY=$(jq -r '.payload.api_key' ~/.local/state/llm-gateway/credentials/bedrock.json)

# 一覧に出るか (discovery が見るのはこれ。/anthropic の隣の /v1/models)
curl -s https://bedrock-mantle.<region>.api.aws/v1/models \
  -H "Authorization: Bearer $KEY" | jq -r '.data[] | select(.id | contains("claude")) | .id'

# invoke できるか
curl -s https://bedrock-mantle.<region>.api.aws/anthropic/v1/messages \
  -H "x-api-key: $KEY" -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' \
  -d '{"model":"anthropic.claude-fable-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}' | head -c 200
```

## 3. llm-gateway 側

- credential は API key の複製で足りる (region ごとの `[credentials.bedrock-*]`
  を config に並べ、credential json を `cp` で複製)
- routing は近い region を先に置く。fallback (429/529/経路断) で次の region →
  OAuth へ流れる
- discovery は 1 時間ごとに一覧を取り直す。**一覧に出ないモデルは経路に
  入らない** (invoke だけ先に通るようになっても、一覧に出るまで gateway は
  使わない)。急ぐ場合はプロセス再起動で即時 refresh

## 知見 (2026-07-29 実測)

- mantle の一覧・invoke とも `Content-Type: application/json` が必須 (無いと 400)
- 一覧は `Authorization: Bearer <key>`、invoke は `x-api-key: <key>` のどちらでも通る
  (gateway は一覧に Bearer、invoke に x-api-key を使う)
- 東京 (ap-northeast-1) は agreement 作成直後の時点で一覧に
  `anthropic.claude-{haiku-4-5,opus-4-7,opus-4-8}` のみ。fable-5 / opus-5 /
  sonnet-5 は一覧・invoke (prefix `apac.`/`global.`/`jp.` を含む) とも未反映
  だった。反映ラグか region 指定漏れかは切り分け中
