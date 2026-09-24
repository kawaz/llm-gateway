# 時間バケットのレート制限と ns の allowlist (DR-0030 §7 手順 3)

DR-0030 §3 (自主レート制限) と §4 (ns の allowlist) を実装に落とす計画。永続化の契約は DR-0031 §2 (3) `CounterStore`。裁定は末尾「裁定済み」。

## 0. 前提 (現状のコード)

- パススルーは `llm-gateway-server` の受け口 → `rejection()` (ns 認証、`Authorization::Accepted(Principal)` / `Open` / `WrongToken`) → `Gateway::relay` → `Passthrough::relay` → `send()` の中で `UpstreamSpec::decide` (上流側 allow、404 / 405 / UnsafePath) → secret 読み出し → 送信。知らせは `events::Passthrough` 1 件 (`status` / `secret` 等)
- 受け口は `rejection()` の戻りで `Principal` を捨てている (`Accepted(_)`)。下流へ主体を渡す口がまだ無い
- `gateway_core::stats::Stats<C: Mergeable>` は writer 別の日ファイル + メモリ、`keep_saving(every)` の周期 flush、`merged()` が `Merged { value, missing }` を返す。日付は UTC 固定
- tz を扱う crate は workspace に無い

## 1. 適用範囲

候補を並べる。

| 案 | 中身 | 利点 | 欠点 |
|---|---|---|---|
| A. パススルーだけ | `[secrets.<id>]` の宣言に対してだけ数える | DR-0018 / 0019 との二重制御が起きない。段が小さい。枠ヘッダを返さない上流 (DR-0030 §3 の動機) はほぼパススルー側 | xAI のような「LLM 経路に乗るが枠ヘッダが弱い」API key credential に効かない |
| B. 両方 (credential 単位で宣言したものだけ) | secret にも LLM credential にも `limits` を書ける。書かれていなければ数えない | DR-0030 §3 の「credential 単位」をそのまま満たす。xAI の API key を LLM 経路で抑えられる | LLM 経路の判定位置が routing の後 (credential 確定後) になり、fallback との相互作用を決める必要がある (§4) |
| C. 両方 + DR-0018 / 0019 を自主バケットで置き換え | — | — | DR-0030 §3 と Consequences が「置き換えず共存」と決めている。対象外 |

**裁定: A で始め、B は後続の段 (3d) として設計だけ揃える (裁定 1)。** カウンタと判定の部品は credential の種類を知らない形 (鍵は `CredentialId`) で core に置くので、B は「LLM 経路の credential 確定点で同じ部品を呼ぶ」だけで足せる。

DR-0018 / 0019 との整理 (B を採る場合):

- DR-0018 / 0019 は **上流の枠ヘッダから導いた「この経路を候補に入れるか」** (routing の選択)。自主バケットは **gateway が数えた回数で「この credential で送ってよいか」** (送信の可否)。役割は重ならないが、同じ credential に両方が掛かると「pace_cap で候補から外れた」と「自主バケットで満杯」がどちらも経路を塞ぐ
- 推し: LLM 経路では自主バケットの満杯を **denial と同じ「候補から外す」扱い** にし、他の経路が残れば回す。全候補が塞がった時だけ 429 を返す。自主バケットだけ即 429 にすると、他 credential が空いているのに止まる (DR-0009 の「次へ回す」に反する)

## 2. 設定の形

### 2.1 バケットは credential 側に書く

DR-0030 §3 は「数える単位は credential」、§5 は「静的 secret はファイルの置き場で、config は id を指すだけ」。枠はキーの持ち主と上流の約束で決まる値なので、書く場所は **config 側の credential 宣言** に置く (secret ファイルの中身は置き場の backend が持つもので、`op://` や cache-warden に差し替えた時に枠の宣言が消えないようにする)。

```toml
[secrets.xai]                      # 固定の秘密の宣言 (中身は [secrets] の置き場の xai.json)
limits = [
  { requests = 60,   per = "minute" },
  { requests = 5000, per = "day", tz = "America/Los_Angeles" },
  { requests = 100000, per = "month", tz = "+09:00" },
]
```

- `per`: `"minute" | "hour" | "day" | "month"`。`"10 minutes"` のような倍数は第一版で持たない (固定窓の境界を UTC 整数分 / 整数時に揃える DR-0030 §3 の規定と、倍数の起点の定義が要るため)
- `tz`: IANA 名か固定 offset (`±HH:MM`)。省略時 UTC。`minute` / `hour` に書いたら設定エラー (DR-0030 §3 で UTC 整数分 / 時に揃える)。ただし 30 分 / 45 分 offset の tz で「時」の境界を現地にしたい需要は未確認 (§7)
- 現状 `[secrets]` は置き場の設定 (`type = "file"`) を持つ表なので、`[secrets.<id>]` と同居させると `type` と id の名前空間が衝突する。案: (a) 置き場を `[secrets.store]` / `[store]` へ移す、(b) 宣言を `[secret.<id>]` (単数) で分ける、(c) `[upstreams.<name>].limits` に書く。**(c) は単位が行き先になり、同じ secret を 2 つの upstream で共有すると枠が割れるので DR-0030 §3 に反する。裁定: 置き場の設定を `[secret_store]` へ改名し、`[secrets.<id>]` を宣言に使う** (末尾の裁定 2)
- LLM credential (案 B): 同じ形の `limits` を credential の設定側 (route / credential 宣言) に書く。credential ファイル (OAuth token の JSON) には書かない (refresh で書き戻されるファイルに運用設定を混ぜない)

### 2.2 ns の allowlist

```toml
[ns.app1.allow]
"api.x.ai" = ["GET /v1/models", "POST /v1/chat/completions"]
"api.example.com" = ["GET /v1/*"]
llm = ["*"]                          # LLM 経路 (段 4 で扱う。§4)
```

- キーは `[upstreams.<name>]` の名前 (既定は apifqdn、ラベル可)。値は段 3 と同じ `"METHOD path-pattern"` の `Allow` を流用し、パースとエラー文言を共有する
- 照合は `pattern::matches` の glob (DR-0030 §4 の「パス prefix」は `/v1/*` で表す)。メソッドに `*` は許さない (裁定 4。副作用のある method を明示させるのが §4 の目的)
- `allow` を書かない ns は **何も通さない** (裁定 3)。ただし既存の LLM 経路 (`/ns-<ns>/v1/...`) の挙動は変えない (DR-0030 Consequences) ので、第一版の allowlist はパススルーにだけ効かせ、LLM 経路は `llm` キーを段 4 で導入するまで対象外。これは既存 ns が段 3 の本番投入で突然パススルーを使えなくなる変化を伴う (段 3 のパススルーは現状 ns 側の制限なしで通る) が、稼働設定にパススルーを使う ns はまだ無いので切替手順は要らない

### 2.3 上流側 `allow` と ns 側 `allow` の責務

| | `[upstreams.<name>].allow` | `[ns.<name>.allow]` |
|---|---|---|
| 問い | その API のどの endpoint を gateway 経由で使ってよいか | この利用者 (ns) がどの行き先の何を使ってよいか |
| 決める人の視点 | キーの持ち主 (そのキーで叩かせてよい範囲) | ns を配る人 (その token を持つアプリの権限) |
| 変わる理由 | 上流 API の形・キーの権限 | アプリの追加・権限の付け外し |
| 判定 | 両方を満たした時だけ通す (積集合)。ns 側は上流側を広げられない |

ns 側に書いた endpoint が上流側に無い場合は設定エラーにせず警告に留める案を推す (上流側を絞った時に全 ns の設定を同時に直させない)。

## 3. カウンタの実装

### 3.1 置き場

- `minute` / `hour`: メモリの固定窓カウンタ (`(bucket_start, count)`)。再起動で消える (DR-0030 §3)
- `day` / `month`: `Stats<C>` を器に、`C = RequestCount(u64)` (merge = 加算)。鍵 (日付文字列) を **バケットの現地日付 / 月** にする。`Stats` は今 UTC 日付固定で日ファイルを切るので、鍵を外から渡せる形 (`add_at(key, ...)`) に広げるか、レート制限用に `CounterStore` を実装した別の器を置く。推しは後者: `Stats` は閲覧用 best-effort の都合 (周期 flush、36,500 日の保持) を持ち、fail-closed の用途と契約が逆なので同じ型に 2 つの倒し方を持たせない。file backend のファイル形式 (writer 別 JSON + 原子的 rename) は `persist` を共有する
- 置き場: `$XDG_STATE_HOME/llm-gateway/ratelimit/<credential>/<bucket-key>.<writer>.json` の形を案とする

### 3.2 fail-closed の実現

DR-0031 の表のとおり `write_own` / `read_merged` とも fail-closed。1 request の判定は:

1. `read_merged(bucket)` → `missing` が空でなければ通さない
2. 合算値 + 自分のメモリ上の未書き込み分 ≥ limit なら 429
3. `write_own` で自分の累計 +1 を**書いてから**送る。書けなければ通さない

「送ってから数える」だと失敗しても上流の枠は減っているので、**受理時点で数える** (上流が 4xx / 5xx を返しても 1 と数える。上流の数え方と合うかは API 次第で未確認)。

毎 request の書き込みは `keep_saving` の「1 リクエストごとに書くのは無駄」(kawaz 裁定) と衝突する。ただしあれは閲覧用 stats の裁定で、fail-closed の枠には「書き損ねた分だけ枠を超える」(DR-0031) ので周期書きは契約違反になる。緩める案として **予約 (lease) 方式**: 一度に `k` 件分を先に書いて (`write_own(累計 + k)`)、メモリで消化し、使い切ったら次を書く。他 writer から見ると最大 `k × writer 数` だけ早く満杯に見える (安全側)。`k` は `limit` の数 % などで決める。第一版は `k = 1` (毎回書く、裁定 7)。予約方式は後続。

### 3.3 `missing` のときの status

DR-0031 は「通さない」まで。候補:

- **503 + `Retry-After`** (裁定 5): 枠が埋まったのではなく gateway が判定できない。429 にするとクライアントは「枠切れ」と読み、`X-RateLimit-*` とも矛盾する
- 429: クライアントの再試行コードが 429 だけを扱う場合に優しいが、原因の区別が付かない

`missing` になるのは他 writer のファイルが読めない (壊れている / 権限) 時で、通常は運用の異常。events に `ratelimit_unavailable` として出す (§5)。

### 3.4 固定窓の境界

- 分 / 時: `floor(now_utc / 60s)` / `floor(now_utc / 3600s)`。tz は見ない
- 日: バケットの tz で `now` の現地日付を求め、その日の 00:00 現地 → 翌日 00:00 現地を窓とする。DST の日は窓が 23 / 25 時間になる (上限は変えない。「その日」の約束として API 側もそう数えるのが普通、ただし未確認)
- 月: 現地の 1 日 00:00 → 翌月 1 日 00:00。日数の違いは暦計算に任せる
- 存在しない / 重複する現地時刻 (DST の切替点): 境界は 00:00 だが、00:00 が飛ぶ tz (過去に例あり) では「その日の最初の有効な時刻」に寄せる。これは tz crate の解決規則 (jiff なら `compatible`) に従う
- tz の実装に crate が要る: `jiff` を使う (裁定 9。IANA DB を同梱でき、DST の曖昧時刻の規則が明示的)

### 3.5 `Retry-After` と `X-RateLimit-*`

- 超過したバケットのうち **窓の終わりが最も遅いもの** を `Retry-After` (秒、切り上げ) にする。早いものを返すとクライアントは戻ってきてまた断られる
- `X-RateLimit-Limit` / `-Remaining` / `-Reset`: 複数バケットのうち **残量の比率が最も小さい (最も詰まっている) 1 つ** を出す。`-Reset` は形の揺れ (epoch 秒 / 残り秒) がある。**残り秒** にする (裁定 6)
- 通した応答にも付ける。パススルーは「応答を触らない」(DR-0030 §2) が、これは §3 が明示的に足すヘッダ。上流が同名ヘッダ (`X-RateLimit-*` / `Retry-After`) を返した場合、gateway が出す時は上流のものを落とし、出さない時はそのまま流す (裁定 6。DR-0030 §3 の「上流の値を混ぜない」)

## 4. 判定の位置

パススルー (受け口 → `Passthrough::relay`):

1. ns 解決、ns 認証 → `Principal` を得る (受け口で捨てていたのを `Relay` に載せる。`Open` の ns は `Principal { ns, subject: None, kid: None }` 相当)
2. 予約名 / 未登録 upstream → 404
3. **ns の allowlist** → 404 / 405 (上流側 `allow` より先。ns に見せてよくない upstream の存在を ns 側の 404 で隠す)
4. 上流側 `decide` → 404 / 405 / UnsafePath
5. secret 読み出し (502)
6. **レート制限** (§3.2) → 429 / 503
7. 送信

レート制限を secret 読み出しの後に置く理由: secret が読めずに 502 になる request を数えない。allowlist 外の request も数えない (上流に届かないので枠を減らさない)。

LLM 経路 (案 B): routing が候補 credential を並べた後、各候補を試す直前 (denial の締め出し判定と同じ位置) でバケットを見て、満杯なら denial と同じく次候補へ。全候補が満杯または denied なら 429。数えるのは実際に送る credential の分だけ。ns allowlist の `llm` キーは routing の前 (ns 認証直後) に置く。

## 5. events / stats

- `events::Passthrough` に `refused: Option<String>` を足す案 (値: `ns_allow` / `upstream_allow` / `rate_limited` / `ratelimit_unavailable` / `secret` / `unreachable`)。status だけでは 404 が ns 側か上流側か、429 が自主か上流かを読めない。上流が返した 429 は `refused = None, status = 429` になり、DR-0030 Consequences「429 の理由が 2 つ」を区別できる
- `rate_limited` には `bucket` (例: `day@America/Los_Angeles`) と `retry_after_secs` を併記
- LLM 経路 (案 B): `request` の知らせにある全候補断りの理由欄 (DR-0014 §8 の denial) に `rate_limited` を理由の 1 つとして加える
- stats: 自主 429 の件数を日次集計に足すかは任意。推しは events だけで始める (stats の軸を増やすと DR-0029 の origin 軸と掛け算になる)

## 6. 段分け

各段で `just ci` が通る。

**段 3a: ns allowlist (パススルーのみ)**
- ゴール: `[ns.<name>.allow]` 外のパススルーを 404 / 405 で断る
- 完了条件: 設定のパースとエラー文言、受け口で `Principal` を `Relay` へ渡す、判定順 (§4 の 3 → 4) の test、`events::Passthrough.refused` の追加
- やらないこと: LLM 経路の allowlist、レート制限
- 3a 実施済み: `[ns.<name>.allow]` (`Allow` を共有、method の `*` は読み込みで断る、書かない ns は閉じる)、受け口で `Principal` を `Relay` へ渡す、判定は ns の allowlist → 行き先の `allow` → 秘密 → (レート制限の位置) → 送信、`events::Passthrough.refused`。あわせて `[secrets] type` を `[secret_store]` に改名 (旧形は理由付きで断る)。断る理由に計画 §5 の語へ `unknown_upstream` / `unsafe_path` を足した。未登録の行き先と ns の allowlist 外は同じ 404 の文言にした (行き先の有無を探らせない)

**段 3b: 固定窓の境界計算と分 / 時のメモリバケット**
- ゴール: credential 宣言の `limits` を読み、分 / 時の超過を 429 + `Retry-After` + `X-RateLimit-*` で返す
- 完了条件: 設定の置き場の整理 (§2.1 (a))、境界計算の単体 test (UTC、DST 切替日、月末、閏年)、tz crate の導入
- やらないこと: 日 / 月、永続化
- 3b 実施済み: `gateway_core::ratelimit` (`Limit` / `Per` / `Zone`、`jiff` で日 / 月の現地境界、`MemoryBuckets`)、`[secrets.<id>] limits` (旧 `[secrets] type` / `dir` は `[secret_store]` へ移ったと断る)、判定は秘密の読み出しと宛先の組み立ての後・送信の直前で秘密の id を鍵に数える、429 + `Retry-After` + `X-RateLimit-*` (残り秒、上流の同名ヘッダは落とす)、知らせの `refused = rate_limited` + `bucket` + `retry_after_secs`。日 / 月の宣言は読み込みで断る (3c まで)。鍵は秘密の id だけで `Principal` は使わない (DR-0030 §3 の数える単位は credential)

**段 3c: 日 / 月バケット (`CounterStore` の fail-closed 実装)**
- ゴール: 再起動と複数 writer を跨いで日 / 月を数え、`missing` で 503 を返す
- 完了条件: `CounterStore` trait を core に切る (DR-0031 §5 の「(3) を切る」はここ)、file backend、2 writer を模した test (片方のファイルを壊す → 503、両方の加算で満杯 → 429)、書き込み失敗で通さない test
- やらないこと: 予約方式 (`k > 1`)
- 3c 実施済み: `gateway_core::counter::{CounterStore, FileCounters, RequestCount}` (writer 別ファイル、置き場は `[ratelimit] dir`、既定 `$XDG_STATE_HOME/llm-gateway/ratelimit/<秘密 (percent-encoding)>.<writer>.json`。中身は窓 (`<per>@<tz>@<始まり>`) ごとの数で、日と月を 1 回の書き込みで揃え、過ぎた窓は書くときに落とす)、`gateway_core::ratelimit::RateLimiter` が分 / 時 (メモリ) と日 / 月 (`CounterStore`) を 1 本の判定にまとめる。`missing` か自分の書き込み失敗で 503 (`Retry-After: 60`、知らせは `ratelimit_unavailable`)。書くのは判定を通った後・送信の前。`CounterStore` には DR-0031 の案の 2 操作に加えて `read_own` を足した (自分の累計を読んで +1 を書くため)

**段 3d (後続、案 B を採る場合): LLM 経路への適用**
- ゴール: LLM credential に `limits` を書け、満杯の候補は次へ回る
- 完了条件: denial と同じ位置の判定、全候補塞がりの 429 の理由表示、`[ns.<name>.allow]` の `llm` キー

## 7. リスク・未確認

- **他 writer の flush 遅延は `missing` にならない (最大のリスク)**: 合算は他 writer のファイルに書かれた値しか見えない。他 writer が周期書き (閲覧用 stats と同じ作り) だと、読めてはいるが古い値で判定し、上限を writer 数 × flush 間隔ぶん超える。fail-closed の契約が守るのは「読めない」までで「古い」は検知できない。§3.2 の「送る前に書く」でこれを塞ぐのが前提で、`Stats` の周期 flush を流用すると再発する。予約方式に進む時も「予約を書いてから消化する」順を崩さない
- **同時到着の競合**: 同一 writer 内は mutex で直列化できるが、writer 間は「読む → 判定 → 書く」の間に相手が書く。両者が残り 1 を見て両方通す。上限超過は writer 数 − 1 件まで。厳密にするには bucket ごとの flock (DR-0031 (1) 相当) が要り、合算カウンタの意味論を外れる。上限を writer 数 − 1 件まで超えるのは許容し、MANUAL に明記する (裁定 8)
- **時計の巻き戻り**: 分 / 時は NTP の後退で前の窓に戻り、既に数えた窓の値を見る (安全側)。窓の鍵が前へ戻った時に「新しい窓」と誤認してリセットしないよう、メモリバケットは「鍵が現在の鍵より小さければ現在の鍵のまま数える」とする。日 / 月は writer ごとの時計のずれで別の日ファイルへ書く writer が出る (境界付近で最大ずれ分の二重窓)
- **DR-0018 / 0019 との二重制限**: 案 B で pace_cap と自主バケットが同じ credential に掛かると、どちらで止まったかを events で分けないと調整できない (§5 で理由欄を分ける)
- **受理時点で数える** ことが上流の数え方と合うか (失敗応答を上流が数えるか) は API ごとに未確認
- **DST の日の窓長** (23 / 25 時間) を上流がどう数えるかは未確認
- 上流の `x-rate-limit-*` 便乗 (DR-0030 §3 末尾) は本計画に入れていない。必須でないので段 3 の範囲外とする

## 裁定済み (統括 2026-09-24)

1. 適用範囲: パススルーだけで始める。LLM 経路 (段 3d) は後続
2. 設定キー: `[secrets]` は id ごとの表 (`[secrets.<id>]` に `limits = [...]` 等)。置き場の設定は `[secret_store] type = "file"` に改名し、`[secrets] type` との互換は持たない (利用者が居ない)。改名は段 3a か 3b の最初で行う
3. `allow` を書かない ns は閉じる (パススルーを何も通さない)。切替手順は不要 (稼働設定にパススルーを使う ns はまだ無い)
4. ns allowlist の method に `*` は許さない
5. `missing` が空でない時は 503 (gateway が判定できない状態で、クライアントの枠超過の 429 とは区別する)
6. `X-RateLimit-Reset` は残り秒。上流の同名ヘッダ (`X-RateLimit-*` / `Retry-After`) は、gateway が出す時は落とし、出さない時はそのまま流す
7. 日次バケットは第一版では毎 request 書く (予約方式は後続)
8. writer 間の同時到着で上限を writer 数 − 1 件まで超えるのは許容する (MANUAL に明記する)
9. `jiff` を依存に足す
