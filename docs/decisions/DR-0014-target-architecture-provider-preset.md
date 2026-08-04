# DR-0014: 目標アーキテクチャ — ingress / egress / exchange と provider preset

- Status: Accepted
- Date: 2026-08-04

## Context

codex (ChatGPT OAuth) を relay (cliproxyapi) 経由でなく**ネイティブ対応**する
ことが決まった。OAuth は gateway が独立してログインし自前で保持する
(`~/.codex/auth.json` とは並走しない)。

これは **provider が初めて複数になる**契機になる。今までは upstream が
実質 Anthropic 一系統だったため、Anthropic の方言が構造の各所へ水平に
染み込んでいる。

- `backend/anthropic*` が `Provider` trait の実質的な唯一の実装で、Bedrock は
  その 4 点差し替え (authorize / base_url / beta_policy / adapt) として乗っている
- 枠ヘッダの読み方 (`usage`)、枠照会 API (`limits`)、本文 usage の形 (`stats`)、
  beta フラグ学習 (`denied_beta`) は、いずれも **Anthropic 固有の知識**なのに
  core 側のモジュールとして横に並んでいる
- per-route の状態 (denial の印・affinity・beta 学習) は core が
  **route 名の String をキーにした map** で持っている。provider が増えると
  「どの provider の話か」を文字列から復元することになる

OpenAI (Responses API) は認証も枠の概念も応答形式も違う。この状態のまま
2 つ目の provider を足すと、分岐が各モジュールへ散る。

語彙も既に乱れている (`docs/design/architecture-overview.md` §5.2)。特に
**`provider` が二重定義** — backend trait の名前と、DR-0004 が導入する
「話す API 名」が衝突している (§5.2-e)。この裁定は codex 対応の実装前に必須。

境界と IF を先に整理する。骨格の正本は同文書 §7。

## Decision

### 1. 三境界の語彙 — ingress / egress / exchange

層の名前を 3 つに定める。

| 語 | 意味 |
|---|---|
| **ingress** | 入口。クライアント方言 (現状 Anthropic Messages のみ) を受けて、正規形 + メタ (ns, model, SessionKey, prefix) にする層。現 server の parse / authorize が該当 |
| **egress** | 出口。正規形を upstream 方言へ変換して送る層。現 `backend/` |
| **exchange** | 1 転送の生涯を持つ型。観測フック (usage 抽出 / stats tap / 節目記録 / events publish) の掛け先 |

相手方の呼称は既存語彙のまま **`client` / `upstream`** を維持する。
レイヤの名前と相手方の名前を分ける (「入口」と「入ってくる相手」は別の話)。

ingress / egress は**場所**の名前であり、**そこに立つ実装が provider** である。

現 module `relay` は「中継」の名前を持ちながら実体は観測専用という乱れが
あった (§5.2-a)。これは **exchange に吸収する** — 1 転送の生涯を持つ型に
観測フックが掛かる、という形が実体に一致する。

### 2. provider = 小 trait の束

一枚岩の `Provider` trait を持たない。責務ごとに小 trait へ割り、
provider はその**束 (preset)** として定義する。

| trait | 責務 (provider 毎に違う「方言」) |
|---|---|
| `Auth` | 認証フロー (OAuth PKCE パラメータ・refresh・SigV4 等) |
| `Wire` | リクエスト変換 + 送信。**パラメータ意味論の変換表を含む** |
| `Metering` | 応答からの抽出 — 枠ヘッダ → quota スナップショット、本文 usage → トークン数、拒否シグナル (429 / retry-after) の読み方、単価表 |
| `QuotaApi` (optional) | 枠照会 API の叩き方 |

**パラメータ変換は `Wire` の変換表に置く。** 例: Anthropic の effort / thinking
→ OpenAI `reasoning.effort`。対応の無いパラメータの取捨も同じ表に書く。
変換規則が散らないよう、方言の対応関係は方言変換の担当者が 1 箇所で持つ。

**capability の非対称は optional 取得で表す** (`fn quota_api(&self) -> Option<&dyn QuotaApi>` 的な形)。
枠照会 API は Anthropic OAuth にはあるが Bedrock には無い。beta フラグ学習の
ような provider 固有機能も同じ扱いにする。「無い」を空実装やエラーで表さず、
**型として無い**ことを示す。

**小 trait 分割の根拠**は既存実装にある。Bedrock は「認証は SigV4 だが方言は
Anthropic」— 認証の軸と方言の軸が**直交している実証**であり、DR-0004 の
2 軸分離と同型である。

### 3. core = IF 規定、provider = impl の preset

- **core が持つのは状態機構の IF 規定**。trait と入出力の契約、共通の状態機械型。
  denial の印の付け外し、quota スナップショット、metering の抽出結果などの
  「型と契約」は core が規定する
- **provider はその IF の provider 毎 impl を preset として束ねて持つ**。
  per-credential / per-route の状態 (denial の印、quota、beta 学習、枠照会
  スケジュール) の**所有も provider 側**。router は各 route に
  「今使えるか?」と聞くだけ (tell-don't-ask)。現在の「core が route 名 String を
  キーに map を持つ」構造を置き換える
- **横断機構のみ core が所有する**。ドメインが provider 間比較・全 provider 合流
  であるものは、定義上 provider の外にある:

| 横断機構 | core が持つ理由 |
|---|---|
| affinity | session → route は provider **間**で選ぶための状態 |
| event bus | 購読者は全 provider のイベントを 1 本のストリームで欲しい。発火は各 provider、bus は 1 個 |
| stats writer | 日次ファイルの writer は 1 本。抽出は provider、書く先は共有 |

#### 判定基準: 「core は provider の名前を 1 つも知らない」

この設計が達成できたかを測る基準を 1 つに定める。**core のコードに
`claude` / `openai` / `bedrock` といった provider 名が 1 つも現れないこと**。

現れたなら、それは provider 固有の知識が core へ漏れている印であり、
3 つ目の provider を足すときに同じ場所を再び触ることになる。

### 4. 内部正規形 = Anthropic Messages 形式

中立 IR は**新設しない**。内部正規形は Anthropic Messages 形式とし、
egress の `Wire` が方言変換を担う。

理由: クライアントが Claude Code (Anthropic 方言) である。中立 IR を挟むと
tool use / SSE イベントの対応付け / beta 機能の表現で変換の完全性が沼になり、
しかも**その苦労は現在の唯一のクライアントには 1 mm も報われない**
(Anthropic → 中立 → Anthropic の往復が増えるだけ)。

将来 OpenAI 方言のクライアントを受ける場合に備え、
**「ingress アダプタ → 正規形」を足す拡張点だけを型で確保**する。実装はしない。

### 5. composite provider (Bedrock)

Bedrock は「独自 Auth + 通訳は他 provider へ委譲」の **composite preset** として
表現する。DR-0004 が置いた composition provider は、小 trait の束になって初めて
素直に書ける。

```
BedrockProvider (preset)
├ Auth     = SigV4 (独自)
├ Wire     = モデルに応じて Anthropic / OpenAI の Wire へ委譲
│            + Bedrock 差分の薄い wrapper (model ID 体系の map、
│              anthropic_version フィールド、エンドポイント形状)
├ Metering = 委譲先の抽出 + Bedrock 固有ヘッダの差分
└ QuotaApi = None (枠照会 API が無い)
```

**trait の切り方の試金石**: 「Anthropic の `Wire` を**書き直さずに** Bedrock が
再利用できるか」。Gemini 追加のような将来の話を待たず、**手元で今すぐ試せる**
検証である。書き直しが要るなら trait の切り方が間違っている。

### 6. codex 対応の実装部品

上記骨格への当てはめ。

1. **ChatGPT OAuth (PKCE + refresh)** → `credential/` の Kind 追加 + OpenAI 用 `Auth` 実装
2. **Responses API egress** → `Wire` の 2 個目の実装 (Messages → Responses 変換、
   SSE 逆変換、effort 変換表を含む)
3. **Metering の OpenAI 実装** → usage 形式差の吸収、単価表追加、429 意味論の読み方
4. **QuotaApi** → OpenAI に相当 API があるか要調査。無ければ `None` で denial のみ運用

## Alternatives Considered

**一枚岩の `Provider` trait を維持し、実装を増やす** — Bedrock が既に
「認証だけ独自、方言は Anthropic」であり、一枚岩だと Anthropic の実装を
丸ごと写すか継承もどきの委譲を書くことになる。軸が直交している事実を
型で表せない。

**中立 IR を新設する** — 変換の完全性 (tool use / SSE / beta) で沼る。
現在の唯一のクライアントが Anthropic 方言である以上、往復の変換が増えるだけで
得るものがない。

**per-route の状態を core が持ち続ける (現状維持)** — provider が増えると
「この route 名はどの provider か」を文字列から復元する処理が各所に要る。
状態の所有者と、その状態の意味を知っている者が分かれている。

**Bedrock を独立した provider として全部書く** — Anthropic の `Wire` と
ほぼ同内容の複製ができる。Anthropic 側の変更が Bedrock へ追従しない乖離を生む。

## Consequences

- `backend/` は egress へ、module `relay` (観測) は exchange へ移る。§5.2 の
  語彙裁定 (a / e) が構造の変更と同時に消化される
- `Provider` trait の分解に伴い、`Provider::needs_credential()` の dead code
  (§6-#6) は分解の過程で消える
- DR-0004 の credential 3 軸 (`type` / `provider` / 範囲) は、この trait 構成の
  上で意味を持つ。`provider` の値が preset を指し、`type` が `Auth` に渡る
  payload の形を決める
- 死蔵メタデータ (StoredCredential の priority / disabled / excluded_models、
  §6-#2) の去就は、状態の所有が provider 側へ移る本再設計の中で決める
- core のテストに provider 名が現れなくなる方向へ寄せる (判定基準の副産物)。
  provider 固有の振る舞いのテストは provider preset 側へ移る

### やらないこと

- **中立 IR の新設** — §4 のとおり。拡張点を型で確保するに留める
- **OpenAI 方言 ingress の実装** — 受ける需要が現れてから
- **effort 変換表の config 化** — `Wire` 内の定数から始める。調整需要が出てから
  config へ出す
- **§6 の乖離の一括解消** — 8 件のうち本再設計に統合するのは #1 (DR-0004 未着手) と
  #2 (死蔵メタデータ) のみ。残りは独立に潰せるので、この再設計を待たせない

## 未確定

以下は本 DR では確定させない。実装着手前または着手中に別途裁定する。

- **全滅時の自前 429 生成の置き場所**。「候補が空」の判断 = router の責務、と
  置くのが素直だが要確認 (現在は gateway が生成し、events に出ないという
  別の乖離 §6-#8 も抱えている)
- **§5.3 の語彙裁定の最終形**。`relay` → observe (本 DR では exchange へ吸収)、
  `usage` → `quota`、backend trait の改名を、新語彙
  (ingress / egress / exchange / provider) と整合させて確定する必要がある。
  §5.2 の d / f / g / h / i / j は、この裁定後に機械的に追従できる
- **OpenAI に枠照会 API 相当が存在するか**。存在しなければ `QuotaApi = None` で
  denial のみの運用になる (要調査)
- **`provider` と `type` の組み合わせ制約を型で表すか実行時検証にするか**
  (DR-0004 から持ち越し)

## 関連

- [DR-0004](./DR-0004-credential-axes.md) — credential の 2 軸分離。**本 DR は
  DR-0004 の 2 軸分離を trait 構成として具体化する** (認証の軸 = `Auth`、
  話す API の軸 = `Wire` + `Metering`)。DR-0004 の composition provider は
  §5 の composite preset として実装形を得る
- [DR-0002](./DR-0002-component-architecture.md) — OpenAI 変換を Phase 2 に
  置いた判断。本 DR はその Phase 2 に入る前の境界整理
- [DR-0003](./DR-0003-beta-flag-negotiation.md) — beta フラグ学習。provider 固有
  機能の optional 取得の例
- `docs/design/architecture-overview.md` §7 — 本 DR の骨格の正本 (議論の全文)
