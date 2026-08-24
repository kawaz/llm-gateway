# DR-0021: upstream の公式状態と実測状態を `/llm-gateway/status` で一括表示する

- Status: Proposed
- Date: 2026-08-24

## 背景

LLM が使えなくなったとき、現在は利用者が次の情報を別々に集めている。

- `llm-gateway usage` または ccmsg のクオータ画面で、credential の枠や締め出しを確認する
- gateway のログや events で、どの route が何を返したか確認する
- Anthropic / OpenAI / AWS の status page を個別に開く
- Caddy、unstable daemon、stable daemon のどこで止まったかを切り分ける

2026-08-24 の Anthropic 障害では、3 本の Claude OAuth route がすべて
`api.anthropic.com` から 529 を受け、gateway は最後の 529 をそのまま返した。
Claude Code が表示した `check your inference gateway` は custom base URL の host を
案内文へ埋めただけで、gateway が原因だと判定したものではなかった。この切り分けは、
gateway がすでに持っている実測と provider の公式状態を一緒に見られれば即座に済む。

`/llm-gateway/healthz` は gateway daemon 自身の liveness であり、upstream へは
一切触らない。Caddy がこれを 5 秒ごとに使っているため、upstream の状態を混ぜては
ならない。`/llm-gateway/usage` は credential の利用枠を答える口で、サービス障害とは
出所も寿命も異なる。そこで第三の口を設ける。

## 決定

### 1. HTTP を正とし、`GET /llm-gateway/status` を追加する

gateway が upstream service の状態を集約し、次の endpoint で返す。

```text
GET /llm-gateway/status
GET /llm-gateway/status?refresh=true
```

CLI は `llm-gateway status [--refresh] [--config <path>]` とし、usage と同様に
起動中の daemon へ HTTP で問い合わせる薄い formatter にする。`healthz` との意味は
次のように固定する。

| endpoint | 答える問い | upstream access |
|---|---|---|
| `/llm-gateway/healthz` | この daemon は要求を受けられるか | しない |
| `/llm-gateway/status` | configured upstream は現在使えそうか | cache refresh 時のみ |
| `/llm-gateway/usage` | credential の枠はどれだけ残っているか | `refresh=true` 時のみ |

status source の一部が読めなくても report 自体は `200 OK` で返す。個別 source の
`state = "unknown"` と `error` で部分失敗を表す。status page の停止によって
gateway 自身や Caddy の health check が失敗する構図を作らない。

認証は healthz / usage と同じく掛けない。token、organization id、upstream の
response body は出さず、tailnet / reverse proxy の境界を信頼する。

### 2. 「公式状態」と「gateway の実測」を別の signal として返す

公式 status page は provider 全体の集約情報であり、特定 account、model、region の
実態と一致するとは限らない。反対に gateway の実測はこの環境には正確だが、provider
全体の障害とは限らない。どちらか一方を真実として上書きせず、同じ service の中へ
別々に載せる。

```json
{
  "schema_version": 1,
  "generated_at": 1787529600,
  "overall": {
    "severity": "critical",
    "service_counts": {
      "ok": 1,
      "warning": 0,
      "critical": 1,
      "unknown": 1
    }
  },
  "services": [
    {
      "id": "anthropic",
      "name": "Anthropic",
      "severity": "critical",
      "routes": ["claude-kawazzz", "claude-zunsystem", "claude-emrd"],
      "official": {
        "state": "major_outage",
        "source": "statuspage_v2",
        "source_url": "https://status.claude.com/",
        "observed_at": 1787529580,
        "stale": false,
        "components": [
          {
            "id": "b13yz5g2cw10",
            "name": "Claude API (api.anthropic.com)",
            "state": "partial_outage"
          }
        ],
        "incidents": [
          {
            "id": "incident-id",
            "name": "Elevated errors on the Claude API",
            "state": "investigating",
            "impact": "major",
            "created_at": "2026-08-24T00:00:00Z",
            "updated_at": "2026-08-24T00:10:00Z",
            "url": "https://status.claude.com/incidents/incident-id",
            "latest_update": "We are investigating elevated error rates."
          }
        ]
      },
      "observed": {
        "state": "failing",
        "observed_at": 1787529572,
        "expires_at": 1787529872,
        "last_success_at": 1787529000,
        "last_failure": {
          "at": 1787529572,
          "kind": "upstream_http",
          "status": 529
        }
      }
    }
  ]
}
```

JSON の語彙は DR-0008 に従い英語にする。既存の field は変更せず、将来の追加 field は
optional とする。

#### `official.state`

正規化後の語彙は次の 6 個に固定する。

- `operational`
- `degraded`
- `partial_outage`
- `major_outage`
- `maintenance`
- `unknown`

adapter が受け取った生の語彙を、このいずれかへ写像する。読めない値は推測せず
`unknown` にする。source の取得に失敗しても最後の成功値は残し、取得から
`stale_after` を超えたら `stale = true` にする。成功値が一度も無ければ
`state = "unknown"` と `error` を返す。

#### `observed.state`

gateway が実通信から判断する語彙は、公式状態と混ざらないよう次の 3 個に絞る。

- `reachable`: 対象 route の upstream が直近に採用可能な応答を返した
- `failing`: upstream HTTP 529、transport error、または `ResponseAdmission` が
  busy と判定した応答を直近に観測し、それより後の成功が無い
- `unknown`: まだ観測が無い、または観測の有効期限を過ぎた

401 / 403 / 429 は credential や quota の問題であり、service health へは使わない。
それらは DR-0020 の `usage.denials` / `events.skipped` が答える。一般の 5xx は relay や
変換層で合成されたものを upstream 障害と誤認しうるため、v1 では自動判定へ含めない。
adapter が `upstream_http` と確定できる 529、transport error、provider 固有の
`ResponseAdmission::Busy` だけを対象にする。

実測状態は履歴集計ではなく、route ごとの「最後の成功」と「最後の health failure」だけを
メモリに持つ。`observation_ttl` の既定は 5 分。再起動後は `unknown` へ戻し、永続化しない。
古い障害印を再起動後に現在の状態として復元する方が危険だからである。

### 3. service の severity は表示用の保守的な合成値とする

UI が icon を 1 個選べるよう、各 service と top level に
`ok | warning | critical | unknown` の `severity` を置く。ただし根拠は必ず
`official` と `observed` に残し、利用者が合成値だけを信じなくてもよい形にする。

| 条件 | severity |
|---|---|
| `observed = failing` | `critical` |
| official が `major_outage` | `critical` |
| official が `degraded` / `partial_outage` / `maintenance` | `warning` |
| official が `operational`、または observed が `reachable` | `ok` |
| どちらからも判断できない | `unknown` |

優先度は `critical > warning > ok > unknown`。したがって公式が operational と言っていても、
この環境で 529 が続いた直後は critical になる。逆に公式情報を取得できなくても、直近の
実通信が成功していれば `ok` にできる。top level は configured service の最大 severity とし、
`unknown` は既知の `ok` を警告へ引き上げない。内訳は `service_counts` で失わない。

### 4. source と route の対応は設定へ明示する

service health は Auth / Wire / Metering / QuotaApi のどれでもない。特に同じ Anthropic Wire
でも `api.anthropic.com` と Bedrock は別の status source を持つ。DR-0014 の provider preset
へ押し込まず、独立した `StatusSource` adapter とする。

```toml
[status]
refresh_interval = "60s"
stale_after = "5m"
observation_ttl = "5m"
failure_refresh_cooldown = "30s"
request_timeout = "5s"

[status.sources.anthropic]
type = "statuspage_v2"
summary_url = "https://status.claude.com/api/v2/summary.json"
incidents_url = "https://status.claude.com/api/v2/incidents/unresolved.json"
page_url = "https://status.claude.com/"
components = ["Claude API (api.anthropic.com)"]

[status.sources.openai]
type = "statuspage_v2"
summary_url = "https://status.openai.com/api/v2/summary.json"
incidents_url = "https://status.openai.com/api/v2/incidents.json"
page_url = "https://status.openai.com/"
components = ["Codex API"]

[status.sources.aws]
type = "link"
page_url = "https://health.aws.amazon.com/health/status"

[routes.claude-kawazzz]
provider = "anthropic"
credential = "claude-kawazzz"
status_source = "anthropic"

[routes.codex-kawaz]
provider = "openai"
credential = "chatgpt-kawaz"
status_source = "openai"

[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock-mantle.ap-northeast-1.api.aws/anthropic"
status_source = "aws"
```

`status_source` は optional。書かなかった route も report から落とさず、route 自身を
service とした `official.state = "unknown"` を返して実測だけを載せる。host 名から
Anthropic / OpenAI / AWS を推測しない。custom relay が同じ方言を話すこと、URL が同じでも
運用主体が違うことがあるためである。

v1 の実 fetch adapter は `statuspage_v2` とし、Anthropic と OpenAI の両方へ使う。
両公式ページとも `/api/v2/summary.json` を公開している。Anthropic は
`/api/v2/incidents/unresolved.json` も公開する一方、OpenAI の同 path は 404 で、
`/api/v2/incidents.json` が全 incident を返す。この差は config の `incidents_url` と
adapter 内の正規化で吸収し、外向き report には漏らさない。全 incident endpoint の場合は
`resolved` / `postmortem` を除いたものだけを unresolved incident として返す。

component filter は page 全体の状態ではなく、実際に使う service へ対応する component の
状態を選ぶ。OpenAI の Codex route は `Codex API`、Anthropic origin は
`Claude API (api.anthropic.com)` を選ぶ。incident に component の対応情報がある場合は
filter と交差するものだけを載せる。対応情報が無い incident は `scope = "page"` として
参考表示できるが、その incident の impact だけで対象 service の severity を引き上げない。
選択 component の状態または gateway の実測を severity の根拠にする。

`link` は外部アクセスせず、`page_url` と `unknown` を返す placeholder とする。AWS Health API は
AWS 認証と対象 support plan を要するため v1 は `link` になる。HTML scraping や route
credential の流用は行わない。将来、正式な取得方法が確定したら source type を追加する。

設定 URL は起動時に検証し、既定では HTTPS のみ許す。redirect は同一 origin の HTTPS のみ、
response body は 1 MiB、incident の `latest_update` は UTF-8 で 4 KiB、全体の request timeout は
既定 5 秒に制限する。外部文字列は UI 側で HTML として解釈しない。

### 5. refresh は single-flight の cache 更新にする

gateway 起動時に各 fetchable source の初回 refresh を background で開始し、その後は
`refresh_interval` ごとに更新する。通常の `GET /status` は現在の memory snapshot を即座に返す。

`?refresh=true` は refresh 完了を `request_timeout` まで待ってから report を返す。同じ source への
同時 refresh は 1 本へ畳み、待つ側は同じ結果を受け取る。取得失敗時は最後の成功 snapshot を
消さない。

upstream HTTP 529 または `ResponseAdmission::Busy` を観測したときは、その route の source を
background refresh する。元の LLM request は status page の応答を待たず、従来どおり次 route へ
fallback する。source ごとに `failure_refresh_cooldown` を置き、529 が多数同時発生しても
status page へ集中させない。transport error は status page 自体へ到達できない可能性も高いため、
同じ refresh trigger に含めるが同じ cooldown を使う。

### 6. CLI は原因の出所を一目で区別して表示する

例:

```text
SERVICE     STATUS    OFFICIAL         OBSERVED   UPDATED
Anthropic   CRITICAL  major outage     failing    18s ago
OpenAI      OK        unknown          reachable  42s ago
AWS         UNKNOWN   unknown          unknown    -

Anthropic: Elevated errors on the Claude API
  https://status.claude.com/incidents/incident-id
```

`--refresh` なしは cache を表示し、stale な official state には age と `stale` を付ける。
終了 code は HTTP report を読めれば 0 とし、upstream outage を command execution failure には
しない。automation が severity で判定したい場合は JSON endpoint を使う。

## ccmsg への組み込み契約

llm-gateway 側の API が入った後、ccmsg には次を依頼する。

1. daemon config に `llm_status_url` を追加する。`llm_usage_url` / `llm_stats_url` と同様に、
   browser から gateway を直接 fetch せず daemon が bounded fetch する
2. protocol に user-role 限定の `llm_status` request と二相の `llm_status_result` を追加する。
   request は `{ op, request_id, refresh? }`、result は既存 usage と同じ
   accepted → completed/error の形にする
3. hello に `llm_status_available` を追加する。WebUI は status / usage / stats のどれか 1 つでも
   available なら既存の「クオータ / 使用量」入口を表示する
4. WebUI は接続中、表示中の page に関係なく cached status を 60 秒ごとに取得する。
   `/usage` 画面では quota / stats tab の上へ service status strip を常設し、incident を展開できる
5. global header は `warning` で黄、`critical` で赤の icon badge を出し、押すと
   `/usage#service-status` へ移動する。`ok` は badge なし。`unknown` は global warning にせず、
   status strip 内だけで灰色表示する
6. 既存 `/webhook/llm-gateway` の request event で 529 を受けたら、5 秒 debounce / single-flight で
   `llm_status_url` を即取得する。gateway 自身も同時に公式 source を refresh するため、completed
   report がまだ古ければ次の通常 poll で追いつく。webhook schema に status snapshot は足さない
7. `latest_update` 等の外部文字列は text node として描画し、HTML を展開しない。未知の field と
   未知の state は `unknown` として扱い、protocol の forward compatibility を保つ

ccmsg は API report を再判定せず、`severity` を icon 選択に使う。詳細画面では必ず
`official` と `observed` を分けて表示し、「provider 発表」と「この gateway の実測」の違いを
保つ。

## 実装スコープ

- `config.rs`: `StatusConfig` / `StatusSourceSpec` と `RouteSpec.status_source`、URL・参照整合性の検証
- 新規 `status.rs`: adapter trait、cache、single-flight refresh、正規 report、severity 合成
- `statuspage_v2.rs`: summary JSON の bounded parser と component filter
- `gateway.rs`: route attempt の成功・529・transport error・admission busy を observer へ通知
- `llm-gateway-server`: `/llm-gateway/status` と `refresh=true`
- `llm-gateway-cli`: `status` subcommand と人間向け formatter

最低限の test は次を含む。

- source が成功 / timeout / malformed / oversized / unknown state の各場合
- component filter が page 全体の別障害を対象 service へ誤適用しないこと
- stale snapshot を失敗 refresh で消さないこと
- 同時 refresh と大量 529 が source ごとに 1 request へ畳まれること
- 529 の LLM response が status refresh を待たずに返ること
- 401 / 403 / 429 / 合成 5xx が observed health を failing にしないこと
- 後続 2xx が failing を reachable へ戻し、TTL 後は unknown になること
- 一部 source が失敗しても endpoint が 200 で全 service を返すこと
- source 未指定 route も report に残ること
- CLI の operational / incident / stale / unknown 表示

## 採らなかった案

- **`healthz` で upstream まで見る**: Caddy が upstream 障害を daemon 障害と誤認し、stable daemon
  へ切り替えて同じ origin へ再送する。liveness と dependency health を分ける
- **usage response に status を混ぜる**: quota と service health は更新契機、source、寿命が違う。
  status だけ使いたい ccmsg も usage probe の意味論に巻き込まれる
- **公式 status だけ返す**: account / model / region 固有障害と、公式発表より早い 529 を見逃す
- **実測だけ返す**: 障害範囲と provider の復旧告知が分からず、traffic が無い間は永遠に unknown
- **AWS の status page HTML を scrape する**: 非公開 DOM への依存を公開 API 契約へ持ち込む。
  AWS Health API を安全に利用できる条件が揃うまでは `link` + 実測で扱う。OpenAI は
  公式 JSON endpoint が確認できたため `statuspage_v2` で扱う
- **529 request の中で status page を待つ**: すでに失敗している request の latency と failure mode を
  増やす。refresh は background に限定する
- **status snapshot を webhook event に同梱する**: request event の 1 件ごとに大きな重複を載せ、
  DR-0012 の軽い恒常 event を壊す。event は refresh の hint、正本は HTTP とする
- **route の provider や URL から status source を推測する**: Bedrock と Anthropic origin、custom
  relay を区別できない。対応は operator が明示する

## 関連

- [DR-0007](./DR-0007-usage-visibility.md) — HTTP を正、CLI を formatter とする先例
- [DR-0009](./DR-0009-credential-denial-fallback.md) — 529 fallback と最後の denial 透過
- [DR-0012](./DR-0012-request-events.md) — ccmsg へ届く request event
- [DR-0014](./DR-0014-target-architecture-provider-preset.md) — provider capability の境界
- [DR-0020](./DR-0020-denial-reason-visibility.md) — quota / route denial の可視化
