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

## 追記: 東京の fable-5 が mantle に出ない件の切り分け結果 (2026-07-30)

アカウント側の前提はすべて満たしても、**region の mantle が当該モデルを
ホストしていなければ一覧にも invoke にも出ない**。

- agreement: `get-foundation-model-availability` で AVAILABLE / AUTHORIZED (東京)
- data retention: `provider_data_share` 設定済み (アカウント単位、全 region 共通)
- それでも東京 mantle は opus-4-7 / 4-8 / haiku-4-5 のみ提供
- 差分は inference profile: 東京で mantle に出るモデルには `jp.` プロファイルがある
  (`jp.anthropic.claude-opus-4-8` 等)。fable-5 / opus-5 / sonnet-5 は `global.` のみで
  `jp.` が無い = mantle 未対応。`global.` 名での invoke も 404
- つまり AWS 側のリージョン別ロールアウト待ち。`jp.anthropic.claude-fable-5` が
  `list-inference-profiles --region ap-northeast-1` に現れたら使えるようになる見込み

確認コマンド (zunsystem アカウント、profile は kawaz@zunsystem):

```bash
AWS_PROFILE=kawaz@zunsystem aws bedrock list-inference-profiles --region ap-northeast-1 \
  | jq -r '.inferenceProfileSummaries[].inferenceProfileId' | grep fable
```
