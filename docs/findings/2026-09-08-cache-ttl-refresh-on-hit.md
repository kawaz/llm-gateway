# キャッシュヒットは TTL を更新する (5m / 1h とも)

「30K の cache A があるとき、A+b を投げれば A の TTL は延びるのか、A+b の新エントリが
できるだけなのか」を、5 分 TTL と 1 時間 TTL の合成 probe で実測した。

## 判明した事実

- **プレフィックスにヒットしたリクエストは、そのエントリの TTL を更新する。5m でも 1h でも同じ**。
  5m は 3 分間隔のヒットで最初の書き込みから **15.5 分後も read 100% / write 0**、
  1h は 50 分後の 1 回のヒットで **70 分後も read 100% / write 0**。書き込みはいずれも最初の 1 回だけ
- 同時刻に作った対照エントリ (以後何も投げない) は、5m 系統が 6 分後、1h 系統が 70 分後に
  失効して全量 write。同じ probe 形・同じサイズなので、上の延命が「たまたま TTL が
  長かった」ではないことの裏取りになる
- **1h エントリに 5m フラグ (`ttl` 指定なし) で当てても TTL は 1h として更新される**。
  5m へ降格しない (t+50.5 に 5m フラグでヒット → 20.5 分後の t+71 でも read 100%)。
  この当て方は write 課金ゼロなので、**延命 ping は 5m フラグで送っても 1h の寿命を維持できる**
  (knowledge の「5m フラグは 1h エントリも読む」「5m 済みに 1h を当てても昇格しない」と整合。
  昇格はしないが、既に 1h のものは 5m フラグのヒットでも 1h のまま延びる)
- 延命に新しい内容は要らない。**差分ゼロの同一本文 replay でもヒットは成立し、TTL は更新される**
  (5m 系統 t+4 の replay は read 57836 / create 0)。新エントリは生まれない
  (knowledge の「差分ゼロの replay では新エントリは生まれない」と整合)
- 延命リクエストの課金はヒット分の read のみ (0.1 倍、5.1 系は 0.025 倍)。write は発生しない。
  usage 上、read は `cache_read_input_tokens` の 1 本で **5m 由来か 1h 由来かは区別されない**
  (`cache_creation.ephemeral_{5m,1h}_input_tokens` は write 側だけの内訳)
- **`max_tokens` は cache key に影響しない**。本文同一で `max_tokens` だけ 1024 → 1 に変えた
  再送は全量 read。応答は `output_tokens: 1` / `stop_reason: max_tokens` で終わり、
  非 stream の所要は 30K プレフィックスで **約 1.0 秒** (通常応答は 2.1〜2.4 秒)
- **`thinking` パラメータを外しても messages 側の断点は無効にならなかった**。
  `{"type":"adaptive","display":"omitted"}` 付きで書いた本文から thinking を削り
  `max_tokens: 1` で再送しても、system / messages 両方の断点を含む全量が read
  (公式仕様の「thinking の変更は messages 以降を無効化」は今回の probe では再現せず。
  ただし応答に thinking ブロックが出た本文では未検証 — 下記「再送本文の変形」の留保参照)
- **adaptive thinking を付けたままでも `max_tokens: 1` は通る**。旧形式
  (`{"type":"enabled","budget_tokens":1024}`) でも `max_tokens` を budget 未満にして 200 が返る
  (この形式は sonnet-5 では効いていないとみられる、下記参照)

## 実用的な示唆 / ベストプラクティス

- **keepalive はセッションを起こさなくても成立する**。gateway が「そのセッションで
  最後に送ったリクエストと同一本文」を Claude Code の形で自送信するだけで、
  read 課金だけで TTL が延びる。DR-0024 の marker 注入方式 (ccmsg 経由で
  `LLMGW-KEEPALIVE-<nonce>` を差し込み、セッションに 1 行返させる) の代替候補になる
- 自送信方式の利点: セッションを起こさない (webui のノイズ・ユーザ体感への干渉ゼロ)、
  ccmsg への依存が消える、応答待ちが 1 リクエストで完結する
- 自送信方式の制約 (採否の判断材料):
  - gateway が最後のリクエスト本文を保持する必要がある (現状は保持していない)。
    本文は会話内容そのものなのでメモリ常駐・永続化の扱いを設計する必要がある
  - 応答トークンが発生する (max_tokens を絞れば数十トークン)。DR-0024 の marker 方式でも
    1 行応答は出るので差は小さい
  - 1h エントリでも成立することは実測済み (下記「1h の場合」)。現行運用 (常時 1h) の
    まま自送信 keepalive に置き換えられる

### 再送本文をどこまで削れるか (DR-0027 の未確定への回答)

- **`max_tokens` は 1 にしてよい**。cache key は tools / system / messages なので、
  `max_tokens` を変えても全量ヒットする。応答は 1 トークンで打ち切られ、
  非 stream の所要は約 1.0 秒。ping 1 本の出力課金は実質ゼロに落とせる
- **`thinking` を外す必要はない**。adaptive thinking を付けたままでも `max_tokens: 1` が
  通るので、DR-0027 が想定した「thinking があると max_tokens を絞れない」制約は
  現行の Claude Code の本文 (adaptive) では発生しない。**本文をそのまま送り
  `max_tokens` だけ 1 にする**のが、加工を最小にしつつ出力を最小化する形
- 仮に外したとしても、実測では messages 側の断点は無効にならず全量 read だった。
  ただしこれは公式仕様と食い違うので、**外す方には寄りかからない**方がよい
  (そのまま送れば済むので、外す動機自体がない)

### 「常時 1h + 55 分ごと ping」vs「常時 5m + 4 分ごと ping」の費用

prefix P トークン、基本 input 単価を w (USD/token)、read 単価を r (= 0.1w、5.1 系は 0.025w)、
write 倍率を k (5m は 1.25、1h は 2.0)、ping 間隔を I 分、維持したい idle 時間を H 時間とすると、
1 回の idle 区間を維持する総コストは

    C(H) = P·w·k + (60H / I)·P·r      (初回 write 1 回 + ping ごとの read)

- 常時 1h・I = 55: `C_1h(H) = P·w·(2.0 + 1.091·H·(r/w))`
- 常時 5m・I = 4:  `C_5m(H) = P·w·(1.25 + 15·H·(r/w))`

分岐点は `H* = 0.75 / ((15 − 1.091)·(r/w))`。`r/w` は全モデル共通で 0.1
(Fable/Mythos 5.1 は 0.025) なので

| r/w | H* (これより長い idle は 1h が安い) |
|---|---|
| 0.1 (通常) | 約 0.54 時間 (32 分) |
| 0.025 (5.1 系) | 約 2.16 時間 |

P に依存しない (両辺に P·w が共通因子として乗るため) のは DR-0024 の損益分岐と同じ構造。
1 日 (H = 24) 連続 idle させた場合の係数は、通常モデルで 1h 側 4.6·P·w に対し 5m 側 37.3·P·w、
5.1 系で 2.65·P·w 対 10.25·P·w。数値の裁定 (I の実値・horizon との組み合わせ) は統括に委ねる。
- probe 形は `docs/findings/2026-09-03-prompt-cache-investigation.md` の最小形どおりで 200 が返る。
  system[0] の `cc_entrypoint=sdk-cli;` にしておくと gateway 側で origin=oneshot と判定され、
  ns=bare の keepalive 戦略に巻き込まれず passthrough (5m) のまま実験できる
  (tap の `cache_strategy: passthrough` / `cache_ttl_secs: 300` で確認)

## 検証の詳細

### 条件

- 経路: `http://127.0.0.1:11301/ns-bare/v1/messages`、model `claude-sonnet-5`、
  route `claude-zunsystem`、非 stream、`max_tokens: 64`
- 雛形: tap (`?include=request_body`) で捕った `claude -p --model claude-sonnet-5` の
  実リクエスト。tools (22 個) と system[0] (billing header) / system[1] (Agent SDK 文) は
  そのまま流用し、**system[2] だけを probe 用の固定文に差し替え**て末尾に
  `cache_control: {type: ephemeral}` を置いた。TTL 指定なし = 5m
- 固定文は「caching proxy の説明段落」を 170 回繰り返した公開可能な英文 (≈30K トークン)。
  A (main) と A' (control) は先頭 1 行の変種名だけが異なる別プレフィックス
- user メッセージには断点を置いていないので、キャッシュ対象は system[2] 末尾まで。
  各リクエストの user 本文 (b0…b5 / c0, c1) は毎回違う短文
- read の内訳: 27725 = tools + system[0..1] (Claude Code 側で既に温まっていた共通部分)、
  30111 = probe の固定文。完全ヒット時は合計 57836

### タイムライン (UTC、`date` 基準)

| t (分) | 時刻 | 対象 | read | create (5m) | 判定 |
|---|---|---|---|---|---|
| 0 | 09:05:41 | A 書き込み (b0) | 27725 | 30111 | A の 5m エントリ生成 |
| 0.05 | 09:05:47 | A' 書き込み (c0、対照) | 27725 | 30111 | A' の 5m エントリ生成 |
| 1 | 09:06:41 | A + b1 | 57836 | 0 | 完全ヒット |
| 4 | 09:09:42 | A + b1 の**同一本文 replay** | 57836 | 0 | ヒット、新エントリなし |
| 6 | 09:11:41 | **A' + c1 (対照)** | 27725 | **30111** | **6 分放置で失効 → 全量 write** |
| 7 | 09:12:41 | A + b2 (判定) | 57836 | 0 | **書き込みから 7 分後もヒット** |
| 10 | 09:15:42 | A + b3 | 57836 | 0 | ヒット |
| 13 | 09:18:42 | A + b4 | 57836 | 0 | ヒット |
| 15.5 | 09:21:12 | A + b5 (長期判定) | 57836 | 0 | **書き込みから 15.5 分後もヒット** |

`cache_creation.ephemeral_1h_input_tokens` は全リクエストで 0 (1h エントリは一切使っていない)。

### 判定ロジック

t+7 の A は、最初の書き込み (t+0) から 7 分経過している。ヒットで TTL が更新されないなら
t+5 で失効して write になるはずだが read だった。同型・同時刻生成の A' が t+6 で write に
なっていることから、「7 分でも生きていた」のは TTL の個体差ではなく **t+1 / t+4 のヒットが
TTL を押し上げた**結果と確定する。t+10 / t+13 / t+15.5 は 3 分間隔のヒットで無限に
延命できることの確認。

公式 doc の「ヒットのたびに TTL は無料で更新される」(knowledge に転記済み) が
実機で裏付けられた形。

### 1h の場合

条件は 5m と同じ (同じ雛形・同じ 170 段落の固定文・sonnet)。差分は 2 点:

- `cache_control` を `{"type":"ephemeral","ttl":"1h"}` にし、`anthropic-beta` に
  `extended-cache-ttl-2025-04-11` を追加
- **雛形 system[1] に付いている 5m 断点を落とした**。1h ブロックは 5m ブロックより前に
  置く必要があり、そのままだと 400
  (`a ttl='1h' cache_control block must not come after a ttl='5m' cache_control block`)

3 系統を同時刻に 1h で書き、当て方だけを変えた:

| t (分) | 時刻 | 対象 | フラグ | read | create | 内訳 |
|---|---|---|---|---|---|---|
| 0 | 09:24:50 | A 書き込み | 1h | 27725 | 30111 | 1h 30111 |
| 0.1 | 09:24:58 | B 書き込み (対照、以後放置) | 1h | 27725 | 30110 | 1h 30110 |
| 0.2 | 09:25:02 | C 書き込み | 1h | 27725 | 30113 | 1h 30113 |
| 50 | 10:14:50 | A + a1 | 1h | 57836 | 0 | — |
| 50.5 | 10:15:23 | C + c1 | **5m** | 57838 | 0 | — |
| 70 | 10:34:53 | **A + a2 (判定)** | 1h | **57836** | **0** | 1h ヒットで延命 |
| 70.5 | 10:35:21 | **B + b1 (対照)** | 1h | 27725 | **30110** | **70 分放置で失効** |
| 71 | 10:35:53 | **C + c2 (判定)** | 1h | **57838** | **0** | 5m フラグのヒットでも延命 |

判定ロジックは 5m と同型。B が 70 分で失効しているので、A / C が 70 分超で生きていたのは
t+50 前後のヒットが TTL を更新した結果。C はヒット (t+50.5) から 20.5 分後の判定なので、
5m フラグで当てた場合に TTL が 5 分相当に短縮されていれば失効しているはずだが read だった
= 5m フラグで当てても 1h の寿命で更新される。

課金区分の記録: 1h の書き込みは `cache_creation.ephemeral_1h_input_tokens` に全量が計上され、
`ephemeral_5m_input_tokens` は 0。ヒット時は `cache_read_input_tokens` に出るだけで、
5m 由来か 1h 由来かの内訳は usage からは分からない。

なお read の 27725 は tools + system[0..1] (Claude Code 側で温まっている共通部分)、
30111 前後が probe の固定文。完全ヒット時の 57836 / 57838 は両者の合計で、末尾 3 桁の
違いは変種名の文字数差による。

### 再送本文の変形

5m TTL、sonnet、`output_config: {effort: "low"}`。断点は 2 個 — system[2] (固定文 120 段落)
と messages 末尾 user (固定文 60 段落 + 質問) — で、messages 側の無効化が
`cache_creation` に現れるようにした。write 時の内訳は read 27725 (共通部分) +
create 31980 (system[2] + user 本文)、完全ヒット時は read 59705。

| 系統 | 書き込み | 1 分後の再送 | read | create | out | 所要 |
|---|---|---|---|---|---|---|
| E1 | mt=1024、thinking なし | 本文同一・**mt=1** | 59705 | 0 | 1 | 1.0s |
| F | mt=2048、**adaptive** | 本文同一 (mt=2048) | 59705 | 0 | 73 | 2.1s |
| G | mt=2048、**adaptive** | **thinking 削除**・mt=1 | 59705 | 0 | 1 | 0.9s |
| H | mt=2048、**adaptive** | adaptive 維持・**mt=1** | 59705 | 0 | 1 | 0.9s |

- E1: `max_tokens` は cache key に含まれない。応答は 1 トークンで打ち切られ、
  通常応答の 2.1〜2.4 秒に対し 1.0 秒で返る
- H: **adaptive thinking を付けたまま `max_tokens: 1` が通る** (400 にならない)。
  DR-0027 が懸念した「`max_tokens` を `budget_tokens` 未満に絞れない」は、
  `budget_tokens` を持たない adaptive 形式には当てはまらない
- G: thinking を外しても messages 側の断点は生きていた (全量 read)。公式仕様の
  「thinking パラメータの変更は messages 以降を無効化する」は再現しなかった

G の留保: 今回の probe は **応答に thinking ブロックが 1 度も出なかった**
(`blocks=['text']` のみ)。`{"type":"enabled","budget_tokens":1024}` の旧形式でも、
effort high + 数学の証明問題でも、`claude -p` の実リクエスト (tap で response_body を
確認) でも出ない。sonnet-5 の adaptive は「思考しない」判断を返せるので、
**思考が実際に発火した本文での再検証は済んでいない**。keepalive は本文をそのまま
送れば済む以上、この方向に依存する必要はない。

旧形式についての付随観測: `{"type":"enabled","budget_tokens":1024}` に対して
`max_tokens: 512` (budget 未満) を送っても 400 にならず 200 が返る。公式仕様では
拒否されるはずなので、sonnet-5 ではこの形式自体が無視されているとみられる
(gateway は ns の `thinking_display` に従い `display` を注入するだけで、
type / budget_tokens には触っていない)。
