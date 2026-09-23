# DR-0027: keepalive は gateway の自送信 (replay) で行う

- Status: Active
- Date: 2026-09-08 (Accepted: 2026-09-10)

## 文脈

それまでの keepalive は、cache を繋ぐために**会話そのものを 1 往復走らせる**作りになっていた。gateway は合図 (nonce 入りの marker) を webhook へ流し、ccmsg がそれをセッションへ注入し、起こされたセッションが 1 行返す — その 1 本が cache を延ばす。「TTL を延ばすには新しいリクエストが要り、リクエストを作れるのはセッションだけ」という前提の上に立っている。

その前提が今日の実測で崩れた (`docs/findings/2026-09-08-cache-ttl-refresh-on-hit.md`):

- **プレフィックスにヒットしたリクエストは、そのエントリの TTL を更新する。5m でも 1h でも同じ**。1h エントリは t+50 の 1 回のヒットで t+70 でも read 100% / write 0、同時刻に作った対照エントリは t+70 で失効して全量 write
- **差分ゼロの同一本文 replay でもヒットは成立し、TTL は更新される**。新エントリは生まれず、課金はヒット分の read だけ (write ゼロ)

つまり cache を延ばすのに**新しい内容も、セッションも要らない**。gateway が最後に転送した本文をそのまま送り直せば足りる。合図方式の複雑さは、そのほぼ全てが「他人 (セッション) にリクエストを作らせる」ことから派生している:

- **セッションを起こす**。webui に ping が並び、TUI に marker と応答が出る。合言葉だけを返させる常時ロードのルールを配って遵守率 98% まで詰めたが、ノイズはゼロにならない (`docs/findings/2026-09-08-keepalive-field-observation.md`: 導入初期の遵守率 32.6%)
- **届け先が要る**。webhook の宛先を書いていない namespace では合図が出ず (`1h` と同じ振る舞いに落ちる)、`llm-gateway check` の警告に頼る
- **合図が戻るとは限らない**。`applied` / `late` / `foreign` / 戻らない、の 4 通りを観測して 1 本へ収束させる規則、合言葉に起点と終わりを埋めて期間を持ち歩かせる仕掛け、停止を兄弟へ回す peers の経路 — いずれもこの不確実性の後始末で、期間の取り直しによる連鎖 (5.5 時間の期間へ 19 本 ≒ 16.5 時間の合図) のような bug の温床になっている
- **再起動で切れる**。見張りは置き場に落としているが、合図の相手 (セッション) が生きているかは gateway から分からない
- **sub に使えない**。`sub = "keepalive"` は設定エラー。subagent は合図を返す相手として当てにできないため

自送信ならこれらは全て消える。相手はセッションではなく upstream で、1 リクエストで完結する。

## 決定

### 1. 合図方式を廃し、自送信 (replay) に置き換える

gateway は系列 (DR-0012 の `prefix` + `session_id`) ごとに「最後に upstream へ転送した本文」を保持し、idle 55 分ごとにそれを**そのまま**再送する。認証の差し替えとモデル名の書き換えを済ませた後の形 — つまり実際に線に乗った bytes — をそのまま出す。加工しないのでプレフィックスが必ず一致し、ヒットする。応答は読み捨てる。

再送で変えるのは **`max_tokens` を 1 にする** ことだけ (`stream` は付けない)。`max_tokens` は cache key に入らず、Claude Code の本文が持つ adaptive `thinking` を付けたままでも API はこれを受理する。読み捨ての費用は output 1 トークン、所要は 30K prefix で約 1 秒 (`docs/findings/2026-09-08-cache-ttl-refresh-on-hit.md` 「再送本文の変形」)。`thinking` を外す加工はしない — 外しても実測では messages 側の断点が生きたが、思考が実際に発火した本文では未検証で、外さない方が加工が少ない。

自送信では、セッションへ頼むための道具立てが丸ごと不要になる: marker の文面、nonce、`cache_keepalive` webhook、`applied` / `late` / `foreign` の語彙と収束規則、合言葉への時刻埋め込み、`keepalive_paused` の通知、peers による停止 / 解除の中継。

本 DR が決めるのは**繋ぎ方だけ**で、どの本文に何を効かせるか (DR-0024 §1 の戦略語彙と origin 判定表)、いつ仕掛けて畳むか (同 §2 の debounce と横断条件、pause API)、どこまで繋ぐか (同 §3 の損益) は DR-0024 のまま。解除だけは簡単になる — 送っているのは自分なので、見張りを畳めばそれが停止で、次の実リクエストが張り直す (置き場を共有するため、決定 3)。

### 2. 戦略は常時 1h + 55 分ごとのまま

findings の費用式 (`C(H) = P·w·k + (60H / I)·P·r`) から、常時 1h・55 分ごとと常時 5m・4 分ごとの分岐は `H* = 0.75 / ((15 − 1.091)·(r/w))`:

| r/w | H* (これより長い idle は 1h が安い) |
|---|---|
| 0.1 (通常モデル) | 約 32 分 |
| 0.025 (5.1 系) | 約 2.2 時間 |

keepalive を掛ける系列は「しばらく止まっているが再開しうる会話」なので、どちらの閾値も下回らない。5m へ寄せると ping 頻度が 14 倍になり、read 単価の低さで相殺しきれない。

### 3. 系列の本文はファイルに置き、兄弟と共有する

置き場は `~/.local/state/llm-gateway/keepalive/<系列>.json` 相当。持つのは本文 (headers を含む、送出直前の形)、horizon の終わり、次の送出予定、ns / モデル / route。

- **メモリでなくファイルにする**。止まっている会話を繋ぐのが keepalive の全てなので、再起動で落ちると意味を失う。本文まで持つことで、リリースのたびに全系列が切れる状態がなくなる
- **11301 と 11302 は同じ置き場を共有し、flock で「撫でるのは 1 台」を保証する** (DR-0010 の `.lock` サイドカーと同型)。合図方式では共有状態を持たない前提だったため観測だけで 1 本へ収束させる規則が要ったが、自送信は gateway が自分で出す 1 本なので、ファイルの排他がそのまま重複防止になる。フェイルオーバー直後の 1 周期の重複も出ない
- 実リクエストのたびに上書きする。horizon が尽きた系列と pause された系列は削除する

object storage 等の分散 backend は作らない (兄弟は同一ホストの 2 プロセスで、要件が無い)。

### 4. 本文の保持にプライバシー保護策は付けない

保持するのは会話本文そのものである。この事実を DR / README / 設定ファイルのコメントに明記する。それだけで、同意フラグ・暗号化・マスキングは持たない。

同じホストには既にセッションの transcript が丸ごと置いてあり、tap (`?include=request_body`、DR-0017) からも本文は読める。gateway だけに保護の儀式を足しても実際に守られるものは増えず、「守っている」という誤った印象だけが残る。

### 5. sub も keepalive の対象にできる

`sub = "keepalive"` を受理する (現行は設定エラー)。既定は `passthrough` のまま。

自送信ならセッションの応答を当てにしないので、subagent でも成立する。同種の subagent は prefix が完全一致することを実測済み (`docs/knowledge/2026-09-02-prompt-cache-and-thinking-facts.md`) なので、種別ごとの prefix を 1 本撫でておけば、散発的に起動する subagent 群の 1h write をまとめて消せる。ただし実運用では sub が支配的なモデルほど改善幅が小さい (field-observation: `opus-5` の $write/1M read は 0.338 → 0.242) ので、既定を変える判断は実測を見てからにする。

### 6. 自送信も普通の 1 本として観測に出す

- request event (DR-0012) の `origin` 語彙に `keepalive` を足す。自送信はこの値で出る
- usage / stats (DR-0011) には通常どおり計上する。日別の発火回数・トークン・費用が stats から自然に取れるので、issue `keepalive-daily-counters` が求めていた専用カウンタは要らなくなる (`origin` 別に割れば ping 分の分離もできる。field-observation が jsonl 走査で代替していた集計が、gateway 側の正本に変わる)
- webui のリングは request event を見ているので、描画側の変更は要らない

### 7. ccmsg 側の受け口とルールは、切り替え後に落とす

移行の順序:

1. gateway に自送信を実装する (合図の経路は残したまま)
2. 両 namespace の設定を切り替える
3. ccmsg の `cache_keepalive` 受け口と、nonce を 1 行で返させる常時ロードのルールを削除する (別 issue)

3 を後回しにするのは、切り替え直後に前の作りの合図が飛んできても壊れないようにするため。

### 8. 控えるのは cache に乗った 1 本だけ

保持と送り直しの続行は、**応答の usage が示す cache の結果** (DR-0012 の `hit` / `written` / `partial` / `none` / `unknown`) で決める:

- **控えるのは `hit` / `written` / `partial` のいずれかで終わった 1 本だけ**。`none` (cache を使わなかった) と `unknown` (usage が読めなかった) は控えない
- **自送信が `none` を返したら、その系列を畳む**。控えを消し、約束した寿命を `cache_expired` で取り消す (期限切れで畳むときと同じ扱い)。`unknown` では畳まない — cache が無いと分かったわけではないので、塞がりと同じく次の予定へ回す

`cache_control` を 1 つも持たない本文は上流で cache に乗らず、送り直しても延びるものは無い。実測 (2026-09-12〜14 の自送信 348 本) では 81 本が「毎回 `none`」の 4 系列で、数十万トークン級の全量入力を 55 分ごとに払い続けていた。決定 1 の「最後に転送した本文をそのまま送り直せば足りる」は、その本文が cache に乗っていることを前提にしている。

結果が出るのは応答本文を読み終えた時点なので、控えを置くのは転送の直後ではなく交換の終端 (`exchange` の観測が締まるところ)。

**知らせに出す連鎖 (DR-0012 の `cache_*`) は、控えを待たずに 1 本目から出す**。実リクエストは連鎖を 0 から数え直すので、見立ては送る時点の値 (この 1 本の時刻・期間・単価) だけで決まる — 控えを読んで組む必要がない。乗らなかった 1 本では、その場で `cache_expired` を出して約束を取り消す (控えないので繋がらない)。先に約束して後から取り消す形にするのは、繋ぐ系列の大半 (cache に乗る 1 本) で 1 本目から終わりが描けるほうが、全系列で 1 本ぶん遅れるより良いため。

### 9. 乗らなかった 1 本は、研究用に一定量・一定期間だけ取っておく

決定 8 で控えない・畳む 1 本は、そこで本文ごと消える。`cache_control` を持っているのに乗らない系列は実在し (`docs/research/2026-09-14-claude-code-uncached-requests.md` の「不明」3 系列)、**何が違ったのかを見るには本文そのものが要る**。

- 置き場は控えの下、`<stats の置き場>/keepalive/uncached/<会話>.<系列>.<送った時刻>.json`。控えを拾う側は `keepalive/` 直下の `.json` しか見ないので、両者は混ざらない
- 中身は控え (`Kept`) と同じ形 — 本文・ヘッダ・model・route・ns・会話・系列 — に、判定の材料 (応答の usage・`Cache` の語・応答の status) と、**どこで捨てたか** (`entry` = 入口で控えなかった / `keepalive` = 自送信が `none` で畳んだ) を添える
- 保持は「新しい順に N 件、かつ M 日」。既定は 50 件 / 7 日で、書くときと起動時 (`restore`) に超えた分を捨てる。時刻はファイル名から読むので、切り詰めに本文は開かない
- 上限は `[stats]` 直下 (`uncached_keep` / `uncached_days`) に置く。`[[ns.<ns>.cache]]` ではない — これは cache 戦略ではなく研究用の退避で、置き場は `stats.dir` が決める。どちらかが 0 なら仕掛けごと止まる (書かないし、既にあるものも触らない)
- **プライバシー保護策は付けない** (決定 4 と同じ理由)。置き場を配るときは、会話の中身がそのまま入っている前提で扱う
- 取っておけなくても転送・自送信は止めない (warn だけ)

## 却下した案

- **messaging socket への直接注入** (`docs/research/2026-09-08-session-injection-and-json-port.md`、issue `keepalive-via-messaging-socket`): 届け先を ccmsg から socket へ替えるだけで、セッションを起こす点は変わらない。加えて session_id → pid → socket の対応取りが `CLAUDE_CONFIG_DIR` のスコープに阻まれ、全 config dir を舐めるか総当たりで書く必要がある
- **本文をメモリだけで持つ**: 再起動で全系列が切れる。止まっている会話を繋ぐ機能なので、切れた分を張り直す者が居ない
- **常時 5m + 4 分ごと**: 決定 2 の分岐時間より短い idle でしか勝たない
- **本文保持への同意フラグ**: 保護にならない儀式 (決定 4)
- **系列の共有に object storage 等の分散 backend を使う**: 同一ホストの 2 プロセスに対して過剰。要求が出てから足す

## 未確定

- **Bedrock / OpenAI 経路で同じ延命が成立するか**。実験は Anthropic OAuth のみ。OpenAI 方言は prompt cache の語彙が違う (DR-0024 §1 と同じ留保)
- **`sub = "keepalive"` の既定値**。決定 5 のとおり、既定は変えずに実測を待つ

決着した分:

- **系列ファイルのサイズ上限と掃除**: 1 系列 8MB を蓋にして、超える系列は保持しない (= 延命しない)。置き場全体には上限を掛けない — 系列の数は会話の数で頭打ちになり、1 本ずつの蓋があれば総量も抑えられる

## 影響

- 自送信の本文保持により、gateway の置き場に会話本文が載る (決定 4)
- **設定語 `keepalive` が自送信を指す**。移行のために置いた暫定語 `replay` は、合図方式の撤去と同時に削除した (alias も残さない)
- 合図方式の撤去で、次のものが無くなった: request event の `keepalive` (合図の扱い) と `cache_paused`、`cache_keepalive` / `keepalive_paused` の知らせ、`[server] peers` と兄弟への中継、`POST /llm-gateway/keepalive/resume` と `GET /llm-gateway/keepalive/paused`。`cache_paused` が消えるのは、停止が控えを落とすだけの操作になって状態として残らないため (DR-0012 を更新済み)
- 連鎖の欄 (`cache_since` 以下) は 1 本目から出る。控えを置くのは応答を読み切った後 (決定 8) だが、見立ては送る時点の値だけで決まる。乗らなかった 1 本はその場で `cache_expired` が取り消す
- ccmsg 側の受け口と常時ロードのルールは、別 issue で落とす (決定 7)
- issue `keepalive-daily-counters` は決定 6 で、issue `keepalive-via-messaging-socket` は却下した案で、それぞれ吸収される

## 関連

- DR-0024 (cache 戦略、keepalive を仕掛ける条件と止める口、損益)
- DR-0010 (置き場の flock 排他)、DR-0011 (日別 stats)、DR-0012 (request event / prefix)、DR-0017 (tap)
- docs/findings/2026-09-08-cache-ttl-refresh-on-hit.md (ヒットで TTL が更新される実測)
- docs/findings/2026-09-08-keepalive-field-observation.md (合図方式の実運用評価)
- docs/research/2026-09-08-session-injection-and-json-port.md (却下した注入経路)
- docs/issue/2026-09-08-keepalive-daily-counters.md、docs/issue/2026-09-08-keepalive-via-messaging-socket.md
