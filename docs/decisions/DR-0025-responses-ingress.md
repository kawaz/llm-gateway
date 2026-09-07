# DR-0025: Responses 形式の受け口 (無変換パススルー + 認証差し替え)

- Status: Accepted
- Date: 2026-09-07

## 文脈

これまでクライアントは Claude Code の 1 系統だけで、gateway が受ける形も
Anthropic Messages 形式だけだった (DR-0014 §5)。ChatGPT サブスクの Codex backend へは
Messages → Responses の変換 (`preset/openai/request.rs`) を通して出している。

そこへ **codex CLI をクライアントにしたい**という要求が来た。codex CLI が話すのは
Responses API なので、受ける形が 2 つ目になる。

素直に見える案は「Responses → Messages に変換して既存の正規形に載せる」だが、
第一段としては割に合わない:

- codex CLI → Codex backend は **元から同じ方言**。往復の変換 (Responses → Messages
  → Responses) を挟むと、変換の穴 (`reasoning.encrypted_content` の持ち回り、
  `store` / `include` / `additional_tools` のような Responses 固有の欄) がそのまま
  劣化になる。**その苦労は今のクライアントに 1 mm も報われない** (DR-0014 §5 と同じ理屈)
- codex CLI に gateway を使わせたい動機は、まず**認証・経路選択・枠管理・記録**
  であって、方言の相互乗り入れではない

## 決定

### 1. Responses の受け口はパススルー。変換は次段

`POST /{ns}/v1/responses` と `POST /v1/responses` を生やす。この口で受けた 1 本は、
**model 欄の alias 解決以外は本文に触らずに上流へ渡し、応答も無変換で流す**。

- `store` / `include` / `instructions` / `input` / `tools` / `stream` は
  クライアントが送ったまま。`stream` の既定も直さない
- 応答は上流の Responses SSE をそのまま中継する (`ResponseMode::Passthrough`)
- gateway が差し替えるのは **認証だけ** — クライアントの `Authorization` を落とし、
  既存の `ChatGptBearer` で OAuth token と `chatgpt-account-id` を載せる

Responses → Messages の変換 (= Responses を話すクライアントから Anthropic 上流へ
出す) は**この DR の範囲外**。要求が出た時点で別 DR にする。

### 2. 受けた形は型で運ぶ。path から復元しない

`RequestShape { Messages, Responses }` を `EgressRequest` に持たせ、受け口
(server の handler) が値を決めて `Gateway::forward` へ渡す。

path 文字列から起こさないのは、**どの受け口をどの形で生やしたかを知っているのは
受け口を作った側だけ**だから。転送側で `path.ends_with("/responses")` のような
復元をすると、受け口の一覧が router の定義と転送側の 2 箇所に散る。

### 3. その形を運べるかは経路が答える

`Wire::accepts(shape)` を足す。既定は正規形 (Messages) だけで、Responses も運べると
答えるのは Responses API へ出る経路だけ。

転送側は候補の経路をこれで絞る。**core が provider 名で分岐しない** (DR-0014 §3 の
判定基準を保つ) ため、「この形を運べる経路」という問いを経路自身へ投げる形にする。
alias 解決・spend_down・同格グループ・affinity は既存のまま効く。

絞った結果が空なら 404 で、文言は「そのモデルの経路が無い」ではなく
**「この形では運べない」** (`no route for model X can carry a responses request`)。
同じモデルが Messages 形式の受け口からなら通るので、区別が付かないと利用者は
設定を直すのか受け口を変えるのか判断できない。

### 4. 素性は `origin: "codex"`

出した側の見分け (`main` / `sub` / `oneshot`、DR-0024) は Messages 形式の本文
(`metadata.user_id` と `system` の請求ヘッダ) の読み方なので、Responses 形式では
付かない。経路へ聞いても `unknown` にしかならない。

そこで **受けた形そのものを素性にする**。`RequestOrigin::Codex` を足し、知らせ
(DR-0012) の `origin` に `"codex"` として出る。`unknown` と潰さないのは、
「codex CLI から来た」が確かな情報であることと、prompt cache の扱いが Messages とは
別だとこの 1 語で分かるため。

`session_id` / `prefix` は Messages 形式の欄の読み方なので付かない。ただし経路の
貼り付け (affinity) は効く — codex CLI は `Session-Id` ヘッダを送るので、
既存の導出 (`session::derive`) がそのまま拾う。

### 5. cache 戦略は当てない

`cache_control` は Messages 形式の語彙で、Responses 形式の本文に置き場所が無い。
`origin = codex` に対する戦略は常に `passthrough` (本文に触らない) とし、
keepalive (DR-0024 §2) の見張りも付かない。

### 6. metering は既存の OpenAI 実装をそのまま使う

枠ヘッダ (`x-codex-*`)・429 の読み方・単価表は変換の有無に関係なく効く。
本文 usage だけは欄の形が 2 通りになる (通訳後の `message_delta.usage` と、
無変換の `response.completed` の `response.usage`) ので、読む側が両方を見る。
同じ 1 本の消費を別表記で書いただけなので、正規区分への写像は 1 つで足りる。

採用判定 (`ResponseAdmission`、DR-0014 §9) も同じ理由で、通訳後の `error` と
無変換の `error` / `response.failed` の両方を読む。読めないと、混んでいる経路を
成功として採用して fallback の機会を捨てる。

## 影響

- `Wire::send` が受けた形を受け取るようになる (返す本文を通訳するかがこれで決まる)。
  Messages しか運ばない経路は無視する
- 中継 (relay) の経路は Messages のみのまま。転送先の gateway も同じ口を持つので
  運べる余地はあるが、確かめていないので広げない
- `GET /{ns}/v1/models` は既存のまま。codex CLI は使わない (実測 2026-09-07)

## 受け入れ確認 (実機 2026-09-07)

codex CLI 0.153.4 をカスタム provider で gateway へ向け、
`codex exec -m gpt-5.6-sol --skip-git-repo-check 'Reply with exactly: ok'` が通った。
tap は `origin=codex` / `cache_strategy=passthrough` / `route=codex-kawaz` /
`status=200` を記録し、stats に消費が載った。設定例は `docs/MANUAL-ja.md`。
