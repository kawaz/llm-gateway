# gateway-core 分割計画 (DR-0030 §1 / §7 手順 2)

DR-0030 §1「汎用層を crate `gateway-core` として切る」と §7 手順 2「crate 分割 + 汎用パススルー route」を、`just ci` が各段で通る単位に割った計画。実装 worker はこの文書の段の順に進める。推しは書くが、分岐の決定は統括と kawaz が行う。

観測は 2026-09-24 時点の `crates/llm-gateway/src/` (v0.53.1)。語彙の出現数は各ファイルの `#[cfg(test)]` より手前 (= 動く側) を大文字小文字無視で数えたもの。

## 1. 現状の module 依存図

### 1.1 crate

```
llm-gateway-cli ──> llm-gateway-server ──> llm-gateway
       └──────────────────────────────────────┘
```

- `llm-gateway-server` (`src/lib.rs` 1 ファイル、axum) が URL の解釈 (`/ns-<ns>/...` の切り出し) と **ns 認証 (`auth_token` の照合)** を持つ。ns 認証は `llm-gateway` 側には無い
- `llm-gateway-cli` は `llm_gateway::{credential, quota, daemon, config, tap, stats, preset, ...}` を直接使う (`credential` 25 箇所、`quota` 25 箇所、`daemon` 18 箇所)
- どの crate にも `publish = false` は無いが、`cargo publish` を行う recipe / workflow も無い (crates.io には出していない)

### 1.2 `crates/llm-gateway/src/` の module

「依存先」は動く側の `crate::` / `super::` 参照。`Error` / `Result` (= `crate::error`) はほぼ全員が使うので省く。

| module | 依存先 | LLM 語彙 (動く側) | 性格 |
|---|---|---|---|
| `pattern` | なし | なし | 汎用 (`*` 付きの名前照合) |
| `persist` | なし (doc コメントで stats / quota に言及するだけ) | なし | 汎用 (writer 名の正規化、原子的書き込み、一時ファイル掃除) |
| `credential/time` | なし | なし | 汎用 (RFC3339) |
| `credential` (mod.rs) | なし | `codex` 1, `cache` 1 (doc) | 汎用: `CredentialId`、`Persistence` trait / 混在: `save_login` が `oauth::Tokens` と `Kind` を取る |
| `credential/file` | なし | なし (`cache` 1 は doc の cache-warden) | 汎用 (`FileStore`: 平文ファイル + flock + 版 = mtime ns) |
| `credential/store` | `error::RefreshFailureClass`, `credential::oauth`, `quota::AuthState` / `AuthStatus` | なし | **混在**: single-flight + flock handoff + 版追従 (DR-0010 / DR-0022) は汎用、`do_refresh` が `oauth::refresh_at` と `payload.oauth_kind()` を直に呼ぶ |
| `credential/stored` | `credential/time` | `model` 12, `bedrock` 14, `codex` 15, `anthropic` 1 | **混在**: トップの `priority` / `disabled` / `last_refresh` は汎用、`excluded_models` / `denied_beta` と `Payload` の各種別 (`claude_oauth` / `codex_oauth` / `bedrock_api_key` / relay) は LLM |
| `credential/oauth` | `discovery`, `credential/stored`, `credential/time` | `anthropic` 17, `openai` 8, `codex` 13, `model` 6 | LLM (各社の token endpoint と PKCE の方言) |
| `daemon/registry`, `daemon/protocol`, `daemon/supervisor` | `config::default_state_dir` だけ | なし (`protocol` に `cache` 1) | 汎用 (DR-0028 の監督者) |
| `error` | なし | `model` 6 | **混在**: `Refresh` / `Credential` / `Login` / `Config` / `Io` / `Json` は汎用、`UnknownModel` / `UnsupportedRequestShape` / `AllUpstreamsFailed` 等は LLM |
| `quota` | `credential`, `denial`, `persist`, `provider`, `stats` | `model` 7, `usage` 9 | **混在**: `AuthState` / `AuthStatus` (認証が生きているか) は汎用、枠の窓・Snapshot は上流の枠情報 (DR-0018 / DR-0019) |
| `denial` | `credential`, `pattern`, `provider`, `quota::Snapshot` | `model` 14 | **混在**: `Reason` / `Availability` / `RouteState` の骨格 (DR-0009) は汎用、`Scope::Model` と Snapshot 由来の窓は LLM |
| `events` | `cache`, `denial`, `exchange`, `metering` | `model` 5, `usage` 9, `cache` 75 | **混在**: `Events` (通し番号 `seq`、`boot`、落とした数、broadcast) は汎用、`Event` の中身 (`model` / `cache_ttl_secs` / `Notice`) は LLM |
| `stats` | `credential`, `exchange`, `metering`, `persist`, `provider`, `quota` | `model` 14, `usage` 11, `cache` 14 | **混在**: writer 別ファイル → 閲覧時合算 (DR-0011) の仕組みは汎用、`Counters.tokens: TokenUsage` と `InputBasis` は LLM |
| `metering` | `stats` | `usage` 15, `cache` 15 | LLM (トークン区分と価格) |
| `provider` | `credential`, `denial`, `egress`, `metering`, `quota` | `model` 12, `usage` 4 | LLM (preset の小 trait 群、DR-0014) |
| `egress` | `credential`, `provider` | `model` 8, `usage` 3 | LLM (Messages 正規形の出口) |
| `exchange` | `credential`, `egress`, `events`, `metering`, `provider`, `stats`, `tap` | `usage` 30 | LLM |
| `router` | `config`, `credential`, `denial`, `discovery`, `egress`, `events`, `preset`, `provider`, `session` | `model` 60 | LLM (model → route 解決。優先度順の並べ方 DR-0015 だけが汎用の芯) |
| `gateway` | ほぼ全部 | `model` 64, `usage` 48, `cache` 69 | LLM (組み立て役) |
| `cache`, `cache/keepalive*` | `config`, `provider`, `credential`, `egress`, `events`, `metering`, `persist` | `cache` 多数 | LLM |
| `config`, `config/extends`, `config/path_expand` | `cache`, `discovery`, `metering`, `pattern` | `model` 40, `anthropic` 18 ほか | **混在**: `extends` / `path_expand` / `default_state_dir` は汎用、`Namespace` (`filter` / `routing` / `cache` / `aliases`) と credential 宣言は LLM。`Namespace.auth_token` だけが汎用 |
| `discovery`, `session`, `tap`, `status`, `statuspage_v2`, `webhook`, `preset/*` | — | 各 provider 名 | LLM |

### 1.3 依存の形 (要点)

```
pattern  persist  credential/time          ← 葉。LLM 語彙なし
   ↑        ↑          ↑
credential/file ── credential(mod) ── credential/store ──> credential/oauth ──> discovery   (LLM)
                                            └──────────> quota::AuthState ──> quota ──> provider/denial/stats (LLM)
credential/stored (トップ汎用 + payload LLM)

daemon/* ──> config::default_state_dir   (config 本体は LLM)
events ──> cache / denial / exchange / metering (Event の型が LLM を参照)
stats  ──> metering::TokenUsage / exchange / provider / quota
```

分割の障害は 3 本の矢印に集約される: `credential/store → oauth`、`credential/store → quota::AuthState`、`daemon → config`。それ以外の汎用候補 (`events` の broker、`stats` の合算) は、汎用部分を型パラメータで切り出せば LLM 側に依存しない。

## 2. 移すもの / 残すもの / 割るもの

DR-0030 §1 の表の汎用層 5 項目 (認証の差し替え、credential の使い分けと refresh、自主レート制限、request 計数、ns 認証と allowlist) との対応を右列に書く。

### 2.1 そのまま `gateway-core` へ移す

| 移すもの | 対応する §1 の項目 | 備考 |
|---|---|---|
| `pattern` | ns 認証と allowlist (§4 の apifqdn / パス prefix 照合で使う) | `llm-gateway` 側は `pub use gateway_core::pattern` で残す |
| `persist` | request 計数 (日次カウンタ)、stats の合算 | `pub(crate)` → `pub` に上げる必要がある |
| `credential/time` | credential の refresh | |
| `credential/file` (`FileStore`) と `Persistence` trait、`CredentialId` | credential の使い分けと refresh | `Persistence` が DR-0030 §5 の「Store 層 IF」の第一候補 (§5 参照) |
| `quota::AuthState` / `AuthStatus` | credential の refresh (認証が生きているか) | `quota` から型だけ抜き、`quota` は re-export する。`hint` / `login_path` は汎用の語 (再ログインの案内) なのでそのまま |
| `error` の汎用 variant (`Credential` / `Refresh` / `RefreshFailureClass` / `Login` / `Config` / `Io` / `Json`) | 全般 | core の `Error` として新設。`llm-gateway::Error` に `Core(#[from] gateway_core::Error)` を足すか、同名 variant へ写す `From` を書く (§6.3) |
| `config/extends`, `config/path_expand`, `default_state_dir` | 設定の読み込み基盤 | 後述の daemon を移すなら必須 |

### 2.2 割って移す

**credential/store (refresh の single-flight)**

- core: `CredentialStore<P: Persistence, R: Refresher>`。single-flight、flock handoff (`RefreshHandoff`)、版追従 (DR-0022)、`AuthState` の記録、「読み直して期限に余裕があれば走らない」判定をすべて持つ
- 新 trait `Refresher` (core が定義、llm-gateway が実装): 「この payload は refresh が要るか / いつ切れるか」「refresh を実行して新しい payload を返す」の 2 つ。現在 `do_refresh` 内の `oauth::refresh_at` 呼び出しと `apply_refresh` がそのまま llm-gateway 側の impl に移る
- `token_url_override` (試験用の差し替え口) は `Refresher` impl 側の持ち物になる

**credential/stored (保存形)**

- 保存ファイルの形は変えない (互換が要る。既存の credential ファイルは kawaz の手元に実在する)
- 推し: core は `StoredCredential<X, P>` を持ち、トップの汎用欄 (`priority` / `disabled`) + `#[serde(flatten)] ext: X` + `payload: P` とする。`last_refresh` は汎用欄に置かない: 書くのは refresh を適用する側 (`Refresher` の実装) だけで、静的 secret / JWKS には refresh が無いため (帰結として現行ファイルのキー順がそのまま保てる)。llm-gateway は `X = LlmExt { excluded_models, last_refresh, denied_beta_expires_ms, denied_beta }`、`P = Payload` (今の enum) を与え、`pub type StoredCredential = gateway_core::StoredCredential<LlmExt, Payload>` で既存名を保つ
- 注意: 現行は手書きの `Serialize` で**キー順を固定**している (人が読む層を先に出すため)。flatten と手書き Serialize の組み合わせで順序が崩れないかは実装時に試験で固定する (§6.2)
- 汎用パススルー用の静的 secret (API key / bearer) の payload は core 側に `StaticSecret` として新設し、llm-gateway の `Payload` enum にもその variant を足す (DR-0030 §7 の xAI 用「plain な API key の bearer」と同じ形なので共用できる)

**events**

- core: `Events<E: Clone + Serialize>` (通し番号、`boot`、落とした数の累積、broadcast、`Watching`)。錠の中で番号を振って流す Design rationale もそのまま移す
- llm-gateway: `Event` / `Notice` / `Cache` / `Origin` は残し、`pub type Events = gateway_core::events::Events<Event>`
- 汎用パススルーの記録は core 側に `PassthroughEvent` を新設せず、**llm-gateway の `Event` に variant を足さずに済む形**を段 3 で決める (推し: core の `Event` は持たず、server が core の passthrough 結果を llm-gateway の events に「model 空」で流すのは避け、`Events<Envelope>` の `Envelope = enum { Llm(Event), Passthrough(..) }` を llm-gateway 側で定義する)

**stats**

- core: 「writer 別の日次ファイルに書き、閲覧時に合算する」機構 (`Stats<C: Mergeable>`、日の鍵 = 地方時の日付 (DR-0011)、ファイル名、`persist` 経由の書き込み)。trait `Mergeable { fn merge(&mut self, other: &Self) }`
- llm-gateway: `Counters` (`requests` + `tokens: TokenUsage`)、`InputBasis`、旧形式の読み替え、`ByOrigin`、`Report` の整形
- 自主レート制限の日次バケット (DR-0030 §3) は同じ `Stats<C>` の上に `C = RequestCount` として載る (段 3 以降、手順 3 の範囲)

**daemon**

- 監督者は台の設定の読み方 (設定ファイル → 待ち受け先) と問い合わせパス (healthz / self / version) を利用側から受け取る (関数値と文字列の `UnitProbe`)。置き場 (状態ディレクトリ) も引数で受け、core は製品名も LLM の設定も知らない

**ns 認証**

- 現在は `llm-gateway-server` の関数 (`auth_token` を書いていない ns は検査せず通す、`Bearer <t>` と `<t>` の両方を受ける)
- core: `ns::Auth` enum (第一版は `Token` だけ。`Jwt` / `Issued` は DR-0030 §7 手順 4 / 5) と `Principal { ns, subject, kid }`。検証の出口を主体に揃える (§6)。`token` 方式の `subject` / `kid` は `None` 相当の固定値
- 設定: `Namespace.auth_token` を core の `NsAuth` 設定型で読む。llm-gateway の `Namespace` は `#[serde(flatten)] auth: gateway_core::config::NsAuth` を持つ形にし、TOML の書き方 (`auth_token = "..."`) は変えない

### 2.3 llm-gateway に残す

`credential/oauth`、`discovery`、`provider`、`preset/*`、`egress`、`exchange`、`metering`、`router`、`gateway`、`cache/*`、`session`、`tap`、`status`、`statuspage_v2`、`webhook`、`quota` (AuthState を除く)、`config` の LLM 部分、`error` の LLM variant。

### 2.4 判断が要るもの (推しを付けて並べる)

| 対象 | 推し | 理由 / 反対側 |
|---|---|---|
| `denial` の骨格 (DR-0009) | **この分割では移さない (統括裁定 2026-09-24)**。手順 2 の汎用パススルーは「1 行き先 = 1 credential」から始め、複数 credential の使い分けが要った時に `RouteState<K>` (範囲の鍵を型パラメータにし、LLM は `Scope::Model`) として切り出す | `denial` は `quota::Snapshot` (上流の枠情報) を読んで窓を決めるので、切るには Snapshot 側の抽象も要り、段が 1 つ増える。反対側: DR-0030 §1 は「credential の使い分け」を汎用層の持ち物と明記しており、手順 2 の見出しにも「credential の使い分けまで」とある。本分割の外の「後続」段で扱う (§4) |
| router の優先度順 (DR-0015) | 同上、`denial` と一緒に動かす | 優先度順の並べ方だけなら小さいが、reset-aware の並べ替えは Snapshot を読む |
| `daemon/*` | 段 1 の後の独立段で移す (段 2) | 語彙ゼロで依存は `default_state_dir` だけなので安く移る。DR-0030 §1 の表には無いが「プロセスは分けない / daemon を二重に持たない」の帰結として汎用層が持つのが自然。反対側: 移さなくても依存方向は崩れない。CLI の `llm_gateway::daemon` 18 箇所を re-export で吸収するので CLI 側の diff は無い |
| `webhook` | 残す | 送り先の設定と `events::Notice` の LLM 形に依存。汎用化は events の envelope が固まってから |

## 3. 判定基準のテスト

DR-0014 §3 のテストは `crates/llm-gateway/src/lib.rs` の `mod provider_neutrality` にある。形は「ファイル名の列挙 → `#[cfg(test)]` の手前までを小文字化 → 禁止語の部分一致 → 1 件でもあれば行番号付きで失敗」+「列挙したファイルが実在すること」の 2 本。

`gateway-core` 用はこれに倣い、次を変える:

- **列挙でなく crate の `src/` 以下の全 `.rs` を再帰で走査する**。crate 丸ごとが汎用層なので、列挙は新ファイルの検査漏れを生む。空振り防止 (DR-0014 側の `every_listed_module_exists` に当たるもの) は「走査したファイル数が 1 以上」と「既知の 1 ファイル (`lib.rs`) が含まれる」で代える
- 禁止語: `model`, `usage`, `cache`, `anthropic`, `claude`, `openai`, `bedrock`, `codex`, `chatgpt`。`token` だけ除外 (DR-0030 §1 が LLM の語彙として model / token / cache を挙げ、token は認証の語として汎用層が使う)
- `cache` を禁止語に入れる帰結として、core へ移る credential store の控えの型 `Cached` は段 1 で改名する (例: `Held`)。cache-warden に言及する doc コメント (`credential` の `Persistence` 周り) も言い換える
- doc コメントも対象にする (DR-0014 側と同じ)。「LLM の場合は…」という説明が core に書かれた時点で、責務が漏れ始めている兆候なので弾いてよい
- 置き場: `crates/gateway-core/tests/vocabulary.rs` (integration test、`env!("CARGO_MANIFEST_DIR")` から `src` を引く)。`just test` / `just ci` は `--workspace` なので追加の recipe は不要
- ファイル全文を対象にする (DR-0014 側と違い `#[cfg(test)]` 以降も読む)。試験もこの crate のコードなので、試験の値 (照合する名前・ファイル名) も中立な名前にする
- 部分一致で誤検知しうる語 (`remodel` 等) は現れた時に単語境界の判定へ変える。先回りで正規表現にはしない

加えて**依存方向**も機械的に縛る: `crates/gateway-core/Cargo.toml` に `llm-gateway` を書けば循環で cargo が拒否するので追加の検査は要らない。ただし `llm-gateway-server` が両方を見る形 (§6.4) では、server 内のコードは判定の対象外になる点を文書に残す。

## 4. 段分け

各段の終わりで `just ci` (fmt check + clippy -D warnings + `cargo llvm-cov --workspace --fail-under-lines 85` + release build) が通ること。**既存の振る舞い・設定・保存ファイルの形・URL は段 1〜2 で一切変えない** (DR-0030 Consequences「移るのは所有であって挙動ではない」)。

### 段 0: crate の器

- 完了条件: `crates/gateway-core` が workspace member にあり、`pattern` / `persist` / `credential/time` を移し、`llm-gateway` が `pub use gateway_core::{pattern, ...}` で既存パスを保っている。§3 の語彙テストが入っていて緑
- 検証: `just ci`。語彙テストに禁止語を 1 語入れたファイルを一時的に置いて赤になることを手元で 1 度確かめる (commit しない)
- やらないこと: credential / events / stats には触らない。`llm-gateway-server` / `-cli` の `Cargo.toml` は変えない
- 実施済み: `crates/gateway-core` を新設し `pattern` / `persist` / `credential::time` を移動、`llm-gateway` は `pub use` で既存パスを維持。語彙テストはファイル全文 (試験を含む) を読む

### 段 0.5: DR-0031 (Store 層) の起草・裁定

- 完了条件: `docs/decisions/DR-0031-*.md` が Accepted で、「単一 writer の更新」の trait (§5) の操作・版・排他・fail-closed の契約が決まっている
- 検証: kawaz / 統括の裁定
- やらないこと: コードの変更。リース / LWW スナップショット / 合算可能カウンタの backend 差し替えの詳細設計

### 段 1: credential と ns 認証

- 完了条件: `Persistence` / `FileStore` / `CredentialId` / `CredentialStore<P, R>` + `Refresher` trait / `AuthState` / core の `Error` / `StoredCredential<X, P>` / `NsAuth` + `Principal` が core にあり、llm-gateway は `oauth` を `Refresher` として実装して使う。server の ns 認証は core の `NsAuth::verify` を呼ぶ。既存の credential ファイルを読んで書き戻した結果がバイト一致する。`Persistence` の形が DR-0031 に合っている。控えの型 `Cached` は改名済みで、§3 の語彙テスト (`cache` を含む) が core 全体で緑
- 検証: `just ci`。既存試験 (`credential/store.rs` の refresh 競合試験群、server の `tokenless` / `locked` ns 試験) が移動後も同じ名前で緑。追加で「現行形式の credential ファイル (claude_oauth / codex_oauth / bedrock_api_key の 3 種) を読み書きしてバイト一致」の試験を置く
- やらないこと: `denial` / router / events / stats / daemon には触らない。ns 認証の `jwt` / `issued` は足さない (enum の variant も作らない)。設定の書き方は変えない

- 1a 実施済み: `Persistence` / `FileStore<V>` / `CredentialId` / core `Error` (`RefreshFailureClass` を含む) を core へ移し、`Persistence` を DR-0031 §2 (1) の形 (書き込みと読み直しは権利越し、版は `Option`) にした。llm-gateway は `pub use` と別名 (`FileStore = FileStore<StoredCredential>`、`CredentialPersistence`) で既存パスを保ち、`Error` へは同名 variant へ写す `From` を足した。控えの型 `Cached` は `Held` に改名

- 1b-1 実施済み: `AuthState` / `AuthStatus` を core へ移し (`quota` は re-export)、`StoredCredential<X, P>` (トップは `priority` / `disabled`、書き出しの並びは core が固定し `type` / `payload` は trait `TaggedPayload` が出す) に総称化。llm-gateway は `LlmExt` と `pub type StoredCredential` で保ち、拡張欄への参照は `.ext.` 経由にした

- 1b-2a 実施済み: llm-gateway の中で `credential::refreshing` に `Refresher` trait と `CredentialStore<P, R>` (束ね方・締め出し・版の追従・認証の観測、core の語彙と `gateway_core::Error` だけで書く) を切り出し、`credential::store` は `OauthRefresher` と、`Credential` へ写す薄い `CredentialStore<P>` になった。refresh 競合試験群は llm-gateway 側に同じ名前で残し、`OauthRefresher` と偽の token サーバを通して総称の store を試す (§6.2 の「core 内に試験用 `Refresher` を書き直す」は採らない)

- 1b-2b 実施済み: `credential::refreshing` (`Refresher` / `CredentialStore<P, R>` / `Clock`) を中身を変えずに `gateway_core::credential::refreshing` へ移し、llm-gateway は `pub use` で同じパスを保つ

- 1c 実施済み: `gateway_core::ns::{NsAuth, Principal, Authorization}` を置き、server は `NsAuth::verify(ns, Authorization ヘッダ)` を呼ぶ。`Principal` は `{ ns, subject: Option, kid: Option }` で、固定トークンでは `subject` / `kid` は `None`。`Namespace` は `deny_unknown_fields` のため flatten でなく `#[serde(rename = "auth_token")] auth: NsAuth` (中身は文字列 1 つ) で持ち、設定の書き方は変わらない。方式の enum は `jwt` を足す段で作る (今は方式が 1 つなので型に出さない)。段 1 はこれで完了

段 1 は diff が最も大きい。さらに割るなら **1a = `Persistence` / `FileStore` / `CredentialId` / `time` / core `Error`**、**1b = `CredentialStore` + `Refresher` + `AuthState` + `StoredCredential` の総称化**、**1c = ns 認証** の 3 つに切れる (各々単独で `just ci` が通る)。worker が 1 PR で扱いきれないと判断したらこちらで進める。

### 段 2: events broker / stats 合算 / daemon

- 完了条件: `Events<E>` / `Stats<C: Mergeable>` / `daemon::*` と `config::{extends, path_expand, default_state_dir}` が core にあり、llm-gateway は型別名と re-export で既存 API を保つ。日次集計ファイルと events の SSE 出力が段の前後で同じ
- 検証: `just ci`。events の通し番号・落とした数の既存試験と、stats の旧形式読み替え試験が緑。CLI の `daemon_shutdown` integration test が緑
- やらないこと: 自主レート制限のバケットは作らない (DR-0030 §7 手順 3)。`webhook` は動かさない。events の envelope (パススルー用 variant) は段 3 で足す
- 2a 実施済み: `gateway_core::events::{Events<E>, Watching<E>, Stamped}` (通し番号・起動の印・落とした数・broadcast)。番号を押すのは trait `Stamped` で、`Notice` が実装する。llm-gateway は `pub type Events = Events<Notice>` / `Watching` で既存の名前を保つ
- 2b 実施済み: `gateway_core::stats::{Stats<C: Mergeable>, Mergeable, Merged}` (日の振り分け、書き手別ファイル、読み戻し、ミリ秒日付の寄せ直し、閲覧時の合算)。`Mergeable` は core が `BTreeMap<K, V: Mergeable>` に鍵ごとの合算として実装し、llm-gateway は `Counters` / `ByOrigin` に実装する (`ByCredential` は `BTreeMap` の別名なので、孤児規則で llm-gateway 側からは実装できない)。合算 `merged` は DR-0031 §2 (3) の `Merged { value, missing }` を返し、読めなかったファイルの書き手を `missing` に挙げる。閲覧 (`Stats::report`) は best-effort なので `missing` を見ない。llm-gateway の `Stats` は鍵 (credential × model × origin) と値付けを持つ薄い包み
- 2c 実施済み: `gateway_core::config::{extends, path_expand, default_state_dir(app), xdg_dir}`。core は製品名を知らないので置き場の最後の 1 段は呼び出し側が渡し、llm-gateway の `default_state_dir()` は `"llm-gateway"` を渡す包みで置き場は変わらない。extends の試験の設定例は中立な名前に書き換えた
- 2d 実施済み: `gateway_core::daemon::{protocol, registry, supervisor}`。`Supervisor::new` は `UnitProbe` を取り、`socket_path` / `log_dir` / `registry::default_dir` は状態ディレクトリを引数に取る。llm-gateway の `daemon::{protocol, registry, supervisor}` は core を glob で再公開し、既定の置き場の関数と `registry::open()` / `supervisor::open()` / `PROBE` を足す (CLI の `Registry::open()` / `Supervisor::open()` は関数呼び出しに変わった)。段 2 はこれで完了

### 段 3: 汎用パススルー route

- 完了条件: 設定 `[upstreams.<name>]` (上流 URL、credential、許可する endpoint) を core が読み、`/ns-<ns>/<name>/<rest>` へのリクエストがクライアントの `Authorization` を落として登録済みの認証を載せ、本文・ヘッダ・応答を変えずに流れる。未登録の `<name>` と許可外の endpoint は 404。既存の `/ns-<ns>/v1/...` と `/ns-<ns>/llm-gateway/...` の振る舞いは変わらない
- 検証: `just ci`。server の試験で、ローカルの偽上流に対し「Authorization の差し替え」「本文・ヘッダ・SSE 応答の無変換」「未登録名・許可外 endpoint が上流へ届かない」「既存 LLM 経路が同じ応答」を確かめる。静的 secret の読み出しが `StaticSecretStore` (§5) 経由であること
- やらないこと: `/ns-<ns>/llm/<provider>/...` への移行と旧 URL の alias (QUESTIONS.md で裁定中)。レート制限と ns ごとの allowlist (手順 3)。`jwt` (手順 4)。複数 credential の使い分けと denial fallback (後続)
- 3a 実施済み: `gateway_core::upstream::{UpstreamSpec, AuthPlacement, Allow, Decision, check_name}` (設定の形、`METHOD パス` の照合で 404 / 405 / 通す、予約名の検査) と `gateway_core::credential::secret::{StaticSecret, StoredSecret, StaticSecretStore}` (固定の秘密、`Persistence` の `load` + `version` だけで読む、読めなければ失敗)。秘密のファイルは `priority` / `disabled` を省略でき、既定値なら省いたまま書き戻す (`TaggedPayload::OMITS_DEFAULT_TOP`)

### 後続 (本分割の境界の外): credential の使い分けの汎用化

`denial` の骨格 (DR-0009) と router の優先度順 (DR-0015) を core へ移し、パススルーでも 1 行き先に複数 credential を持てるようにする。`denial` が上流の枠情報 `quota::Snapshot` を読んで窓を決めているため、Snapshot の抽象 (枠情報を返さない行き先では空になる形) を先に切る必要がある。本分割は 1 行き先 = 1 credential で完了とし、この段は別計画で扱う (統括裁定 2026-09-24)。

## 5. Store 層 IF との関係

issue `2026-09-15-store-layer-for-replaceable-persistence` は Store 層を 4 つの意味論で切る: (1) 単一 writer の更新、(2) リース、(3) 合算可能なカウンタ、(4) LWW スナップショット。

- **DR-0030 §5 の静的 secret は (1) 単一 writer の更新**に当たる。「掴む → 最新を読む → 書く」の単位と版は credential と同じ仕組み (DR-0010) を使う、と §5 自身が書いている
- 既存の `credential::Persistence` trait (`load` / `store` / `list` / `lock` → `Guard` / `version`) が既にこの意味論の形をしている。**trait の正本は DR-0031 (段 0.5 で起草・裁定)**。新しい trait を並べず、段 1 は `Persistence` / `FileStore` を DR-0031 の形に合わせて core へ移し、それを (1) の Store 層 IF とする (統括裁定 2026-09-24)
  - 型の上では `Persistence` が `StoredCredential` を固定で扱っているので、段 1b の総称化 (`StoredCredential<X, P>`) で値の型を型パラメータ (または関連型) に上げる。これで OAuth credential と静的 secret が同じ backend (`FileStore`) に乗る
  - 「設定でプラガブル」は段 3 で入れる: 設定に `store = "file"` (既定) を置き、core が backend を選ぶ口 (enum で持つ。第一版は `File` の 1 variant) を作る。`op://` や cache-warden は variant を足すだけ
  - 静的 secret は refresh しないので `Refresher` は要らない。静的 secret 用の読み手は `Persistence` の `load` + `version` だけを使う薄い `StaticSecretStore` にする (DR-0022 の「版が変わったら読み直す」は同じく効かせる)
- issue の harnessrouter 追記「操作ごとに fail-closed か best-effort かを契約に書く」は、段 1 で `Persistence` の doc に書き足す (refresh と静的 secret の読み出しは fail-closed)
- (3) 合算可能なカウンタは段 2 の `Stats<C: Mergeable>` がその器になる。ただし Store 層としての trait (backend 差し替え) まではこの分割で切らない — 今は file 実装しか無く、(1) で先に形を固めてから同じ流儀で切る方が安い。(2) リースと (4) LWW スナップショットはこの分割の範囲外
- issue の受け入れ条件は「kawaz の着手指示が出たら Store 層の trait 設計を DR として起票」。これは DR-0031 で満たす。段 1 の前に起草するので、段 1 は既存の `Persistence` をそのまま持ち上げるのでなく DR-0031 の契約に合わせる

## 6. リスクと未確認

### 6.1 循環依存になりそうな箇所

- `credential/store → quota::AuthState`: `AuthState` を core へ移さずに store だけ移すと core → llm-gateway の逆依存になる。段 1b で型ごと移す (`quota` は re-export)
- `credential/store → credential/oauth → discovery`: `Refresher` trait で切らない限り core に oauth が引きずり込まれ、語彙テストが即赤になる。段 1b の芯
- `credential/mod.rs::save_login` が `oauth::Tokens` と `Kind` を取る: llm-gateway 側 (`oauth` の隣) へ移す
- `daemon → config::default_state_dir`: 段 2 で `default_state_dir` を core へ移すか、daemon の構築時に引数で渡す形へ変える
- `persist` の doc コメントが `crate::stats` / `crate::quota` へのリンクを持つ: core に移すと intra-doc link が壊れ、`cargo doc` が warning を出す (`just ci` は `cargo doc` を回していないので落ちはしないが、リンクは書き換える)

### 6.2 テストの移動で壊れそうな箇所

- `credential/store.rs` の試験 (`Spy` / `Watched` / `FakeTokenServer`) は oauth の token endpoint を偽物で立てて refresh 競合を検証している。core へ移すと oauth が居ないので、**試験用の `Refresher` を core の試験内に書き直す**必要がある。oauth の HTTP 形 (form / JSON) を確かめている部分は llm-gateway 側に残す。ここを雑に割ると「single-flight の試験」と「oauth 方言の試験」が片方ずつ消える
- 段 1b の実際: 試験は llm-gateway 側に残し、`OauthRefresher` と偽の token サーバを通して core の store を試している。core 単体の束ね試験 (試験用 `Refresher`) は、core を単独で使う利用者が出た時に足す
- `credential/stored.rs` の手書き `Serialize` (キー順固定) と `#[serde(flatten)]` の相性: 既存ファイルとのバイト一致試験 (段 1 の完了条件) で固定するまで信用しない
- カバレッジ下限 85% は `--workspace` 全体で測っているので crate が増えても下限は割らないはずだが、core に試験の薄い新コード (backend 選択の enum 等) が入ると全体値が下がる。段ごとに `just test` の数値を記録する
- `lib.rs` の `provider_neutrality` の `GENERIC` 列挙に `events.rs` / `stats.rs` 等が載っており、`every_listed_module_exists` がファイル実在を確かめている。段 2 でファイルを割った後も llm-gateway 側に同名ファイルが残るなら緑のままだが、名前を変えたら列挙も直す (直さなければ赤になるので気づける)

### 6.3 Error の二重化

`Error::Refresh` を llm-gateway 内の 3 ファイル以外に `llm-gateway-server` が 5 箇所でパターンマッチしている (refresh 失敗を応答へ写す)。core の `Error` を `llm_gateway::Error::Core(..)` で包むとこのマッチが 1 段深くなる。推し: llm-gateway の `Error` に `Refresh` 等を残したまま `From<gateway_core::Error>` で同名 variant へ写す (server の diff ゼロ)。反対側: 同じ variant が 2 つの enum に並ぶ重複が残る。

### 6.4 `llm-gateway-server` / `-cli` は両 crate を見る必要があるか

- **server**: 段 1 (ns 認証) までは llm-gateway の re-export で足りる。段 3 のパススルー route は LLM と無関係なので、推しは **server が `gateway-core` を直接依存に持つ**こと (パススルーのためだけに llm-gateway を経由すると、llm-gateway が LLM と無関係な API を再公開する窓口になる)
- **cli**: `llm_gateway::{credential, daemon, quota}` の使用は re-export で吸収できるので、直接依存は不要。静的 secret を CLI から登録する口 (`login` の相当) を作る時に再検討
- server は両 crate を跨ぐ組み立て役になるので、**server 内のコードは語彙テストの対象外**のまま。将来 server を「汎用の HTTP 骨格」と「LLM の route」に割る話は本計画の範囲外

### 6.5 未確認・裁定待ち

- **裁定済み (統括 2026-09-24)**: DR-0030 §7 手順 2 の「credential の使い分け」のうち denial / router の骨格はこの分割で動かさない。パススルーは 1 行き先 = 1 credential で始め、汎用化は §4 の「後続」段で `quota::Snapshot` の抽象と一緒に扱う
- `Refresher` の非同期 trait の書き方 (`async fn` in trait は Rust 1.75 以降で使えるが、`dyn` にするなら `Box<dyn Future>` が要る)。`CredentialStore<P, R>` を型パラメータで持つなら静的ディスパッチで足り、`Gateway<P>` の型引数が 1 つ増える。`Gateway<P>` を持ち回っている箇所 (server 全体) への波及は未計測
- パススルーの URL `/ns-<ns>/<name>/...` と既存の `/ns-<ns>/v1/...` / `/ns-<ns>/llm-gateway/...` の名前衝突: `v1` と `llm-gateway` (と将来の `llm`) を予約名として `[upstreams.<name>]` の読み込みで拒否する必要がある。予約名の一覧をどこが持つか (core は `llm` を知ってはいけない) は、server が core に予約名を渡す形を推す
- 静的 secret の payload を core の `StaticSecret` にした時、xAI 用 (DR-0030 §7) の「plain な API key の bearer」と同じ型で済むかは未確認 (xAI 側の作業が先に入れば、それを core へ上げる形になる)
- `issued` 用 JWKS の置き場 (DR-0030 §5) は本計画の範囲外。ただし段 1 の `Persistence` 総称化はその置き場にも使える形にしておく
