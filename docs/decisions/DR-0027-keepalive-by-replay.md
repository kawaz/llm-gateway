# DR-0027: keepalive は gateway の自送信 (replay) で行う

- Status: Proposed
- Date: 2026-09-08

## 文脈

DR-0024 §2 の keepalive は、cache を繋ぐために**会話そのものを 1 往復走らせる**作りに
なっている。gateway は合図 (nonce 入りの marker) を webhook へ流し、ccmsg がそれを
セッションへ注入し、起こされたセッションが 1 行返す — その 1 本が cache を延ばす。
「TTL を延ばすには新しいリクエストが要り、リクエストを作れるのはセッションだけ」という
前提の上に立っている。

その前提が今日の実測で崩れた (`docs/findings/2026-09-08-cache-ttl-refresh-on-hit.md`):

- **プレフィックスにヒットしたリクエストは、そのエントリの TTL を更新する。5m でも 1h でも
  同じ**。1h エントリは t+50 の 1 回のヒットで t+70 でも read 100% / write 0、同時刻に
  作った対照エントリは t+70 で失効して全量 write
- **差分ゼロの同一本文 replay でもヒットは成立し、TTL は更新される**。新エントリは生まれず、
  課金はヒット分の read だけ (write ゼロ)

つまり cache を延ばすのに**新しい内容も、セッションも要らない**。gateway が最後に転送した
本文をそのまま送り直せば足りる。DR-0024 §2 の複雑さは、そのほぼ全てが「他人 (セッション)
にリクエストを作らせる」ことから派生している:

- **セッションを起こす**。webui に ping が並び、TUI に marker と応答が出る。合言葉だけを
  返させる常時ロードのルールを配って遵守率 98% まで詰めたが、ノイズはゼロにならない
  (`docs/findings/2026-09-08-keepalive-field-observation.md`: 導入初期の遵守率 32.6%)
- **届け先が要る**。webhook の宛先を書いていない namespace では合図が出ず (`1h` と同じ
  振る舞いに落ちる)、`llm-gateway check` の警告に頼る
- **合図が戻るとは限らない**。`applied` / `late` / `foreign` / 戻らない、の 4 通りを
  観測して 1 本へ収束させる規則、合言葉に起点と終わりを埋めて期間を持ち歩かせる仕掛け、
  停止を兄弟へ回す peers の経路 — DR-0024 の追補はいずれもこの不確実性の後始末で、
  期間の取り直しによる連鎖 (5.5 時間の期間へ 19 本 ≒ 16.5 時間の合図) のような
  bug の温床になっている
- **再起動で切れる**。見張りは置き場に落としているが、合図の相手 (セッション) が
  生きているかは gateway から分からない
- **sub に使えない**。`sub = "keepalive"` は設定エラー。subagent は合図を返す相手として
  当てにできないため

自送信ならこれらは全て消える。相手はセッションではなく upstream で、1 リクエストで完結する。

## 決定

### 1. 合図方式を廃し、自送信 (replay) に置き換える

gateway は系列 (DR-0012 の `prefix` + `session_id`) ごとに「最後に upstream へ転送した
本文」を保持し、idle 55 分ごとにそれを**そのまま**再送する。認証の差し替えとモデル名の
書き換えを済ませた後の形 — つまり実際に線に乗った bytes — をそのまま出す。加工しないので
プレフィックスが必ず一致し、ヒットする。応答は読み捨てる。

再送で変えるのは **`max_tokens` を 1 にする** ことだけ (`stream` は付けない)。`max_tokens` は
cache key に入らず、Claude Code の本文が持つ adaptive `thinking` を付けたままでも API は
これを受理する。読み捨ての費用は output 1 トークン、所要は 30K prefix で約 1 秒
(`docs/findings/2026-09-08-cache-ttl-refresh-on-hit.md` 「再送本文の変形」)。`thinking` を
外す加工はしない — 外しても実測では messages 側の断点が生きたが、思考が実際に発火した
本文では未検証で、外さない方が加工が少ない。

これで不要になるもの: marker の文面、nonce、`cache_keepalive` webhook、`applied` /
`late` / `foreign` の語彙と収束規則、合言葉への時刻埋め込み、`keepalive_paused` の
通知、peers による停止 / 解除の中継。**DR-0024 §2 と、それに付く追補 (多プロセス収束 /
peers 中継 / 合言葉が持ち歩く終わり) を supersede する**。

維持するもの:

- **§1 の戦略語彙** (`passthrough` / `none` / `5m` / `1h` / `keepalive`) と ns × モデル
  glob × main/sub の設定形。origin の判定表もそのまま
- **§3 の損益** (`keepalive_horizon` の時間 / 比率、分岐時間の式)
- **§2 の debounce と横断条件**: 実リクエストのたびに +55 分で再武装、`keepalive_horizon`
  を延ばせるのは実リクエストだけ、直前の実リクエストが通った先 (ns / モデル / route) が
  塞がっているなら送らず +55 分で次を試す、`tools` を持つリクエストだけを対象にする
- **§2 の pause API** (`POST /llm-gateway/keepalive/pause`)。止めたい会話があるという
  要求は方式に依らない。ただし解除は簡単になる — 送っているのは自分なので、実リクエスト
  1 本で解くだけでよく、兄弟へ渡す必要がない (置き場を共有するため、決定 3)
- **§禁則** (触るのは `cache_control` の `ttl` のみ)

### 2. 戦略は常時 1h + 55 分ごとのまま

findings の費用式 (`C(H) = P·w·k + (60H / I)·P·r`) から、常時 1h・55 分ごとと
常時 5m・4 分ごとの分岐は `H* = 0.75 / ((15 − 1.091)·(r/w))`:

| r/w | H* (これより長い idle は 1h が安い) |
|---|---|
| 0.1 (通常モデル) | 約 32 分 |
| 0.025 (5.1 系) | 約 2.2 時間 |

keepalive を掛ける系列は「しばらく止まっているが再開しうる会話」なので、どちらの閾値も
下回らない。5m へ寄せると ping 頻度が 14 倍になり、read 単価の低さで相殺しきれない。

### 3. 系列の本文はファイルに置き、兄弟と共有する

置き場は `~/.local/state/llm-gateway/keepalive/<系列>.json` 相当。持つのは本文
(headers を含む、送出直前の形)、horizon の終わり、次の送出予定、ns / モデル / route。

- **メモリでなくファイルにする**。止まっている会話を繋ぐのが keepalive の全てなので、
  再起動で落ちると意味を失う (合図方式でも見張りは置き場に落としていた。DR-0024 §2-7)。
  本文まで持つことで、リリースのたびに全系列が切れる状態がなくなる
- **11301 と 11302 は同じ置き場を共有し、flock で「撫でるのは 1 台」を保証する**
  (DR-0010 の `.lock` サイドカーと同型)。合図方式では共有状態を持たない前提だったため
  観測だけで 1 本へ収束させる規則が要ったが、自送信は gateway が自分で出す 1 本なので、
  ファイルの排他がそのまま重複防止になる。フェイルオーバー直後の 1 周期の重複も出ない
- 実リクエストのたびに上書きする。horizon が尽きた系列と pause された系列は削除する

object storage 等の分散 backend は作らない (兄弟は同一ホストの 2 プロセスで、要件が無い)。

### 4. 本文の保持にプライバシー保護策は付けない

保持するのは会話本文そのものである。この事実を DR / README / 設定ファイルのコメントに
明記する。それだけで、同意フラグ・暗号化・マスキングは持たない。

同じホストには既にセッションの transcript が丸ごと置いてあり、tap
(`?include=request_body`、DR-0017) からも本文は読める。gateway だけに保護の儀式を
足しても実際に守られるものは増えず、「守っている」という誤った印象だけが残る。

### 5. sub も keepalive の対象にできる

`sub = "keepalive"` を受理する (現行は設定エラー)。既定は `passthrough` のまま。

自送信ならセッションの応答を当てにしないので、subagent でも成立する。同種の subagent は
prefix が完全一致することを実測済み (`docs/knowledge/2026-09-02-prompt-cache-and-thinking-facts.md`)
なので、種別ごとの prefix を 1 本撫でておけば、散発的に起動する subagent 群の 1h write を
まとめて消せる。ただし実運用では sub が支配的なモデルほど改善幅が小さい
(field-observation: `opus-5` の $write/1M read は 0.338 → 0.242) ので、既定を変える判断は
実測を見てからにする。

### 6. 自送信も普通の 1 本として観測に出す

- request event (DR-0012) の `origin` 語彙に `keepalive` を足す。自送信はこの値で出る
- usage / stats (DR-0011) には通常どおり計上する。日別の発火回数・トークン・費用が
  stats から自然に取れるので、issue `keepalive-daily-counters` が求めていた専用カウンタは
  要らなくなる (`origin` 別に割れば ping 分の分離もできる。field-observation が jsonl
  走査で代替していた集計が、gateway 側の正本に変わる)
- webui のリングは request event を見ているので、描画側の変更は要らない

### 7. ccmsg 側の受け口とルールは、切り替え後に落とす

移行の順序:

1. gateway に自送信を実装する (合図の経路は残したまま)
2. 両 namespace の設定を切り替える
3. ccmsg の `cache_keepalive` 受け口と、nonce を 1 行で返させる常時ロードのルールを
   削除する (別 issue)

3 を後回しにするのは、切り替え直後に前の作りの合図が飛んできても壊れないようにするため。

## 却下した案

- **messaging socket への直接注入** (`docs/research/2026-09-08-session-injection-and-json-port.md`、
  issue `keepalive-via-messaging-socket`): 届け先を ccmsg から socket へ替えるだけで、
  セッションを起こす点は変わらない。加えて session_id → pid → socket の対応取りが
  `CLAUDE_CONFIG_DIR` のスコープに阻まれ、全 config dir を舐めるか総当たりで書く必要がある
- **本文をメモリだけで持つ**: 再起動で全系列が切れる。止まっている会話を繋ぐ機能なので、
  切れた分を張り直す者が居ない
- **常時 5m + 4 分ごと**: 決定 2 の分岐時間より短い idle でしか勝たない
- **本文保持への同意フラグ**: 保護にならない儀式 (決定 4)
- **系列の共有に object storage 等の分散 backend を使う**: 同一ホストの 2 プロセスに
  対して過剰。要求が出てから足す

## 未確定 (実装前に確かめる)

- **Bedrock / OpenAI 経路で同じ延命が成立するか**。今日の実験は Anthropic OAuth のみ。
  OpenAI 方言は prompt cache の語彙が違う (DR-0024 §1 と同じ留保)
- **系列ファイルのサイズ上限と掃除**。本文を持つので 1 系列が数 MB になりうる。
  上限を超えた系列を諦めるのか、置き場全体に上限を掛けるのか
- **`sub = "keepalive"` の既定値**。決定 5 のとおり、既定は変えずに実測を待つ

## 影響

- DR-0024 の Status が `Partially superseded by DR-0027` になる (§1 / §3 / pause API は
  生きている)
- 自送信の本文保持により、gateway の置き場に会話本文が載る (決定 4)
- ccmsg 側に不要な受け口とルールが残る期間がある (決定 7)
- issue `keepalive-daily-counters` は決定 6 で、issue `keepalive-via-messaging-socket` は
  却下した案で、それぞれ吸収される

## 関連

- DR-0024 (cache 戦略と合図方式の keepalive、本 DR が部分的に上書きする)
- DR-0010 (置き場の flock 排他)、DR-0011 (日別 stats)、DR-0012 (request event / prefix)、
  DR-0017 (tap)
- docs/findings/2026-09-08-cache-ttl-refresh-on-hit.md (ヒットで TTL が更新される実測)
- docs/findings/2026-09-08-keepalive-field-observation.md (合図方式の実運用評価)
- docs/research/2026-09-08-session-injection-and-json-port.md (却下した注入経路)
- docs/issue/2026-09-08-keepalive-daily-counters.md、
  docs/issue/2026-09-08-keepalive-via-messaging-socket.md
