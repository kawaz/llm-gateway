# 全体設計たたき — コンポーネント境界・語彙・IF・データフロー

> 状態: **たたき** (議論用ドラフト)。合意後に `docs/DESIGN-ja.md` + 英訳へ昇格し、
> 人間向け HTML 版を別途生成する。実コードとの照合は 2026-08-03 時点の main
> (`d68d4494`, v0.16.0)。

## 1. 全体像

3 crate 構成。中心は core の `Gateway<P: Persistence>` で、HTTP 層と CLI は薄い。

```
llm-gateway-cli ──────► llm-gateway-server ──────► llm-gateway (core)
  引数解析・serve 組立      axum ルート・認証適用・        ルーティング・credential・
  6 サブコマンド            Error→HTTP status 変換         backend・観測 (全ドメイン)
```

core 内はハブ & スポーク: `gateway.rs` がほぼ全モジュールを呼ぶハブで、
周辺モジュール同士の直接依存は 3 本のみ (limits→credential 型、denial→usage 型、
router→config→provider 構築)。それ以外は**文字列 (名前) で疎結合**。

```
                    ┌─ config (+ extends)     設定の正本・検証
                    ├─ router ─ discovery     モデル解決・catalog・affinity
                    │           pattern
                    ├─ credential/            取得・refresh・flock・OAuth login
   gateway ◄────────┤─ backend/anthropic*     Provider trait + 転送 (forward)
   (ハブ)           ├─ denial ◄─ limits       締め出し・枠照会
                    ├─ usage / stats          枠ヘッダ観測 / トークン集計
                    │   └─ persist            共有の書き込み作法
                    ├─ events ─► webhook      転送イベント broadcast → POST
                    ├─ session                affinity キー導出
                    └─ pricing                静的単価表
   server 側: relay (ストリーム観測) / stats::tap (本文覗き見)
```

### 共有状態 (Gateway が保持、gateway.rs:34-46)

| フィールド | 中身 | Arc の理由 |
|---|---|---|
| `config` | `Config` (clone 保持) | — |
| `router` | catalog: RwLock / affinity: Mutex | — |
| `credentials` | `CredentialStore<P>` = Arc\<Inner\> | probe タスクへ持ち出す |
| `usage` `stats` `denials` `events` | 各 Arc | バックグラウンドタスクと共有 |

## 2. コンポーネント責務 (現状)

| モジュール | 責務 (1 行) |
|---|---|
| `config` (+`extends`) | TOML スキーマ・起動時検証。`Namespace` にモデル→credential 解決と token 検査が同居 |
| `router` | catalog を ns で絞り、モデル名 → `Vec<Arc<Route>>`。session affinity 保持 |
| `discovery` | upstream `/v1/models` の取得と正規化 (Anthropic/Bedrock 差の吸収)、alias 解決 |
| `pattern` | `*` のみの glob 照合 |
| `credential/` | 取得の唯一の窓口 `acquire`。refresh single-flight、flock (DR-0010)、OAuth login |
| `denial` | 断られた経路の一時除外 (DR-0009)。メモリのみ |
| `limits` | `/api/oauth/usage` (非公開 API) で枠取得 (DR-0007)。トークン消費ゼロ |
| `backend/anthropic*` | `Provider` trait (Official/Bedrock/Relay) + HTTP 送信 + beta Policy (DR-0003) |
| `gateway` | 1 リクエストを捌く中枢 (forward / try_route / send)、能動プローブ |
| `relay` | **中継しない。観測のみ** (初チャンク/終端/途切れ/中断の記録)。中継実体は forward.rs |
| `stats` | 応答**本文**の usage を日×credential×モデルで集計 (DR-0011) |
| `usage` | 応答**ヘッダ**の枠使用率スナップショット (DR-0007) |
| `events` → `webhook` | 転送イベントの broadcast (DR-0012) → 受け口へ POST |
| `session` | affinity キー導出のみ |
| `pricing` | 静的単価表。コストは閲覧時計算 |
| `persist` (非公開) | tmp→rename・writer 名サニタイズ等の書き込み作法 |
| `error` (非公開) | core の Error。HTTP status 変換は server の責務 |

## 3. リクエスト 1 本のデータフロー

```
POST /ns-x/v1/messages
  [server]
  ① relay::request_span (本文読み前に通し番号)
  ② 全読み (64MiB 上限) → JSON parse → ヘッダ収集
  ③ ns 判定 → Namespace::authorize (auth_token 未設定 = Open で通過)
  [core: Gateway::forward]
  ④ モデル解決 (alias なら本文 rewrite) → SessionKey 導出
  ⑤ routes_for: 可視 credential ∩ ns の優先順 → affinity を先頭へ
  ⑥ denials.candidates で候補絞り。全滅なら upstream を叩かず 429 自前生成
  ⑦ 経路ごとに try_route:
       acquire (期限 300s 前で refresh、single-flight)
       → beta Policy 適用 → forward::send (strip → authorize → adapt → POST)
       → usage.observe + events.publish (status 不問)
       → 400+beta なら学習して 1 回だけ再送
       → 2xx: affinity 記録 + denial 解除、return
         401/403/429/529: denial 記録して次へ / 5xx: 記録なしで次へ
  [server: 応答]
  ⑧ stats::tap (内側、本文から Tokens 抽出、送出は Drop 時)
  ⑨ relay::observe (外側、節目記録) → Body::from_stream
```

不変条件 2 つ:

- **経路切替はクライアントへ 1 バイト書く前まで**。ストリーム開始後の upstream 断は救えない
- **SSE は中継でなく素通し + 覗き見** (tap/observe の 2 層)。イベント parse は tap の usage 抽出のみ

## 4. フック・拡張点 (差し込み位置順)

| 拡張点 | 位置 | 性質 |
|---|---|---|
| `relay::request_span` | 本文読み前 | 通し番号を全ログへ |
| `denials.candidates` | 経路試行前 | 候補削減。全滅時は 429 自前生成 |
| beta `Policy` | 送信直前 + 400 後 1 回 | ヘッダ削除。学習は credential に永続 |
| `usage.observe` / `events.publish` | upstream ヘッダ受信直後 | 読み取りのみ、status 不問 |
| `denials.deny/allow` | 応答 status 判定時 | 印の付与・解除 |
| `stats::tap` / `relay::observe` | 応答ボディ内側/外側 | 集計送出は Drop 経由 / 記録のみ |
| `probe_in_background` | 締め出し検知時 | 非同期 limits 照会 (定期 1h + 429 直後 60s 間隔) |

パターンとして一貫: **札を外すのは必ず Drop** (RefreshHandoff / Probing / FileGuard)。

## 5. 語彙

### 5.1 定義済みの語 (正)

| 語 | 意味 |
|---|---|
| `namespace` (`ns`) | 設定上の区画。URL では `ns-` 接頭辞 |
| `Route` | provider + credential の組。試行の単位 |
| `CredentialId` | ファイル stem = config キー = route 名 (**等式が暗黙前提**) |
| `Denial` / `Reason` / `Scope` | 締め出しの印・理由 (Limited/Busy)・範囲 |
| `Limit` | 枠 1 本 (upstream の語をそのまま) |
| `SessionKey` / `prefix` | affinity キー / 会話系列識別子 (system 先頭ブロックの hash) |

### 5.2 語彙の乱れ (深刻度順、要裁定)

| # | 乱れ | 内容 |
|---|---|---|
| a | `Relay` ×3 | config の type / backend アダプタ / ストリーム観測モジュール。しかも `CodexOauth` も provider::Relay に流用中。**module `relay` は観測専用なのに名前が「中継」** |
| b | `usage` ×3 | usage.rs = 枠ヘッダ / stats の token usage / limits = 照会枠。**module 名 usage が token usage を持たない** |
| c | `scope` ×3 | denial::Scope / OAuth scope 文字列 / limits 内ローカル。DR-0004 の「範囲」軸に採ると 4 つ目 |
| d | `denied/denial` ×2 系統 | denied_beta (永続・フラグ単位) と Denials (メモリ・経路単位) |
| e | `provider` ×2 | backend trait vs DR-0004 が導入予定の「話す API 名」。**codex 対応の実装前に裁定必須** |
| f | 除外 ×3 系統 | CredentialSpec::exclude (有効) / ns.filter.exclude (有効・階層非対称) / excluded_models (**dead**) |
| g | 型名衝突 | stats::Report vs usage::Report、config::Stats vs stats::Stats |
| h | `probe` ×2 | トークン消費する probe (実リクエスト) と消費しない probe (limits 照会) |
| i | 保存動詞 | flush / save / save が不統一 |
| j | `models` ×4 | 照合パターン / 宣言モデル / 可視一覧 / discovery::Model。前 2 者は同じ TOML キー |

### 5.3 正規化のたたき台 (統括案)

- **relay 問題 (a)**: module `relay` → `observe` (中継の観測が実体)。config type の `relay` は
  「素通し転送先」の意味で妥当なので残す。backend の `provider::Relay` は `Passthrough` へ
- **usage 問題 (b)**: `usage.rs` → `quota.rs` (枠使用率)、"usage" は token usage (stats) に譲る。
  limits は「照会」なので `quota::poll` 系へ寄せる案も (b/c まとめて quota 語彙圏に統合)
- **provider 問題 (e)**: backend trait は `Backend` へ改名し、`provider` の語は DR-0004 の
  「話す API」(claude/openai/bedrock) に明け渡す — codex 対応の前提整理
- d/f/g/h/i/j は上記 3 つの裁定後に機械的に追従可能

## 6. 設計と実装の乖離 (棚卸しで発見、実機裏取り済み)

| # | 乖離 | 重み |
|---|---|---|
| 1 | DR-0004 (credential 軸分離) が未着手。軸は `type` 1 本のまま | codex 対応の前提 |
| 2 | StoredCredential の priority/disabled/excluded_models が死蔵 (login は保存し続ける) | 軸再設計の分岐点 |
| 3 | `auth_token` 未設定 = fail-open (DR-0006 裁定どおり) だが、**config.example.toml が DR 番号引用で逆を断言** | 事故誘発 doc bug |
| 4 | ns.credentials 外を routing[].credentials が指しても validate が通り、実行時 UnknownModel | validate の穴 |
| 5 | 「宣言順に試す」doc は **ns.credentials 未指定時のみ**名前順になる (明示指定時は宣言順) | doc 精度 |
| 6 | `Provider::needs_credential()` は dead code (実体は `needs_secret()`) | 平行 API |
| 7 | 能動プローブの消費トークンが日次集計に入らない | 意図か漏れか未裁定 |
| 8 | AllDenied の自前 429 が events に出ない (webhook/SSE から見えない) | イベント網羅性 |

## 7. 目標アーキテクチャ (2026-08-04 議論で合意した骨格)

背景: codex (ChatGPT OAuth) を relay (cliproxyapi) でなくネイティブ対応する
(kawaz 裁定 2026-08-03。OAuth は gateway が独立ログインで自前保持、`~/.codex/auth.json`
とは並走しない)。この対応を「provider が初めて複数になる」契機として、境界と IF を
先に整理する。

### 7.1 三境界の語彙 (kawaz 裁定 2026-08-04)

| 語 | 意味 |
|---|---|
| **ingress** | 入口。クライアント方言 (現状 Anthropic Messages のみ) を受けて正規形 + メタ (ns, model, SessionKey, prefix) にする層。現 server の parse/authorize が該当 |
| **egress** | 出口。正規形を upstream 方言へ変換して送る層 (現 `backend/`) |
| **exchange** | 1 転送の生涯を持つ型。観測フック (usage 抽出 / stats tap / 節目記録 / events publish) の掛け先。現 `relay` (観測) はここに吸収 |

相手方の呼称は既存語彙のまま `client` / `upstream` を維持 (レイヤ名と相手方名を分離)。
ingress/egress は「場所」の名前で、そこに立つ実装が **provider**。

### 7.2 provider = 小 trait の束 (kawaz 裁定 2026-08-04)

一枚岩の trait でなく、責務ごとの小 trait に割り、provider はその束:

| trait | 責務 (provider 毎に違う「方言」) |
|---|---|
| `Auth` | 認証フロー (OAuth PKCE パラメータ・refresh・SigV4 等) |
| `Wire` | リクエスト変換 + 送信。**パラメータ意味論の変換表を含む** (例: Anthropic の effort/thinking → OpenAI `reasoning.effort`。対応の無いパラメータの取捨も同表) |
| `Metering` | 応答からの抽出 — 枠ヘッダ → quota スナップショット、本文 usage → トークン数、拒否シグナル (429/retry-after) の読み方、単価表 |
| `QuotaApi` (optional) | 枠照会 API の叩き方 (Anthropic OAuth のみ存在。Bedrock には無い) |

capability の非対称は optional 取得 (`fn quota_api(&self) -> Option<&dyn QuotaApi>` 的な形)
で表現する。beta フラグ学習のような provider 固有機能も同様。

小 trait 分割の根拠: 既存の Bedrock が「認証は SigV4 だが方言は Anthropic」という
組み合わせ = 軸が直交している実証 (DR-0004 の 2 軸分離と同型)。

### 7.3 方言 (dialect) と機構 (mechanism) の分離

provider 側に持たせるのは**方言 = parse/format/変換表**のみ。状態と機構は core に残す:

- core 残留: denial の状態機械 (印の付与/解除/Drop 解除)、stats の集計・永続化、
  affinity、イベント配信、credential store の排他
- core は provider が抽出した「正規化済みの値」を受け取るだけ。
  こうしないと状態管理が provider 実装ごとに複製される

### 7.4 内部正規形

中立 IR は新設しない。**内部正規形 = Anthropic Messages 形式** とし、egress の `Wire` が
方言変換を担う。理由: クライアントが Claude Code (Anthropic 方言) であり、中立 IR は
変換の完全性 (tool use / SSE イベント対応付け / beta 機能) で沼る。将来 OpenAI 方言の
クライアントを受ける場合は「ingress アダプタ → 正規形」を足す拡張点だけ型で確保。

### 7.5 codex 対応の実装部品 (上記骨格への当てはめ)

1. **ChatGPT OAuth (PKCE + refresh)** → `credential/` の Kind 追加 + OpenAI 用 `Auth` 実装
2. **Responses API egress** → `Wire` の 2 個目の実装 (Messages → Responses 変換、SSE 逆変換、
   effort 変換表を含む)
3. **Metering の OpenAI 実装** → usage 形式差の吸収、単価表追加、429 意味論の読み方
4. **QuotaApi**: OpenAI に相当 API があるか要調査。無ければ None で denial のみ運用

### 7.6 残論点

- §5.3 の語彙裁定 (relay→observe は exchange へ吸収、usage→quota、backend trait の
  改名) を新語彙 (ingress/egress/exchange/provider) と整合させて確定する
- 数値 budget → 離散 effort の対応表は egress `Wire` 内の定数から始める
  (調整需要が出たら config へ出す)
- §6 の乖離 8 件のうち、DR-0004 未着手 (#1) と死蔵メタデータ (#2) は本再設計に統合。
  それ以外は独立に潰せる
