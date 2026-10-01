# DR-0035: thinking の跨ぎ (account / model) を session の出所 tuple で判定し、跨いだ session は thinking を text として送る

- Status: Accepted (kawaz 裁定 2026-10-01)。実装済
- Date: 2026-10-01

## 文脈

一次資料 ([Preserved thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking)) は、履歴に載って戻ってきた thinking block の署名を 3 種の束縛で検める。

| 束縛 | 中身 | Sonnet 5.5 | Fable 5.1 | Opus 5.5 |
|---|---|---|---|---|
| account 束縛 | 生成した account (または linked account) でしか効かない | あり | なし | なし |
| prefix 束縛 | block より前の `system` / `tools` / `messages` が生成時と一致していること | あり | あり | あり |
| model 束縛 | 読めるモデルが生成したモデルに限られる | あり | あり | あり |

束縛に外れた block は、API が黙って捨てたうえで 200 を返す (読めないモデルの block も 400 ではなく drop で、input として課金も計上もされない)。account 束縛は今は Sonnet 5.5 だけだが、資料は「newer Claude models の性質」「later checks add values」と書いており、他モデルに広がらない保証は無い。

gateway は DR-0009 の fail over (401/403/429/529/5xx)、pace_cap (DR-0019)、締め出し (denial) で別 credential に移る。affinity は「前回通った経路を先頭へ寄せる」優先であって固定ではなく、経路名を覚えるだけで account の同一性を知らない。クライアントの側も、同じ会話の途中でモデルを替えうる。どちらの跨ぎでも、推論の連続性はクライアントにも gateway にも何も見えないまま失われる。

実測 (`docs/research/2026-09-30-preserved-thinking-and-account-switching.md`):

| 跨ぎ | 結果 | `input_transformations` (beta `thinking-binding-controls-2026-08-01` 有) |
|---|---|---|
| Sonnet 5.5、account A → B | 200。`usage.input_tokens` は 401 (beta 有) / 409 (無) で、同じ本文を account A へ送った対照 517 / 520 より turn 1 の `thinking_tokens` (114 / 109) ぶん少ない = 落ちている | `[]` (報告なし) |
| Sonnet 5.5、同一 account で先行本文を改変 | 200、落ちる | `prefix_binding_mismatch` |
| Fable 5.1 → Opus 5.5 (同一 account) | 200。`input_tokens` は 611 で、thinking を削除した対照と同値 (thinking を残して Fable へ送った対照は 1021。差 410 は turn 1 の `thinking_tokens` 408 にほぼ一致) = 落ちている | `model_binding_mismatch` |
| Fable 5.1、別の account へ (同じモデル) | 200。`input_tokens` は同一 account の対照と同じ 1021 = 落ちていない | `[]` |

- Fable の account 跨ぎで落ちなかったのは、表の「account 束縛 なし」と整合する。ただし 2 account が linked でないことは確かめられていない (移った先の account には Sonnet 5.5 の経路が無く、account 束縛される Sonnet で非 linked を確かめられない)
- model 束縛は全モデルに効く。Fable → Opus の跨ぎで、Fable の thinking は黙って落ちた
- tool ループ中の Sonnet 5.5 を account B へ無改変で再送した時は `{"type":"thinking_dropped","reason":"end_user_binding_mismatch"}` が出た (資料に載る `organization_binding_mismatch` とは別の値で、text-only 履歴の再送では `[]` だった)。この報告が出る条件は確定していない

**gateway が自前で判断する必要がある。** account の不一致は header 有りでも報告されないことがあり、model の不一致は報告されても応答で初めて分かる。thinking を救う変換 (決定 5) は送る前に決めなければならないので、報告を判断の根拠にできない。

## 決定

### 1. 束縛と跨ぎの判定

履歴の thinking block を読めるのは、次の両方を満たす時だけとみなす。

- **生成したのと同じモデル** (全モデル)
- そのモデルが account 束縛なら、**生成したのと同じ account**

account 束縛のあるモデルは設定で持つ。「どのモデルの thinking が account に束縛されるか」は upstream の性質で、namespace ごとに変わるものではないので、最上位に 1 つ置く。

```toml
# 書かなければ ["claude-sonnet-5-5"]。書けば置き換わる (DR-0013 の配列の規則)。
account_bound_thinking = ["claude-sonnet-5-5", "claude-fable-5-*"]
```

- 要素は routing / cache の `models` と同じパターン。照合するのは解決後のモデル名 (alias を解いた後)
- 空配列 `[]` で無効にできる。他モデルに account 束縛が広がったら 1 行足す
- この設定が決めるのは account 束縛の対象だけ。model 束縛は設定を問わず全モデルに当てる

同一性の定義:

- **model の同一性**は、gateway が解決したモデル名の文字列そのまま (経路ごとの upstream 名への書き換えの前)。date 接尾辞の除去や family への正規化はしない。どのモデルの組が互いの block を読めるかの表は公開されておらず (資料が名指しするのは「Sonnet 5.5 の block を読むのは Sonnet 5.5 だけ」)、バージョン違いで落ちるかも分かっていないので、推測の規則を足さない
- **account の同一性**は credential 名。同じ credential を指す経路は同じ account、別の credential は別 account。credential を持たない経路 (relay) は経路名を account 名として扱う (中の account を gateway は知らない)。linked account は無い前提で、同一視の設定は持たない

経路 R へモデル M を送る時、session の出所 (決定 2) に次を満たす tuple `t` が 1 つでもあれば、その 1 本は **跨いでいる**。

```
t.model != M  ||  (M が account 束縛  &&  t.account != R の account)
```

出所は一度入ったら寿命まで消えないので、跨いだ session は以後ずっと跨いだまま (送り先が開始 account や元のモデルに戻っても、別の出所の thinking が履歴に残っている)。切替点の index は持たない。

### 2. 出所 tuple

session ごとに、履歴に thinking を持つ 1 本 (決定 3) が 2xx で通った `(account, model)` の集合を持つ。

| 欄 | 中身 |
|---|---|
| `session` / `account` / `model` | 鍵。session は session key (`session.rs` が metadata / header / 本文冒頭から導く)、account は決定 1 の credential 名、model は解決後のモデル名 |
| `decided_at` | この tuple が初めて 2xx で通った時刻 |
| `seen` | この session が最後に 2xx で通った時刻。寿命の起点 |

- 鍵に namespace を含めない。束縛は upstream の性質で namespace に依らないので、同じ session key が別の namespace を通っても 1 つの出所を共有する (跨いだ時の方針はその時の namespace のもの。決定 4)
- 「跨いだ」のような派生フラグは持たない。跨ぎは集合から導く (決定 1)
- **開始 account** = 同じモデルの tuple のうち `decided_at` が最小のものの account。account 束縛モデルの候補の並べ替え (決定 4) に使う。`decided_at` が同じなら account 名の小さい方 (どの unit でも同じ答えになるように)
- 同じ鍵について 2 つの言い分 (2 unit、またはメモリとファイル) があれば、`decided_at` は小さい方、`seen` は大きい方を取る。鍵に account を含むので、同じ session・同じモデルに別 account の 2xx が並行して届いても tuple が 2 つ並ぶだけで、開始 account は `decided_at` で決まり、遅い側は自動的に跨ぎになる (取り合いの規則が要らない)
- `seen` は session 単位で進める。session のどの tuple が通っても、その session の全 tuple の `seen` を進める
- 寿命は最後の `seen` から 24 時間。session の tuple は揃って寿命切れになる

### 3. 履歴に thinking を持つ要求だけが対象

同じ session id で別モデルが走る脇の呼び出し (権限判定の classifier・要約) を出所に混ぜると、本流が毎回「別モデルの出所がある」=「跨いだ」と判定され、`thinking_as_text` の session は本流の thinking まで text になる。affinity のようにモデルを session の鍵に含めれば脇の呼び出しは分かれるが、それではモデルの跨ぎが見えない (モデルの跨ぎを見るには出所を session で束ねる必要がある)。

そこで、出所への記録・跨ぎの判定・候補の絞り込み (決定 4) は **履歴に thinking を持つ 1 本**だけに当てる。

- 履歴に thinking を持つ = messages の assistant の content に `thinking` / `redacted_thinking` block がある
- thinking を使っていた session を、thinking を無効にしたモデルへ切り替えた 1 本も対象になる。履歴の thinking は跨ぎとして扱い、`thinking_as_text` なら text として文脈に載せる
- それ以外の 1 本は出所に記録せず、跨ぎを判定せず、本文にも触らない。脇の呼び出しは会話の履歴を持たないので、ここで外れる

限界:

- 会話の履歴ごと別モデルへ送る脇の呼び出し (fork 等) は、履歴に thinking があれば対象になり、出所に別モデルの tuple を足す (以後は本流も跨いだと判定される)

### 4. `on_thinking_crossing`

跨いだ 1 本と、account 束縛モデルの開始 account の経路が全部使えない時の振る舞いを選ぶ。

置き場は namespace (`[ns.<名前>]`)。跨いだ時に推論の連続性と可用性のどちらを取るか、本文を変えてよいかは upstream の性質ではなく使う側の方針で、`thinking_display` (DR-0016) と同じ層に属する。upstream の性質である `account_bound_thinking` (決定 1) が最上位だけに置かれるのとは分ける。最上位にも書けて、そちらは各 namespace の既定になる。解決順は **その namespace の値 → 最上位の値 → `drop_thinking`**。

```toml
on_thinking_crossing = "drop_thinking"  # 最上位: 各 namespace の既定。"stay" | "drop_thinking" | "thinking_as_text"

[ns.personal]
on_thinking_crossing = "thinking_as_text"  # この namespace だけ上書き
```

出所 (決定 2) は namespace を跨いで共有するが、判定と振る舞いは今の 1 本が通る namespace の方針で決める。同じ session が別の namespace を通れば、出所は同じまま、その時の namespace の `on_thinking_crossing` に従う。

設定名は account の跨ぎだけでなく model の跨ぎも指すので、`on_account_switch` と書いた設定は設定エラーにする。

| 値 | 振る舞い |
|---|---|
| `stay` | account 束縛モデルで開始 account がある session は、候補を開始 account の経路だけに絞る。全部使えなければ、全経路が締め出された時の既存の応答と同じ形の 429 + `retry-after` を返す。model の跨ぎはクライアントが選んだものなので止めない (本文はそのまま送る) |
| `drop_thinking` (既定) | 開始 account の経路が全部使えなければ DR-0009 のまま別 account へ移る。跨いでも本文は変えず、API が束縛外の thinking を捨てるのに任せる |
| `thinking_as_text` | 開始 account の経路が全部使えなければ別 account へ移る。跨いだ 1 本は決定 5 の変換を当てて送る (出所は消えないので、跨いだ session は以後の全リクエストで変換される) |

送る経路ごとの判定:

- 跨いでいれば、`stay` / `drop_thinking` は「履歴の thinking が捨てられる見込み」の印を出し (決定 7)、`thinking_as_text` は変換する
- 跨いでいなければ何もしない
- 候補のうち 1 本目は跨がず 2 本目は跨ぐ、がありうるので、判定は経路を試すたびに行う

候補の並べ替え: account 束縛モデルで開始 account がある session は、開始 account の経路を候補の先頭へまとめる (`stay` ではそれ以外を外す)。affinity と spend_down の繰り上げ (DR-0018) の後に効かせ、繰り上げもこれを越えない。他の session の affinity は今のまま「優先」。

既定を `drop_thinking` にするのは、本文を変えない側だから。`stay` は `docs/issue/2026-09-25-low-priority-slow-requests-should-wait-not-switch.md` の「待つ」に相当する。

### 5. text 変換

跨いだ 1 本 (`thinking_as_text`) は、送る直前の本文の履歴を次のように変換する。

- `thinking` block → `{"type": "text", "text": <本文>}`。本文は thinking 本文から末尾の改行 (`\n`) と半角空白だけを除いたもの。内部の改行 (`\n` / `\n\n`) はバイトのまま残す (下流の webui が `\n\n` 区切りで翻訳を並列化する)。タブ等の他の空白も残す。prefix や装飾は付けない。署名は捨てる
- `redacted_thinking` block と、本文が空の `thinking` block → 落とす (運ぶ本文が無い)
- 元にするのはクライアントが送ってきた本文そのまま。`thinking_display = "summarized"` (DR-0016) の namespace なら要約が入り、`omitted` なら空なので落ちる
- 位置は元の block と同じ。他の block (text / tool_use / tool_result) には触らない
- 元の block に `cache_control` が付いていたら、置き換えた text block に移す
- 決定的に変換する (同じ入力から同じ本文)。それでも初回の変換で prefix が変わるので、その session の prompt cache は 1 度壊れる。前提として受け入れる
- 跨いだ後に新しい出所 (別 account / 別モデル) が生成した thinking も、次のリクエストでは同じく text になる。その session は preserved thinking の恩恵を諦め、推論は「読める独り言」として残る
- 変換は cache 戦略 (DR-0024) より前に当てる (変換後の本文に `cache_control` の整えを当てる)

この形の根拠は実測 (research doc の「tool ループ中の置換」「置換の形 (account B)」「fable の account 跨ぎと fable→opus のモデル跨ぎ」節):

- Sonnet 5.5 の account 跨ぎで、本文だけ (3/3) と改行を半角空白にした本文だけ (3/3) は `end_turn` で text の応答が返った。tool ループの途中 (直前の assistant turn が `tool_use` を持つ) で置換しても 400 にならなかった
- 見出し (`THINKING:\n` 3/3、`(previous reasoning)\n`)、`🧠` / `💬` の接頭辞、括弧囲み、user turn の text への移動は、すべて `stop_reason: refusal` / `stop_details.category: reasoning_extraction` になった
- Fable 5.1 → Opus 5.5 で、本文 (末尾の改行と半角空白を除去) の text に置換すると、`input_transformations: []`・`end_turn` で回答し、refusal は出なかった。`input_tokens` は削除時の 611 に要約本文の 148 を足した 759 で、運べるのは送られてきた本文 (この測定では要約) までで、署名が運んでいた推論全体 (約 410) は載らない

### 6. 永続化

出所は `[stats] dir` の共有ファイルに置き、restart と unit (stable / unstable) を跨いで共有する。

- 置き場は `[stats] dir` 配下の `thinking-sources/` に `sources.json` と `sources.json.lock`。`[stats] dir` は keepalive の控え (DR-0027) と同じく兄弟の unit が共有する走行状態の置き場で、既定は `$XDG_STATE_HOME/llm-gateway/stats`。`[store] dir` は credential の置き場で、credential 以外を混ぜない
- 形式は **1 ファイル 1 map** (JSON オブジェクト、鍵ごとに 1 レコード `{session, account, model, decided_at, seen}`)。書き込みは `write_atomically` (一時ファイル + fsync + rename) で丸ごと差し替える。jsonl の追記は採らない (同じ鍵の更新が行として溜まり、読むたびに畳む処理と切り詰めが要る)
- 書き換えは DR-0010 と同じ形: `.lock` を flock で掴む → 最新を読み直す → 自分の変更を当てる (決定 2 の `decided_at` min / `seen` max) → 書く → 手放す。`.lock` は消さない (本体は rename で inode が変わるので、脇のファイルを掴む)。flock を持つ区間に送信や upstream の待ちを含めない
- 両 unit は同一ホストの時計を使う前提で、`decided_at` の比較は unit 間の時計ずれを考えない
- **読み** (リクエストごと): メモリの控えを引く。控えにはファイルの版 (mtime のナノ秒・inode・長さ) を添え、引く前に版を見て変わっていれば読み直す。1 本あたりの仕事は `stat` 1 回
- **書き** (履歴に thinking を持つ 1 本の 2xx): 新しい tuple が入った時と、session の `seen` が前に書いた値から 5 分以上進んだ時だけ書く (寿命 24 時間に対して最大 5 分の誤差)。書き込みは session 数に比例し、リクエスト数には比例しない
- メモリの控えはその場で更新し、ファイルへの書き込みは unit ごとに 1 本の書き手が blocking の仕事として裏で順に流す。同じ unit の次の 1 本はメモリの控えを見るので、書き込みの完了を待たない
- 失敗は best-effort: 読めなければメモリの控えのまま進み、書けなければ警告を残して進む (次に書く条件に当たった 2xx で書き直す)。出所は推論の連続性を守る仕組みで、置き場の障害で転送を止めるほど重くない
- 刈り込みは書き込む側が flock の内側で行う (寿命切れのレコードを落としてから書く)。担当 unit は決めない。起動時は読み取りだけで刈らず、寿命切れは読み手が無いものとして扱う
- 起動時 (`Router` の組み立て時) にファイルを読んでメモリの控えに載せる。reload (DR-0032) は出所の表に触らない (出所は経路ではなく account とモデルに結ばれていて、経路表を差し替えても意味が変わらない)
- affinity (前回通った経路名の優先) は永続化しない。鍵 `(namespace, session, model)`・寿命 1 時間・メモリのまま。経路名の優先は prompt cache の都合で、失っても設定順に落ちるだけで正しさは壊れない
- `[stats] dir` 配下の `account-lock/locks.json` は読まず、移行もしない。鍵と値の形が違うので新しい置き場を切る。`account-lock/` は gateway が掃除しない

### 7. events の印

- `thinking_as_text` で変換した 1 本は、events の `request` に `thinking_as_text: true`
- `stay` / `drop_thinking` で跨いだまま本文を変えずに送った 1 本は `thinking_dropped_by_switch: true` (履歴の thinking が API に捨てられる見込みの印。実際に捨てられたかは gateway には見えない)
- 当てはまらない 1 本では欄ごと出さない (`skipped` / `cache_ttl_secs` と同じ流儀)
- stats (DR-0011 / DR-0029) には軸を足さない。変換は session の状態であってトークンの行き先ではなく、どの session が何本変換されたかは events で追える
- `status` / `daemon status` に出所の件数などは載せない。見たければ `thinking-sources/sources.json` を読めば足りる。置き場の読み書きの失敗は tracing の警告に出す (他の store 品目と同じ)

## 却下した案

- **切替時に drop を観測・記録するだけ**: 推論は救えない。しかも account の不一致は header 有りでも報告されないことがあり、観測そのものが成立しない
- **`input_tokens` の差で事後に検知する**: 同じく推論を救えず、対照 (同一 account・同一モデルで同じ本文を送った値) が手元に無いので差を取れない
- **切替点より前の thinking だけ text にする**: account A → B → A と戻ると、B が生成した block が A で黙って落ちる。防ぐには block ごとに生成元を覚える segment 管理が要り、状態が session 単位から block 単位に膨らむ
- **thinking を単純に剥がして切り替える**: 推論の痕跡が消える。text に置けば署名と束縛は無くなっても内容はモデルに届く (剥がす挙動が欲しい運用は `drop_thinking` で API の drop に任せれば足りる)
- **ラベル付き置換** (`THINKING:\n` 等の見出し・絵文字・括弧を付ける、user turn へ移す): 実測で全部 `reasoning_extraction` の refusal になる
- **鍵を `(namespace, session, model)`・値を開始 account のまま、model の跨ぎを別の機構で足す**: model の跨ぎは session をモデルを越えて束ねないと見えない。鍵にモデルを含めたままでは、モデルごとの表を横断して引く別の仕組みが要り、状態が 2 つに割れる
- **「跨いだ」フラグを持ち続ける**: 出所の集合から導けるので、状態が二重になる (どちらが正かを合わせる規則が要る)。集合なら 2 unit の言い分も `decided_at` min / `seen` max で当てるだけで済む
- **モデル名を date 接尾辞の除去などで family にまとめる**: バージョン違いの block が読めるかは未知数で、まとめた結果「読める」とみなして素通しすると、落ちた時に何も見えない
- **全リクエストを出所の対象にする**: 同じ session で別モデルが走る脇の呼び出しが本流を跨ぎ扱いにし、`thinking_as_text` の session では本流の thinking まで text になる (決定 3)
- **affinity ごと永続化する**: 経路名の優先は 1 時間で捨ててよい状態で、永続化すると 2xx ごとに書くことになり、reload の引き継ぎ規則 (経路が変わったら捨てる) もファイル側に持ち込むことになる
- **affinity の寿命を延ばすだけ / 出所の寿命だけメモリで延ばす**: 沈黙は塞げるが、restart と unit の移動は塞げない
- **Caddy 等の前段の sticky で session を 1 unit に寄せる**: unit の移動は減るが restart は塞げず、session key (gateway が本文や header から導く) を前段が知らない。session の状態は gateway の責務
- **unit (writer) ごとのファイルに書いて読む時に merge する (DR-0031 (3) の形)**: 読むたびに全 writer のファイルを開いて merge することになり、判定の場所が読み手の数だけ増える。1 ファイルを flock の内側で更新すれば判定は 1 箇所で閉じる

## 未確定

- Store 層の interface を切るタイミング: issue `2026-09-15-store-layer-for-replaceable-persistence` / DR-0031 の進み次第。第一版はファイルを直に読み書きする実装を router の外の 1 モジュールに置き、interface は後から切る。DR-0031 の 4 意味論のどれに載せるか (「掴む → 読み直す → 書く」は (1) の形だが、失敗の扱いは best-effort で (1) の fail-closed と違う) もその時に決める

## 影響

### 実物照合

| 箇所 | この DR で要ること |
|---|---|
| `thinking_sources` モジュール (`account_lock` を置き換える) | 出所の表: 3 値の鍵、`decided_at` / `seen` の当て方、版の照合つきの控え、flock の内側での書き換えと刈り込み、裏の書き手 (決定 2・6) |
| `Router` の出所の口 | 出所を引く口と、履歴に thinking を持つ 1 本の 2xx で記録する口。候補の並べ替えで account 束縛モデルの開始 account の経路を先頭へまとめ、`stay` では他を外す (決定 4)。reload は出所の表に触らない。affinity は `(namespace, session, model)` のメモリのまま |
| `gateway.rs` の `Call` と送る直前の本文 | 送る経路ごとに跨ぎを判定し (出所 + 送り先の account + モデル、方針は今の 1 本が通る namespace の `on_thinking_crossing`)、`thinking_as_text` なら変換、`stay` / `drop_thinking` なら印。変換は経路ごとのモデル名の書き換えと cache 戦略の前 (決定 5) |
| `thinking::carries_thinking` | 決定 3 の「履歴に thinking を持つ 1 本」の判定 (assistant の content の block だけを見る) |
| 設定 | `on_thinking_crossing` は namespace と最上位 (各 namespace の既定) に置き、ns → 最上位 → `drop_thinking` の順に解決する (決定 4 の設定エラーを含む)。`account_bound_thinking` は最上位のまま据え置き。`check` で同じ検証 |
| events | `thinking_as_text` / `thinking_dropped_by_switch` の欄 (決定 7) |
| OpenAI 経路への変換 | 履歴の thinking / redacted_thinking を落とす変換は変えない。OpenAI 経路はモデルも credential も別なので、履歴に thinking を持つ session がそこへ送れば跨ぎになり、`thinking_as_text` なら落とされる前に text になって内容が届く |

### DR-0024 の「本文を変えない」との線引き

DR-0024 は cache 戦略が触るのを `cache_control` だけに限り、「thinking / system / tools / messages の中身には触らない」と定めた。理由は prefix 束縛 (本文を変えると過去の署名が無効になる)。本 DR の変換は gateway が messages の中身を変える処理になる。線引き:

- DR-0024 の規定は cache 戦略の規定として正しい。cache 戦略は引き続き `cache_control` 以外に触らない
- 本文の中身を変えてよいのは、**署名がもう効かないと gateway が知っている 1 本だけ** (跨いだ 1 本)。跨いだ後の署名は account 束縛か model 束縛でどのみち捨てられるので、prefix 束縛を壊す損は無い

### keepalive の自送信 (DR-0027)

keepalive は最後に転送した本文を控えて送り直す。控えるのは変換後の本文 (変換前を控えると、跨いだ送り先へ送り直した時に束縛で落ちる本文を送る)。

### 既知の端

- **変換で content が空になる assistant turn**: `thinking` / `redacted_thinking` だけで text も tool_use も持たない assistant turn は、変換後に content が `[]` になる (実運用の assistant turn は text か tool_use で終わるので、ほぼ起きない)。起きた場合、API はその 1 本を 400 で断りうる
- **24 時間より長く黙った session**: 寿命は session の最後の 2xx から 24 時間で、それより長く黙った session は出所を忘れる。次の 1 本で出所を作り直すので、そこで別 account / 別モデルに移れば、以前の thinking は黙って落ちる
- **履歴ごと別モデルへ送る脇の呼び出し** (fork 等): 決定 3 の限界。履歴に thinking があれば出所に別モデルの tuple を足し、本流を跨ぎ扱いにする
- **本文冒頭のハッシュに落ちる session key**: metadata / header で会話を名乗らないクライアントは、system と最初の user 本文のハッシュが session key になる。冒頭が同じ別の会話は出所を共有し、モデルが違えば互いを跨ぎ扱いにする
- **credential 名を設定で付け替えた時**: 旧名の tuple は新しい設定のどの経路とも一致しないので、account 束縛モデルの次の 1 本は跨ぎ扱いになる。実害は「跨いだ扱いが 1 度増える」だけと見て、特別な扱いは置かない

## 関連

- docs/issue/2026-09-30-sonnet-5-5-thinking-is-account-bound-so-route-switches-drop-it.md (account 束縛の発端)
- docs/issue/2026-09-30-persist-session-account-lock-across-restart-and-units.md (restart と unit 間の共有の発端)
- docs/issue/2026-09-25-low-priority-slow-requests-should-wait-not-switch.md (`stay` が同 issue の「待つ」に相当)
- docs/issue/2026-09-15-store-layer-for-replaceable-persistence.md (Store 層の品目として)
- docs/research/2026-09-30-preserved-thinking-and-account-switching.md (一次資料と実測)
- [DR-0009](DR-0009-credential-denial-fallback.md) (fail over の契機と affinity)
- [DR-0010](DR-0010-credential-cross-process-lock.md) (`.lock` の flock と mtime の版)
- [DR-0013](DR-0013-config-extends.md) (配列は置換)
- [DR-0016](DR-0016-ns-thinking-display-override.md) (`thinking_display`。変換で残る本文が要約か空かを決める。`on_thinking_crossing` を namespace に置くのも同じ層だから)
- [DR-0018](DR-0018-spend-down-priority.md) / [DR-0019](DR-0019-pace-cap.md) (spend_down の繰り上げと pace_cap。開始 account の並べ替えがこれらより強い)
- [DR-0024](DR-0024-cache-strategy-and-keepalive.md) (cache 戦略の「本文を変えない」。線引きは影響の節)
- [DR-0027](DR-0027-keepalive-by-replay.md) (keepalive の自送信が控える本文、`[stats] dir` を兄弟の unit が共有する先例)
- [DR-0031](DR-0031-store-layer.md) (Store 層の意味論。載せる trait は未確定)
- [DR-0032](DR-0032-daemon-reload.md) (reload の走行状態の引き継ぎ)
