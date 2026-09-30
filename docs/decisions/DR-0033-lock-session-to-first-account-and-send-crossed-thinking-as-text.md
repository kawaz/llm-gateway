# DR-0033: session を開始 account にロックし、account を跨いだ session は以後 thinking を text として送る

- Status: Accepted (kawaz 裁定 2026-09-30)。改定 2026-09-30 (置換の形と enum)。部分実装 (段 1〜4 実装済、段 5 の実機確認待ち)
- Date: 2026-09-30

## 文脈

一次資料 ([Preserved thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking)) は、履歴に載って戻ってきた thinking block の署名を 3 種の束縛で検める。

| 束縛 | 中身 | Sonnet 5.5 | Fable 5.1 | Opus 5.5 |
|---|---|---|---|---|
| account 束縛 | 生成した account (または linked account) でしか効かない | あり | なし | なし |
| prefix 束縛 | block より前の `system` / `tools` / `messages` が生成時と一致していること | あり | あり | あり |
| model 束縛 | 読めるモデルが生成したモデルに限られる | あり | あり | あり |

account 束縛に外れた block は、API が黙って捨てたうえで 200 を返す。今は Sonnet 5.5 だけだが、資料は「newer Claude models の性質」「later checks add values」と書いており、他モデルに広がらない保証は無い。Fable の routing は Bedrock (別 account) を fail over 先に含むので、Fable に広がると長い thinking 連鎖が fail over の瞬間に黙って消える。

gateway は DR-0009 の fail over (401/403/429/529/5xx)、pace_cap (DR-0019)、締め出し (denial) で別 credential に移る。affinity は「前回通った経路を先頭へ寄せる」優先であって固定ではなく、経路名を覚えるだけで account の同一性を知らない。よって Sonnet 5.5 の会話は、経路が変わった瞬間に推論の連続性を失い、クライアントにも gateway にも何も見えない。

実測 (`docs/research/2026-09-30-preserved-thinking-and-account-switching.md`): account A で生成した turn 1 を account B へ再送すると、beta `thinking-binding-controls-2026-08-01` の有無によらず 200 で、`input_transformations` は beta 有でも `[]` だった。drop は `usage.input_tokens` が同一 account の対照より turn 1 の `thinking_tokens` 分だけ少ないことでしか見えない。同一 account で先行本文を改変した場合は `prefix_binding_mismatch` が報告されるので、header 自体は効いている。**header で account 不一致を観測する方式は成立せず、gateway が自前で判断する必要がある。**

一方、tool ループ中の同じ実測 (同 research doc の「tool ループ中の置換」節) では、account B へ無改変で再送した時に `input_transformations` へ `{"type":"thinking_dropped","reason":"end_user_binding_mismatch"}` が出た。資料に載る `organization_binding_mismatch` とは別の値で、text-only 履歴の再送では `[]` だった。この報告が出る条件は確定していないので、判断の根拠には使わない。

## 決定

### 1. account 束縛のあるモデルを設定で持つ。既定は `claude-sonnet-5-5` だけ

対策をモデル固有のコードにしない。「どのモデルの thinking が account に束縛されるか」は upstream の性質で、namespace ごとに変わるものではないので、namespace の下ではなく最上位に 1 つ置く。

```toml
# 書かなければ ["claude-sonnet-5-5"]。書けば置き換わる (DR-0013 の配列の規則)。
account_bound_thinking = ["claude-sonnet-5-5", "claude-fable-5-*"]
```

- 要素は routing / cache の `models` と同じパターン。照合するのは解決後のモデル名 (alias を解いた後)
- 他モデルに束縛が広がったら 1 行足すだけで以下の 2〜5 が効く
- 空配列 `[]` で無効にできる

### 2. 束縛モデルの session は開始 account にロックする

対象は決定 1 に当たるモデルの session だけ。それ以外の session の affinity は今のまま「優先」。

- **開始 account** = その `(namespace, session, model)` で最初に 2xx を返した経路の account。affinity を覚える契機 (DR-0009、2xx のみ) と同じ
- **account の同一性は credential 名で表す**。同じ credential を指す経路は同じ account、別の credential は別 account。linked account は無い前提で、同一視の設定は持たない。credential を持たない経路 (relay) は経路名を account 名として扱う (中の account を gateway は知らない)
- ロック中の session の候補は、開始 account の経路だけに絞る。affinity は「先頭へ寄せる」から「開始 account 以外を外す」になる。spend_down の昇格 (DR-0018) もロックを越えない
- 開始 account の経路が全部使えないとき (枠切れ・締め出し・pace_cap・5xx) の振る舞いは設定 `on_account_switch` で選ぶ。置き場は決定 1 の `account_bound_thinking` と同じ最上位 (global)

```toml
# 書かなければ "drop_thinking"
on_account_switch = "stay"  # "stay" | "drop_thinking" | "thinking_as_text"
```

| 値 | 振る舞い |
|---|---|
| `stay` | 切り替えない。ロック側の締め出しとして 429 + `retry-after` を返す (全経路が締め出された時の既存の応答と同じ形) |
| `drop_thinking` (既定) | DR-0009 のまま別 account へ切り替え、API が束縛外の thinking を黙って捨てるのを受け入れる。gateway は events に記録する (決定 5) |
| `thinking_as_text` | 別 account へ切り替え、その session は決定 3 の置換を以後の全リクエストに当てる |

既定を `drop_thinking` にするのは今の挙動と同じで、本文を変えない側だから。`stay` は `docs/issue/2026-09-25-low-priority-slow-requests-should-wait-not-switch.md` の「待つ」に相当する。

### 3. `thinking_as_text` で account を跨いだ session は、以後の全リクエストで thinking を assistant の text として送る

「跨いだ」= 開始 account 以外の account へ送る時点。その 1 本から後は、送り先が開始 account に戻っても続ける (session 単位のモードで、切替点の index は持たない)。

変換は送る直前の本文に対して行う:

- `thinking` block → `{"type": "text", "text": <本文>}`。本文は thinking 本文の改行 (`\n`) を全部半角空白 1 個に置き換え、末尾の空白をトリムしたもの。prefix や装飾は付けない。署名は捨てる
- `redacted_thinking` block と、本文が空の `thinking` block → 落とす (運ぶ本文が無い)
- 元にするのはクライアントが送ってきた本文そのまま。`thinking_display = "summarized"` (DR-0016) の namespace なら要約が入り、`omitted` なら空なので落ちる
- 位置は元の block と同じ。他の block (text / tool_use / tool_result) には触らない
- 決定的に変換する (同じ入力から同じ本文)。それでも初回の変換で prefix が変わるので、その session の prompt cache は 1 度壊れる。前提として受け入れる
- 跨いだ後に新しい account が生成した thinking も、次のリクエストでは同じく text になる。その session は preserved thinking の恩恵を諦め、推論は「読める独り言」として残る

この形の根拠は実測 (`docs/research/2026-09-30-preserved-thinking-and-account-switching.md` の「tool ループ中の置換」「置換の形 (account B)」節):

- 本文だけ (3/3) と改行を半角空白にした本文だけ (3/3) は `end_turn` で text の応答が返った
- 見出し (`THINKING:\n` 3/3、`(previous reasoning)\n`)、`🧠` / `💬` の接頭辞、括弧囲み、user turn の text への移動は、すべて `stop_reason: refusal` / `stop_details.category: reasoning_extraction` になった
- tool ループの途中 (直前の assistant turn が `tool_use` を持つ) で置換しても 400 にならなかった

改行を畳むので置換後は 1 行の text になり、webui 等の下流では「改行の無い長い段落」として変換済みの thinking を見分けられる。

### 4. 状態は affinity と同じ場所・同じ寿命で持つ

affinity の値 (`Binding`) に「開始 account」と「跨いだか」を足す。鍵は affinity と同じ `(namespace, session, model)`。

- 寿命は affinity と同じ (最後に通ってから 1 時間、`AFFINITY_TTL`)
- `Binding.route` (前回通った経路) は今どおり更新し、開始 account は最初に決めたまま変えない。跨いだ印は一度立てたら消さない
- 読み直し (DR-0032) の引き継ぎも affinity と同じ規則: 「経路名 + credential + provider」が前後で同一の経路に結ばれた Binding だけ残す

### 5. 跨いだ 1 本は events の `request` に印を出す

- `thinking_as_text` で変換した 1 本は `thinking_as_text: true`
- `drop_thinking` で開始 account 以外へ送った 1 本は `thinking_dropped_by_switch: true` (履歴の thinking が API に捨てられる見込みの印。実際に捨てられたかは gateway には見えない)

当てはまらない 1 本では欄ごと出さない (`skipped` / `cache_ttl_secs` と同じ流儀)。stats (DR-0011 / DR-0029) には軸を足さない: 変換は session の状態であってトークンの行き先ではなく、どの session が何本変換されたかは events で追える。

## 却下した案

- **切替時に drop を観測・記録するだけ**: 推論は救えない。しかも実測で header は account 不一致を報告しないので、観測そのものが成立しない
- **`input_tokens` の差で事後に検知する**: 同じく推論を救えず、対照 (同一 account で同じ本文を送った値) が手元に無いので差を取れない
- **切替点より前の thinking だけ text にする**: account A → B → A と戻ると、B が生成した block が A で黙って落ちる。防ぐには block ごとに生成 account を覚える segment 管理が要り、状態が session 単位から block 単位に膨らむ
- **thinking を単純に剥がして切り替える**: 推論の痕跡が消える。text に置けば署名と束縛は無くなっても内容はモデルに届く (剥がす挙動が欲しい運用は `drop_thinking` で API の drop に任せれば足りる)
- **ラベル付き置換** (`THINKING:\n` 等の見出し・絵文字・括弧を付ける、user turn へ移す): 実測で全部 `reasoning_extraction` の refusal になる

## 影響

### 実物照合

| 箇所 | 今 | この DR で要ること |
|---|---|---|
| affinity (`crates/llm-gateway/src/router.rs:391`, `Binding` は `:396-401`) | 値は `{ route: String, seen: Instant }`。経路名だけで account を知らない | `Binding` に開始 account (credential 名) と跨いだ印を足す。経路から credential 名を引く口が要る (`Route.credential` は `CredentialId`、`router.rs:44`) |
| 候補の並べ替え (`router.rs:770-785`) | 覚えた経路を先頭へ寄せるだけで候補は減らさない | 束縛モデルでロック中なら開始 account 以外を外す。spend_down の昇格 (`:766`) の後に効かせる |
| 覚える契機 (`router.rs:928-948`、呼び出しは `crates/llm-gateway/src/gateway.rs:695-697`) | 2xx で経路名と時刻を上書き | 初回だけ開始 account を決め、以後は開始 account と違う account で送った時点で跨いだ印を立てる |
| 読み直し (`router.rs:437-456`) | 引き継いだ経路に結ばれた Binding だけ残す | 規則は同じ (決定 4)。Binding が捨てられた session は次のリクエストで新しい session として開始 account を決め直す |
| 送る直前の本文 (`gateway.rs:1210-1233`) | 経路ごとに `rewrite_model` と `cache::apply` だけを当てる | ここで決定 3 の変換を当てる。経路ごとに判断する (候補の 1 本目は開始 account で変換なし、2 本目が別 account なら変換あり、がありうる) |
| Anthropic の本文透過 (`crates/llm-gateway/src/preset/anthropic/wire.rs:68-94`) | 本文を再直列化して透過、thinking と署名には触らない | wire は変えない。変換は wire の手前の core で、方言に依らず正規形 (Messages 形) に当てる |
| OpenAI 変換 (`crates/llm-gateway/src/preset/openai/request.rs:202-203`) | 履歴の thinking / redacted_thinking を警告して落とす | 跨いだ session では、落とされる前に text になっているので内容が Responses 側にも届く。OpenAI 経路の credential は別 account なので、そこへ送った時点で跨いだ扱い |
| session key (`crates/llm-gateway/src/session.rs:62-89`) | metadata / header が無ければ本文冒頭のハッシュに落ちる | ハッシュでも同じ会話なら同じ鍵なのでロックは効く。冒頭が同一の別会話は同じロックを共有する (影響は開始 account を共有するだけ) |

### DR-0024 の「本文を変えない」との線引き

DR-0024 は cache 戦略が触るのを `cache_control` だけに限り、「thinking / system / tools / messages の中身には触らない」と定めた。理由は prefix 束縛 (本文を変えると過去の署名が無効になる)。本 DR の変換は gateway が messages の中身を変える初の処理になる。線引き:

- DR-0024 の規定は cache 戦略の規定として今も正しい。cache 戦略は引き続き `cache_control` 以外に触らない
- 本文の中身を変えてよいのは、**署名がもう効かないと gateway が知っている session だけ** (決定 3 の条件)。跨いだ後の署名は account 束縛でどのみち捨てられるので、prefix 束縛を壊す損は無い
- 変換は cache 戦略より前に当てる (変換後の本文に `cache_control` の整えを当てる)。元の thinking block に `cache_control` が付いていたら、置き換えた text block に移す

### keepalive の自送信 (DR-0027)

keepalive は最後に転送した本文を控えて送り直す。控えるのが変換後の本文であることを確かめる (変換前を控えると、開始 account 以外へ送り直した時に束縛で落ちる本文を送る)。

### 実機で確かめてから入れること

- **置換の形が別の会話・別モデルでも refusal にならないこと**: 実測は Sonnet 5.5 の 1 つの tool ループだけ。段 5 で会話を変えて確かめる

### 既知の端: 変換で content が空になる assistant turn

`thinking` / `redacted_thinking` だけで text も tool_use も持たない assistant turn は、変換後に content が `[]` になる (実運用の assistant turn は text か tool_use で終わるので、ほぼ起きない)。起きた場合、API はその 1 本を 400 で断りうる。

### ロックが外れる場面

ロックは affinity と同じ寿命なので、1 時間黙っていた session と、読み直しで Binding を捨てた session は次のリクエストで開始 account を決め直す。そこで別 account に移れば、以前の thinking は今までどおり黙って落ちる。寿命を affinity と揃えたのは裁定どおりで、これを塞ぐかは本 DR の外 (塞ぐなら寿命を分けるか、履歴に署名付き thinking がある session を「開始済み」とみなす判定が要る)。

### 実装の段分け案

| 段 | 中身 | 完了条件 |
|---|---|---|
| 1 | 設定 `account_bound_thinking` (決定 1)。パターン照合、既定値、`check` の検証 | 試験で、書かない設定は `claude-sonnet-5-5` だけに当たり、書いた配列が既定を置き換え、`[]` はどれにも当たらない |
| 2 | `Binding` に開始 account と跨いだ印 (決定 4)。覚える契機と読み直しの引き継ぎ | 試験で、2xx の初回が開始 account になり、別 credential の経路で送ると印が立ち、同じ credential を指す別名の経路では立たず、読み直しで同一経路の Binding が残る |
| 3 | 設定 `on_account_switch`、変換 (決定 3) を送る直前に当てる。events の `thinking_as_text` / `thinking_dropped_by_switch` (決定 5) | 試験で、跨いだ session の本文の thinking が改行を空白に畳んだ本文だけの text に、redacted_thinking と空の thinking が消え、跨いでいない session と束縛外モデルの本文はバイト一致のまま |
| 4 | ロック (決定 2)。束縛モデルの候補を開始 account に絞り、全滅時は `on_account_switch` に従う | 試験で、ロック中の session が spend_down の昇格でも他 account へ行かず、開始 account が全部断った時に `stay` は 429 + `retry-after`、`drop_thinking` は切り替えて本文不変、`thinking_as_text` は切り替えて段 3 の変換が効く |
| 5 | 実機: account A で turn 1、B へ切替えた turn 2 で `input_tokens` が変換後の text 分だけ載ることと、別の会話でも置換形が refusal にならないこと | `docs/research/2026-09-30-preserved-thinking-and-account-switching.md` のマトリクスに行が足される |

## 関連

- docs/issue/2026-09-30-sonnet-5-5-thinking-is-account-bound-so-route-switches-drop-it.md (本 DR の元)
- docs/issue/2026-09-25-low-priority-slow-requests-should-wait-not-switch.md (`stay` が同 issue の「待つ」に相当)
- docs/research/2026-09-30-preserved-thinking-and-account-switching.md (一次資料と実測)
- DR-0009 (fail over の契機と affinity の鍵)
- DR-0016 (`thinking_display`。変換で残る本文が要約か空かを決める)
- DR-0018 / DR-0019 (spend_down の昇格と pace_cap。ロックがこれらより強くなる)
- DR-0024 (cache 戦略の「本文を変えない」。線引きは影響の節)
- DR-0027 (keepalive の自送信が控える本文)
- DR-0032 (読み直し時の走行状態の引き継ぎ規則)
