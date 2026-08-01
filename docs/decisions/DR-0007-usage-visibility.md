# DR-0007: 全 credential の利用量を一括で見えるようにする

- Status: Active
- Date: 2026-07-29

## Context

kawaz の要望:

> 各アカウントの 5h と 7d のコンテキスト利用料とリセット日時などを一括で確認できるものが
> 欲しいですね。今どのアカウントのクレジットがどれくらい残ってるのかとか確認できないのが不便。

調査 (2026-07-29 実測) で、必要な情報は upstream の応答から取れることが分かった。

### Anthropic (Claude OAuth)

`/v1/messages` の応答ヘッダに `anthropic-ratelimit-unified-*` が乗る (実測値):

| ヘッダ | 意味 | 実測例 |
|---|---|---|
| `unified-5h-utilization` | 5 時間ウィンドウの使用率 (0.0〜1.0) | 0.71 |
| `unified-5h-reset` | リセット時刻 (Unix 秒)。実時刻の 5 時間境界に丸まる | 05:00 JST |
| `unified-7d-utilization` / `-reset` | 7 日ウィンドウの同上 | 0.3 / 8/2 18:00 |
| `unified-{5h,7d}-status` | `allowed` など上限到達フラグ | allowed |
| `unified-overage-disabled-reason` | 従量課金フォールバックが塞がれた理由 | out_of_credits |

**公式ドキュメントに記載が無い** (documented なのは従量 API 向けの
`requests-*` / `tokens-*` のみ)。実測が唯一の根拠なので、予告なく変わる前提で扱う。
クレジット残の**数値**は取れない (overage の可否という 2 値だけ)。

ヘッダを得るには実リクエストが要る (haiku + max_tokens=1 で input 8 / output 1
トークン)。

#### 副作用なしの専用口 (実測 2026-08-01)

`GET https://api.anthropic.com/api/oauth/usage` が、**トークンを使わずに**枠を返す
(`authorization: Bearer <OAuth token>` / `anthropic-beta: oauth-2025-04-20` /
`anthropic-version: 2023-06-01`)。ヘッダより広く、モデル別の枠まで載る:

```json
{"limits": [
  {"kind": "session",       "percent": 0,   "severity": "normal",   "resets_at": null, "scope": null, "is_active": false},
  {"kind": "weekly_all",    "percent": 100, "severity": "critical", "resets_at": "...", "scope": null, "is_active": true},
  {"kind": "weekly_scoped", "percent": 80,  "severity": "warning",  "resets_at": "...",
   "scope": {"model": {"id": null, "display_name": "Fable"}}, "is_active": false}]}
```

`limits` を正本にする。`five_hour` / `seven_day` / `seven_day_opus` のような欄も
並ぶが、中身が `null` の欄が多く、どの枠がどのモデルに掛かるかを持たない。

この口も公開ドキュメントに無い。**読めない応答は「情報なし」に落とし**、
利用状況の一覧から欄ごと省く (推測で埋めない)。

##### 語の意味は、素直に読むと間違う

同時刻に 3 つの credential を突き合わせた実測 (2026-08-01):

| credential | session | weekly_all | weekly_scoped (Fable) | haiku | fable / opus / sonnet |
|---|---|---|---|---|---|
| A | 0 % | **100 % critical / active** | 80 % warning | **200** | 429 |
| B | 3 % | 35 % normal | **47 % normal / active** | 200 | 429 |
| C | 0 % | **100 % critical / active** | 57 % normal | 200 | 429 |

読み取れること:

- **`is_active` は「塞がっている」ではない。** B は 47 %・`severity: normal` の枠が
  `is_active: true`。応答ヘッダの `representative-claim` (今どの窓を見ているか) に
  近い意味と考えられる
- **`weekly_all` が 100 % でも credential は死んでいない。** A / C はその状態で
  haiku に 200 を返す。5 時間 / 7 日の枠を使い切った後も、安い側は通る
  (ヘッダの `fallback-percentage: 0.5` がこの段に対応するとみられる)
- **どのモデルが断られるかは、この口からは分からない。** B は全部の枠が
  `normal` のまま fable / opus / sonnet に 429 を返す
- **同じ「7 日」でも数字が一致しない。** A のヘッダは同時刻に
  `7d-utilization: 0.34 / status: allowed`、この口は `seven_day: 100.0`。
  別の物差しなので、片方をもう片方の代わりにはできない

したがってこの口は**利用状況を見せるため**に使い、**経路を締め出す判断には
使わない** (DR-0009 の締め出しは、実際に断られた応答だけを根拠にする)。

### Codex (ChatGPT OAuth)

2 経路ある (Codex CLI のソースで確認)。

- 応答ヘッダ `x-codex-{primary,secondary}-{used-percent,reset-at}` — Anthropic の 5h/7d に
  対応する 2 段窓。窓の長さ自体もサーバが返す (`window_minutes`)
- `GET https://chatgpt.com/backend-api/wham/usage` — **副作用なしの専用口**。
  クレジット残の数値 (`x-codex-credits-balance` 相当) やプラン種別まで取れる

### Bedrock

API key (推論実行権限) では使用量を取れない。使用量系は別の IAM アクションで、
Anthropic 側 Admin API の対象でもない (AWS Marketplace 課金)。**対象外と割り切り、
表示では「対象外 (AWS 課金)」と明示する**。

### Relay

転送先の gateway が同じ口を持てば再帰集約できる余地があるが、当面は対象外。

## Decision

**HTTP を正とし、CLI は薄いフォーマッタにする。**

- `GET /llm-gateway/usage` — credential ごとの利用状況を JSON で返す
  (名前・種別・5h/7d の使用率とリセット時刻・上限フラグ・overage 可否・**取得時刻**)
- `?refresh=true` のときは専用の口にも聞き、`limits` をそのまま `limits` 欄に
  載せる。語は upstream のもの (`weekly_scoped` / `percent` / `resets_at`) を
  そのまま使う — 言い換えると、実物と照らす人が対応表を覚えることになる。
  聞けなかった credential では**欄ごと出さない** (空配列と区別する)
- CLI はモデル別の枠 (`scope.model` を持つもの) を表の下に 1 行で出す。
  5 時間 / 7 日の欄には収まらず、ヘッダにも出てこないので、ここに出さないと
  利用者から見えない
- `llm-gateway usage` — 上を叩いて人間向けに整形する CLI サブコマンド。
  server が起きていなければその旨を出す (CLI 単独ではスナップショットを持たない)

### 取得は「便乗」を基本にする

gateway は全リクエストを仲介しているので、**応答ヘッダを通りすがりに読んで
credential ID ごとの最新スナップショットを持つ**。追加の API コールはゼロ。

弱点は、しばらく使われていない credential の情報が古い/無いこと。そこで:

- スナップショットには必ず**取得時刻**を付け、表示側で古さが見えるようにする
- `?refresh=true` (CLI は `--refresh`) のときだけ**能動プローブ**する。
  Codex は副作用なしの `wham/usage`、Anthropic は haiku + max_tokens=1 の最小リクエスト。
  プローブの消費量は JSON にだけ載せる。CLI には出さない — 表示して得るものが無い
  (kawaz 裁定 2026-07-29。window の status 注記・overage 注記も同じ理由で CLI に出さない)

既定を便乗のみにするのは、usage 計測が usage を勝手に消費する構図を避けるため。

### 認証は掛けない (healthz と同じ扱い)

出すのは使用率・リセット時刻・フラグだけで、credential の中身・token・
organization id は出さない。tailnet 内からしか到達できない前提で、
利用率の露出は許容する。問題が出たら締める。

### スナップショットを永続化する (2026-07-31 改訂)

当初はスナップショットをメモリだけに置いた。ディスクに残すと、消えない古い値を
最新だと誤解する危険があると見たため。

実際に運用すると、こちらの弊害の方が大きかった。gateway を再起動するたびに全
credential が未観測へ戻り、次にその credential を使うまで一覧に何も出ない。
観測は通りすがりでしか起きないので、しばらく使っていない credential ほど戻って
こない。「今どのアカウントがどれくらい残っているか」を見たいのが元の要望なのに、
再起動直後は最も答えられない状態になる。

kawaz 裁定 (2026-07-31):

> 最後に取得した時の値は取れても良い。各エントリに最終更新を付ければ判断に使える。
> 頻繁に永続化する必要はない

誤解の危険は**取得時刻で既に塞がれている**。スナップショットは必ず `observed_at`
を持ち、CLI は 5 分を超えた分に経過を添える。読み戻した値は当時の取得時刻を
そのまま持つので、古ければ古いと表示に出る。値そのものは「最後に観測したときの
実測」であって推測ではない。

- 保存先は日次集計 (DR-0011) と同じ置き場の `usage-latest.<書き手>.json`。
  書き手の名前も一時ファイル経由の書き方も日次集計と共通
- **他の writer のファイルは読まない**。向こうの観測は向こうが持っている
  (日次集計は全 writer を足すが、こちらは「最新の 1 つ」なので足せない)
- 落とすのは日次集計と同じ 60 秒の周回と終了時。観測があったときだけ書く。
  頻度を上げても得るものは無い — 失っても次のリクエストで拾い直せる

## 関連

- [DR-0006](./DR-0006-namespace-routing.md) — `/llm-gateway/` 配下が gateway 自身の機能の置き場
- `docs/findings/` — unified ヘッダの実測記録
