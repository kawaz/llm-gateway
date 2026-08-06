# llm-gateway 全体設計 — コンポーネント境界・IF・データフロー

> 状態: 現行実装のコード地図。詳細な設計判断は `docs/decisions/` を正本とする。

## 1. 全体像

3 crate 構成。`llm-gateway` がドメインと転送の core、`llm-gateway-server` が
Anthropic Messages API の HTTP ingress、`llm-gateway-cli` が起動・設定確認・閲覧・
ログインを担う。

```
llm-gateway-cli ──────► llm-gateway-server ──────► llm-gateway (core)
  コマンドライン          axum route / ingress          routing / provider 契約と preset /
  起動と表示              namespace 認可                egress / exchange / 観測と永続化
```

core は provider-neutral な契約と横断機構、provider preset の実装を分離する。

```
                              ┌─ config (+ extends)   設定・namespace・検証
                              ├─ router ─ discovery   model → route、catalog、affinity
                              │           pattern
                              ├─ credential/          token 取得・refresh・OAuth・永続化
                              ├─ provider             Auth / Wire / Metering / capability 契約
                              ├─ preset/              契約の provider 別実装と組み合わせ
   gateway ◄─────────────────┤─ egress               正規形 → upstream request → send
   1 リクエストの司会        ├─ denial               route state の型と状態機械
                              ├─ quota                枠の正規形・最新値の永続化
                              ├─ metering ─ stats     token 正規形・単価契約・日次集計
                              ├─ exchange             1 転送の節目・本文観測・終端
                              ├─ events ─ webhook     全 route の転送イベント
                              ├─ session              affinity key の導出
                              └─ persist              共有の原子的書き込み作法
```

### 共有状態

| 所有者 | 状態 | 理由 |
|---|---|---|
| `Gateway` | `Config`, `Router`, `CredentialStore`, HTTP client | リクエスト間で共有する転送基盤 |
| `Router` | provider preset、model catalog、session affinity、event bus | route 選択と provider 間比較の横断状態 |
| `Preset` | `RouteState` (denial、quota、probe schedule) | 状態の意味を読む provider と所有者を一致させる |
| `Gateway` | `QuotaStore`, `Stats`, `Events` | provider を横断して保存・配信する 1 本の器 |

## 2. コンポーネント責務

| モジュール | 責務 |
|---|---|
| `config` / `config::extends` | TOML schema、namespace、routing、起動時検証、設定の継承 |
| `router` | model catalog と alias を解決し、affinity を加味して `Route` を選ぶ。全滅時の 429 も生成する |
| `discovery` | upstream のモデル一覧を取得し、client 名と upstream 名の対応へ正規化する |
| `pattern` | `*` を使う model pattern の照合 |
| `credential/` | credential 取得の窓口、refresh single-flight、プロセス間 lock、OAuth login |
| `provider` | `Auth` / `Wire` / `Metering` と optional な `QuotaApi` / `Negotiation`、それらを束ねる `Preset` の契約 |
| `preset` | provider ごとの契約実装。`anthropic`、Anthropic Wire を再利用する `bedrock`、認証なしの `relay`、単価表を持つ |
| `egress` | Messages 形式の `EgressRequest`、provider-neutral な HTTP 型、encode → authorize → send の出口手順 |
| `denial` | 1 route の availability、denial scope、quota、probe schedule を表す状態機構 |
| `quota` | provider が抽出した枠の正規形、最新 snapshot の保存、利用状況 report |
| `metering` | 拡張可能な token 区分、usage、pricing、本文 observer の core 契約 |
| `stats` | usage を日 × credential × model で集計し、閲覧時に provider の単価で USD 換算する |
| `exchange` | request span、本文受信・upstream header・先頭 chunk・終端を記録し、本文を変えず usage observer へ通す |
| `gateway` | route 選択、credential 取得、交渉、送信、fallback、quota/event 観測、能動 probe の司会 |
| `events` / `webhook` | 全 provider の転送イベントを 1 本の bus へ流し、購読先へ送る |
| `session` | client metadata と header から session affinity key を導出する |
| `persist` (非公開) | tmp → rename、writer 名の sanitization など共通の書き込み作法 |
| `error` (非公開) | core error。HTTP status への変換は server が担う |

### provider preset の構成

`Preset` は必須の `Auth` / `Wire` / `Metering` と optional capability を束ねる。
capability が無いことは空実装でなく `Option` で表す。

| trait | 責務 |
|---|---|
| `Auth` | 直列化済み upstream request に request-time 認証を適用する |
| `Wire` | Messages 正規形を upstream request へ encode し、送信する |
| `Metering` | quota、拒否、本文 usage、model 単価を provider の応答から読む |
| `QuotaApi` | 枠照会 API と最小 probe request を提供する |
| `Negotiation` | upstream が拒否する request header を除去し、失敗応答から学習する |

`preset::from_spec` だけが設定の `type` と preset の対応を知る。router と gateway は
provider の顔ぶれではなく、組み上がった `Preset` の capability を使う。

## 3. リクエスト 1 本のデータフロー

```
POST /ns-x/v1/messages
  [server: ingress]
  ① exchange::request_span で通し番号を作る
  ② 本文を 64 MiB 上限で読み、JSON と header を取り出す
  ③ namespace を解決し、Namespace::authorize で client を認可する

  [core: Gateway::forward]
  ④ egress::model_of → router.resolve。alias は Messages 正規形の model を書き換える
  ⑤ session key と event origin を導出する
  ⑥ router.routes_for が model を扱える route を設定順に並べ、affinity を先頭へ寄せる
  ⑦ router.select が route 自身の availability を聞く
       全 route が denied なら、router が 429 を生成して event bus に発火する
  ⑧ route ごとに try_route:
       credential.acquire
       → Negotiation::prepare
       → egress::send: Wire::encode → Auth::authorize → Wire::send
       → Preset::observe_quota + Events::publish
       → 400 + negotiation blame なら学習して 1 回だけ再送
       → 2xx: affinity を記録し route の denial を解除、応答を採用
         401/403/429/529: provider が denial を読み、次の route へ
         500/502/503/504: route 障害として次の route へ

  [server: response]
  ⑨ provider の Metering が content-type に応じた UsageObserver を作る
  ⑩ exchange::observe が本文を変更せず client へ流し、節目と usage を記録する
```

不変条件:

- **route 切替は client へ 1 byte 書く前まで**。stream 開始後の upstream 断は再試行できない
- **本文は byte stream のまま流す**。方言固有の解釈は provider が作る `UsageObserver` に閉じる
- **core の汎用 module は provider 名を知らない**。`lib.rs` のテストが production code を走査する
- **route 固有状態は route が持つ**。router は `Availability` だけを受け取る

## 4. 拡張点と責務境界

| 拡張点 | 差し込み位置 | 所有者 |
|---|---|---|
| `preset::from_spec` | 起動時の route 構築 | 設定 `type` → preset の組み合わせ |
| `Auth` | Wire encode 後、送信前 | provider の認証方式 |
| `Wire` | egress request の変換と送信 | provider の upstream 方言 |
| `Metering` | response header / body / status の観測 | provider の quota・拒否・usage・pricing |
| `QuotaApi` | 明示 refresh と denial 後の background probe | 枠照会能力を持つ provider |
| `Negotiation` | request header の準備と 400 後の学習 | provider 固有の交渉機能 |
| `Router::select` | route 試行前 | provider 間の候補選択と全滅応答 |
| `exchange::observe` | 採用した response body | provider-neutral な stream lifecycle と集計への受け渡し |
| `Events` | upstream header 受信時 / 全滅時 | 全 provider を束ねる横断 bus |
| `PricingSource` | stats report の生成時 | 集計行を回答 route の単価へ接続する役 |

内部正規形は Messages 形式の `serde_json::Value` で、中立 IR は置かない。
`Wire` が upstream 方言への変換を担い、`egress` が正規形共通の model 読み書きと
HTTP の出口手順を担う。

集計正規形は `TokenKind(String)` と `BTreeMap<TokenKind, u64>` で拡張可能にする。
provider が知らない区分も落とさず保持し、料金は `Pricing::rates` に明示された区分だけを
合計する。親区分と内訳が同居しても、単価表に採らない内訳は二重課金されない。

## 5. 残っている語彙の乱れ

| # | 乱れ | 現状 |
|---|---|---|
| a | `usage` | token usage (`metering` / `stats`) と、利用状況 endpoint / CLI (`usage_report`, `/llm-gateway/usage`) が同じ語を使う |
| b | `scope` | route denial の `Scope` と OAuth / upstream payload の scope が別概念として共存する |
| c | `denied` / `denial` | `denied_beta` は credential に永続化する header 学習、`Denial` は route の一時状態 |
| d | report 型 | `quota::Report` と `stats::Report` があり、CLI では alias が必要 |
| e | `probe` | quota API の非消費照会、最小 request を使う消費 probe、締め出し中の background probe を指す |
| f | 保存動詞 | `Stats::flush`、`QuotaStore::save`、`Gateway::save` が混在する |
| g | `models` | 設定 pattern、宣言 model、公開 catalog、`discovery::Model` を同じ語で呼ぶ |

## 6. 残っている設計と実装の乖離

| # | 乖離 | 影響 |
|---|---|---|
| 1 | DR-0004 の `provider` / `type` 軸は設定 schema へ未反映で、`preset::from_spec` が `type` から preset 全体を選ぶ | OpenAI preset 導入時の設定表現が未確定 |
| 2 | `StoredCredential` の `priority` / `disabled` / `excluded_models` は保存・再ログインで維持されるが routing に使われない | 人が編集できる運用 metadata と実行時挙動が一致しない |
| 3 | `auth_token` 未設定は fail-open だが、`dist/config.example.toml` と `config.rs` の冒頭説明が全拒否と記す | 設定例が実装と DR-0006 に反する |
| 4 | namespace routing が参照する credential の存在確認が起動時 validation に無い | 誤記が runtime の model 解決失敗として現れる |
| 5 | 「宣言順」は `BTreeMap` の名前順であり、ファイル上の記述順ではない | 文書上の語と実際の優先順がずれる |
| 6 | 明示 refresh の最小 request が消費した token は quota report の `probe` には出るが日次 stats へ積まれない | 総使用量と probe 報告が別集計になる |

## 7. 今後

### Phase 2 — Bedrock を試金石にした完了確認

- Anthropic `Wire` を書き直さず、Bedrock が認証差分だけで再利用できていることを
  integration test と実運用経路で確認する
- SigV4 が署名した bytes と実送信 bytes が同一であること、provider-neutral core の
  名前漏れ検査が十分な範囲を覆うことを確認する
- P1 で移した route state、quota、exchange、metering の lifecycle を Bedrock 経路で
  end-to-end に確認する

### Phase 3 — OpenAI native provider

- ChatGPT OAuth の login / refresh lifecycle を `Auth` と credential 層へ追加する
- Messages → Responses request 変換と Responses SSE → Messages SSE 変換を `Wire` に実装する
- OpenAI usage / rejection / pricing を `Metering` の正規形へ写す
- 枠照会 API の実機応答を確認し、`QuotaApi` の有無と設定 `provider` / `type` 制約を確定する

### Phase 4 — 運用完成

- 残存する §5 の語彙と §6 の乖離を解消する
- provider 追加時に contract test、preset test、server integration test のどこへ何を書くかを固定する
- 日次 stats、quota snapshot、event stream、background probe の運用監視と障害時の切り分けを整備する
