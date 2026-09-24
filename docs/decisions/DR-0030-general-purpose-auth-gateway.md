# DR-0030: 汎用の認証 gateway を下に敷き、LLM をその上の 1 利用者にする

- Status: Accepted (kawaz 裁定 2026-09-24)。§1 crate 分割・§2 パススルー・§3 レート制限・§4 allowlist は実装済み (v0.55.0〜v0.58.0)、§6 の `jwt` / `issued` は未実装
- Date: 2026-09-17

## Context

このプロダクトが実際に担っている仕事は、名前が示す「LLM の proxy」より広い。

- **認証の差し替え** — クライアントは gateway の token で話し、上流へは本物のキー (OAuth token / API key / SigV4) が付く (DR-0025 §1 が Responses でやったことはまさにこれで、LLM 固有の変換は 1 つも要らなかった)
- **credential の使い分け** — 複数の認証情報を優先度と枠の状況で選ぶ (DR-0015)、断られたら次へ回す (DR-0009)、refresh と版の追従を 1 台に絞る (DR-0010 / DR-0022)
- **自主的な枠管理** — 借りる速度を自分で抑える (DR-0018 / DR-0019)
- **namespace ごとの入口と記録** — ns 認証 (DR-0006)、日次集計 (DR-0011)、知らせ (DR-0012)

このうち **LLM に固有なのは「usage の方言」「prompt cache 戦略」「model catalog」「origin の読み方」だけ**で、残りは相手が LLM API である必要がない。

そして実際に、LLM でない API を同じ扱いで通したい要求が出ている。手元のアプリから外部 API を叩くたびに、アプリごとにキーを配り、アプリごとにレート制限の面倒を見て、アプリごとに秘密をファイルへ置いている。gateway は既にその 3 つを LLM 向けに解いているのに、隣の API には効かない。

同時に、上流の枠情報に頼った制御には限界がある。DR-0018 / DR-0019 は Anthropic がくれる枠ヘッダと reset 時刻が前提で、それを返さない API では何も効かない。**上流が教えてくれなくても自分で数えて止める**手段が要る。

本 DR が決めるのは **責務の境界と URL・レート制限・ns 認証の形**であって、プロダクトの改名でも、実装の着手指示でもない (Status は Proposed)。

## Decision

### 1. 汎用層を crate として切る。プロセスは分けない

workspace に汎用層の crate `gateway-core` を切る。

| 側 | 持つもの |
|---|---|
| 汎用層 | 認証の差し替え、credential の使い分けと refresh、自主レート制限、request 計数、ns 認証と allowlist |
| llm-gateway | LLM 固有 — usage の方言、cache 戦略、model catalog、origin の読み方、既存の provider preset |

**依存方向を固定する: llm-gateway → 汎用層。逆は禁止。** 汎用層は provider の名前も LLM の語彙 (model / token / cache) も知らない。これは DR-0014 §3 の判定基準「core は provider の名前を 1 つも知らない」を 1 段外側へ広げたもので、測り方も同じ — **汎用層のコードに `model` / `usage` / provider 名 (`anthropic` / `openai` 等) が現れないこと**。`token` は認証の語 (bearer token) として汎用層が使うので判定語にしない。

**プロセスは分けない。** 別 daemon にすると、1 リクエストに 2 段の hop と 2 つの config が要り、credential store が 2 つに割れて DR-0010 の flock が守っている「更新は 1 箇所」の前提が崩れる。daemon / service (DR-0028)、stats (DR-0011)、events (DR-0012) も二重に持つことになる。crate 分割だけで依存方向の目的は達する。

プロダクト名と bin 名は据え置く。改名は本 DR の裁定対象ではない。

### 2. URL は ns の下に「行き先」の段を持つ

```
/ns-<ns>/llm/<provider>/<endpoint>      LLM 経路 (現行 /ns-<ns>/v1/... の後継)
/ns-<ns>/<apifqdn>/<endpoint>           任意ホストへの無変換パススルー
```

DR-0006 が「gateway の機能はすべて ns 配下」と決めた形をそのまま延長し、ns の直下に**どこへ出るか**の段を足す。`llm` は行き先の 1 つで、特別扱いしない。

汎用パススルーは **行き先ごとに「上流 URL + credential + 許可する endpoint」を設定で登録**し (`[upstreams.<name>]`、名前は既定で apifqdn、任意のラベルも可)、`/ns-<ns>/<name>/<rest>` の `<rest>` を上流 URL にそのまま連結する。gateway はクライアントの `Authorization` を落として登録された認証 (API key / bearer / OAuth token) を載せて、それ以外は本文もヘッダも応答も触らずに流す。DR-0025 §1 と同じ「認証だけ差し替える」形で、変換は持たない。

対象ホストは **設定に書いたものだけ** (§4)。任意の URL を中継する open proxy にしない。

確定した設定の形 (gateway-core 分割 段 3、統括 2026-09-24):

```toml
[upstreams."api.x.ai"]      # 名前 = 既定は apifqdn、任意ラベル可。英数字と . _ - のみ。予約名: v1 / llm-gateway / llm
url = "https://api.x.ai"    # <rest> をそのまま連結
secret = "xai"              # 固定の秘密の id ([secret_store] の置き場の xai.json。最小形 {"type":"static","payload":{"value":"..."}})
auth = "bearer"             # 載せ方: "bearer" / { header = "x-api-key" }。上流 API の形なので上流側に書く
allow = ["GET /v1/models", "POST /v1/chat/completions"]   # "METHOD path-pattern"。外れは 404 / 405 で上流に出さない

[secret_store]
type = "file"               # 既定の置き場 $XDG_STATE_HOME/llm-gateway/secrets。credential とはディレクトリを分ける
```

### 3. レート制限は gateway が自分で数える

上流が枠情報をくれるかに依存せず、gateway 自身が**時間バケットのカウンタ**を持つ。

- **単位は credential (キー) 単位**。キーの持ち主が上流と結んでいる約束が枠の実体なので、数える単位もキーに合わせる。第一版はこれだけで始める。ns × apifqdn の 2 段目 (「この ns はこの API を 1 日 100 回まで」) は要る時に足す — 設定を書く場所も別になるので、一緒に作る必要がない
- **バケットは複数宣言できる** (例: 60 req/分 と 5,000 req/日)。どれか 1 つでも埋まったら止める
- **窓は固定窓で、境界の基準時刻はバケットごとに設定する**。日次・月次の reset は API ごとに現地時間か利用者固定の時刻で切られるのが普通なので、バケット宣言に基準タイムゾーン (IANA 名、または固定 offset) を持たせる。省略時は UTC。分・時のバケットは境界を UTC の整数分・整数時に揃える
- **超過したら gateway が 429 + `Retry-After` を返す**。溜めて遅延送信する形は採らない (理由は Alternatives)
- **アプリ向けに `X-RateLimit-Limit` / `-Remaining` / `-Reset` を出す**。値は **gateway 自身のバケット**のもので、上流の残量ではない。上流の値を混ぜるとどちらの枠で止まったのか読めなくなる
- **日次バケットは再起動を跨ぐので、writer ごとのファイルに書いて読む時に合算する** (DR-0011 の日次集計と同じ形、issue の「合算可能なカウンタ」に当たる)。**分単位のバケットは再起動で消えてよい** — 消えた側に倒れるのは 1 分ぶんの緩みで、その保存のために書き込み頻度を上げる価値がない
- 上流が `x-rate-limit-*` 等を返す場合は DR-0007 と同じ「**便乗**」として活用してよいが、**必須にしない**。返さない上流でも自主バケットだけで成立する

### 4. ns ごとに行き先を allowlist する

ns 設定に、**使える apifqdn / パス prefix / メソッド**を宣言する。宣言に無い組み合わせは 404 / 405 で、gateway は問い合わせない。

LLM 専用だった間は、通る先が config の routing で閉じていたので ns の権限は実質「使えるモデル」だけだった。汎用化すると **副作用のある API (POST / DELETE) が通りうる**ので、ns の権限は行き先の宣言として明示が要る。

この帰結として: **ns 認証が実質 dummy (固定 token を配っただけ) でよいのは、その ns の allowlist が読み取り系に閉じている場合に限る。**

### 5. 秘密の置き場 — 預かりものはファイル、自前の秘密は JWKS 1 つだけ

汎用パススルー用のキー・bearer (静的 secret) は、**Store 層の interface を切って設定でプラガブル**にし、第一版の backend は今の credential と同じ**生のファイル** (kawaz 裁定 2026-09-24)。ファイルの版と排他は DR-0010 の仕組みをそのまま使う。後から `op://` 解決や cache-warden に差し替える時は backend を足すだけで、読む側は変えない。

**gateway が持つ自身の秘密は、`issued` (§6) 用の署名鍵 = JWKS ただ 1 つ。** これ以外は全部「他所から預かったキー」であって gateway が作ったものではない、という区別を保つ。その JWKS の置き場も段階を踏む:

1. **初期はファイル管理** — 他の credential と同じ扱い (版 + flock)
2. **cache-warden が稼働したら、そちらへ移す** — issue `2026-09-15-store-layer-for-replaceable-persistence` の Store 層の backend として

平文ファイルで置く期間の危険度は預かりキーより高い (§Consequences)。

### 6. ns 認証は方式の enum。出口は主体に揃える

ns 設定の `auth` を方式の enum にする。**検証の出口は方式によらず「Bearer → 主体 `(ns, subject, kid)`」に揃え**、下流 (allowlist / レート制限 / stats / events) は主体だけを見る。方式が増えても下流は変わらない。

| 方式 | 中身 |
|---|---|
| `token` | 固定文字列の照合。既存アプリ向けの最低線 (現行 `auth_token` の位置) |
| `jwt` | ns 設定が持つ JWKS (kid → 公開鍵) で検証する |
| `issued` | gateway 自身が IdP として access / refresh を発行する |

`jwt` の規定:

- **alg は kid ごとに設定で固定し、JWT ヘッダの `alg` を信用しない**。ヘッダの申告で検証アルゴリズムを選ぶと、鍵の取り違えと `none` 系の事故の口になる
- 鍵種は **Ed25519**
- **必須の検証は「有効な署名 + 既知の kid + `exp` + 基本 claim」**。`iss` / `aud` の照合は **ns ごとの任意** (受け手が 1 つしかいない配置で aud を必須にすると、既存アプリに意味のない設定を強いる)
- **寿命の上限は ns 設定で決める**。既存アプリ向けに長寿命の JWT を許す ns を作れる
- **失効は kid の削除、ローテーションは新しい kid を先に配ってから旧 kid を消す** (JWKS に両方が載る期間を作る)

`issued` の方向: access / refresh とも JWT とし、gateway の署名鍵は JWK (JWKS) として管理する (置き場は §5)。**発行の口は 2 つ** (kawaz 裁定 2026-09-24): 最初の 1 本 (bootstrap) は host 上の **CLI** が署名鍵ファイルを直接読んで refresh token を標準出力に出す (gateway は保存しない。kid ごとの最終発行時刻だけ DR-0010 の flock 下で記録)。refresh はアプリが自分で行うので **HTTP の token endpoint** (`POST /ns-<ns>/auth/token`、OAuth 2 の `grant_type=refresh_token` の形に合わせ、標準クライアントがそのまま使える) を持つ。access の寿命と refresh の rotation は §未確定の方向どおり。

**Claude Code 向けの最初の運用は `jwt` 方式の長寿命 token** (kawaz 裁定 2026-09-24)。Claude Code 側に token を更新する仕組みが無いので `issued` の refresh は使えず、ns 設定で長寿命を許した JWT を CLI で鋳造して配り、ローテは手動 (新 kid を先に配って旧 kid を消す) で定期的に行う。ローテの周期と手順は runbook に書く。`issued` の HTTP endpoint は refresh を自前で回せるアプリが出た段階でよい。

**helper CLI** (鍵ペア生成 + JWKS 断片の出力 + 手元での署名 + 上記の bootstrap 発行) を用意するが、**生成物を標準出力に出すだけで gateway は保存しない**。アプリの秘密鍵を gateway が作って持つ形にすると、§5 の「自前の秘密は `issued` 用の JWKS 1 つだけ」が崩れる。

### 7. 進め方

1. 本 DR の裁定
2. crate 分割 + 汎用パススルー route (認証差し替えと credential の使い分けまで)
3. 時間バケットのレート制限 + ns の allowlist
4. ns 認証 `jwt`
5. `issued` (別途設計)

**xAI (`api.x.ai`、Responses 形式) は本 DR を待たない。** DR-0025 の受け口 (`POST /ns-<ns>/v1/responses`、body の `model` で route を選ぶ) に乗るので、plain な API key を bearer で載せる credential 種別 (現行は OAuth 2 種と Bedrock の key しか無い) を 1 つ足し、`provider = "openai"` + `url = "https://api.x.ai/v1"` の route を 1 つ書けば通る (`x_search` が実機で動くことは 2026-09-17 に確認済み)。

## Alternatives Considered

- **汎用層を別プロダクト / 別プロセスとして独立させる**
  - 不採用理由: 1 リクエストに 2 段 hop と 2 config が要る。credential store が割れて DR-0010 の flock の前提 (更新者は 1 つ) が崩れる。daemon / service / stats / events を二重に持つことになる。依存方向を固定したいだけなら crate 分割で足りる
- **レート制限を「遅延して通す」(超過分をキューに積んで枠が空いたら送る)**
  - 不採用理由: 待ち行列と、待っている間の接続保持が増える。クライアントから見て「遅い」と「詰まっている」が区別できず、タイムアウトの責任が gateway へ移る。429 + `Retry-After` なら待ち方はクライアントが決められる
- **host ベースの virtual host (`<api>.gateway.local/...` で行き先を表す)**
  - 不採用理由: クライアント側は base URL を差し替えるだけで済ませたい。パスの段なら 1 つのホスト名と 1 つの証明書で全部に届く。DNS と証明書を行き先の数だけ用意する理由がない
- **上流の枠ヘッダだけでレート制限する (現状の延長)**
  - 不採用理由: 枠情報を返さない上流では何も効かない。汎用化の対象はまさにそういう API を含む
- **JWT の検証で `alg` を JWT ヘッダから読む (一般的な実装)**
  - 不採用理由: 検証側のアルゴリズム選択を攻撃者の申告に委ねる形になる。kid ごとに設定で固定すれば、失うのは「1 つの kid で複数 alg」だけで、それが要る場面が無い

## Consequences

- **URL が変わる。** `/ns-<ns>/v1/...` は `/ns-<ns>/llm/<provider>/...` へ移る。**切替中は旧 URL を一時 alias として binary に持つ** (kawaz 裁定 2026-09-24): 旧 `/ns-<ns>/v1/<endpoint>` は今と同じ「本文の model で route を選ぶ」扱いのまま残し、**alias の hit 数を `/llm-gateway/self` に出して、0 が続いたら消す**。手元の Caddy は `lb_policy first` で 11301 が落ちた時だけ 11302 に回し 404 では回らないので、alias 無しで unstable だけ先にパスを変えると旧 URL のクライアント (Claude 設定 ×3、codex) はその瞬間から 404 になり、走行中の Claude セッションは起動時の base URL を持ち続けるため一斉切替でも止まる
- **汎用層に LLM の語彙が現れないことをテストで縛る**必要がある (DR-0014 §3 の判定基準と同じ手当て)。文章の禁止だけでは、便利な近道として漏れる
- **gateway が 429 を返す理由が 2 つになる** — 全経路が断られた結果 (DR-0009 / DR-0014 §8) と、自主バケットの超過。events と応答で区別が付く形にしないと、上流が混んでいるのか自分で止めたのか読めない
- **日次カウンタが stats と同型の永続を 1 つ増やす。** issue `2026-09-15-store-layer-for-replaceable-persistence` の「合算可能なカウンタ」にそのまま当たるので、Store 層を切る時に一緒に収まる
- **副作用のある API が通りうる**ので、ns 認証の設計不足が実害に変わる。§4 の allowlist はその歯止めであり、省略できない
- **パススルーは `Cookie` 等の認証以外のヘッダもそのまま上流へ渡す** (§2 の無変換の帰結)。クライアントは手元のアプリが前提で、ブラウザのように他所の cookie を勝手に載せる相手は想定しない
- **静的 secret の版は更新時刻 (mtime) だけで比べる** (DR-0010 と同じ制約)。同じ時刻の粒度で 2 度書き換えると読み直しを取りこぼしうる
- **`issued` の署名鍵は、預かりキーより漏洩の影響が広い。** 上流キーが漏れれば漏れた 1 本の枠を使われるが、署名鍵が漏れれば**任意の ns の access token を偽造できる** — allowlist も ns の区分も丸ごと迂回される。平文ファイルで置く期間 (cache-warden 以前、§5) はこの差が剥き出しなので、**鍵ローテの runbook を `issued` 稼働の前提条件とする** (漏洩に気づいてから手順を考えるのでは、発行済み token が生きている間ずっと偽造が通る)
- 既存の LLM 経路の振る舞い (routing / denial / spend_down / pace_cap / cache / stats) は変えない。汎用層へ移るのは所有であって挙動ではない

### やらないこと

- **任意 URL の中継** — 行き先は設定に登録した apifqdn だけ (§2 / §4)。登録を要らなくすると §4 の allowlist が意味を失い、open proxy になる
- **汎用パススルーでの本文変換** — 認証だけ差し替える (§2)。変換を足したくなった時点で、それは LLM 側と同じく「方言を知る層」の仕事なので上の層へ置く
- **プロダクトの改名** — 本 DR の裁定対象外
- **上流の枠情報への依存を必須化すること** — 便乗はしてよいが前提にしない (§3)

## 未確定

- **`issued` の残り** (refresh の再利用検知、access の寿命)。発行の口 (§6) と署名鍵の置き場 (§5) は決まっているので、未確定なのはそれ以外。以下 2 点は**方向だけ**決まっている:
  - **鍵ローテの順序は「新 kid を先出し → 旧 kid での発行停止 → 旧 kid を失効」**。失効してよい時刻は **旧 kid で最後に発行した access token の `exp`** から機械的に決まる (それ以降は旧 kid で検証すべき token が 1 つも残らない)。そのため **gateway は kid ごとの最終発行時刻を覚える**
  - **refresh token も refresh のたびに新 kid で発行し直す (rotation)**。refresh が旧 kid のまま残ると失効時刻が refresh の寿命に引きずられるが、毎回新 kid へ載せ替えれば**失効判定は access の `exp` だけで足りる**

## 関連

- [DR-0001](./DR-0001-scope-and-architecture.md) — 当初のスコープ。本 DR はその外側の境界を引き直す提案
- [DR-0006](./DR-0006-namespace-routing.md) — 機能はすべて ns 配下。本 DR §2 の URL はこの形の延長
- [DR-0009](./DR-0009-credential-denial-fallback.md) — 断られたら次へ。汎用層へ移る機構の 1 つ
- [DR-0010](./DR-0010-credential-cross-process-lock.md) — credential の flock。プロセスを分けない判断 (§1) の根拠
- [DR-0014](./DR-0014-target-architecture-provider-preset.md) — 三境界と provider = 小 trait、「core は provider 名を知らない」。本 DR §1 はこの判定基準を 1 段外へ広げる
- [DR-0018](./DR-0018-spend-down-priority.md) / [DR-0019](./DR-0019-pace-cap.md) — 上流の枠情報に基づく制御。本 DR §3 の自主バケットはこれを置き換えず、枠情報が無い上流を埋める
- [DR-0025](./DR-0025-responses-ingress.md) — 無変換パススルー + 認証差し替え。本 DR §2 の汎用パススルーは同じ形を LLM 以外へ広げる
- `docs/issue/2026-09-15-store-layer-for-replaceable-persistence.md` — 永続化を意味論で切る Store 層。本 DR §3 の日次カウンタはその「合算可能なカウンタ」
