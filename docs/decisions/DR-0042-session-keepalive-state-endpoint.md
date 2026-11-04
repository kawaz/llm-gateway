# DR-0042: 会話 (session) 単位の cache / keepalive の状態を問う口 (`GET /llm-gateway/sessions[/{session_id}]`)

- Status: Proposed
- Date: 2026-11-04

## 背景

会話を見ている側 (ccmsg、Claude Code の mod) が、自分の会話について「cache はいつまで生きているか」「gateway は送り直しを何本目まで出していて、いつ畳むか」「この会話の経路は枠で塞がっていないか」を知りたい。webui にリングと残りを描く、status line / band に残りと本数を出す、継ぎ足しの終わり (`cache_until`) が近づいたら cache が生きているうちにハンドオフを書く、隣のレーンの枠が戻ったら作業を再開する、といった用途で、いずれも**今の状態**を 1 回読めれば足りる。

**ccmsg と gateway の連携は、ccmsg が gateway へ取りに来る (pull) だけにする** (kawaz 裁定 2026-11-04)。ccmsg は Claude Code の hook (turn end / step / prompt) で「この会話が今動いた」を知れるので、そのタイミングで `GET /llm-gateway/sessions/{session_id}` を 1 回取れば足りる。「変わった」の合図も poll も要らない。daemon の再起動と webui の初期表示は一覧 `GET /llm-gateway/sessions` で一括して取る。取りに来る形なら「欲しい時に取れない」(起動前の出来事を持っていない、取りこぼし) が構造的に消える。gateway から ccmsg へ押す経路 (events の SSE 購読、`[webhook]`、`[[hooks]]` とその署名) は ccmsg の用途では使わない。

状態を知る口は今 `/llm-gateway/events` (DR-0012) しか無い。events は「起きたこと」を流すだけで状態は持たず (DR-0012「流すのは『起きたこと』だけ。状態は持たない」)、繋いだ後の分しか届かない。`--resume` で続きを開いた会話では直前の控えと連鎖が起動前に出来ていて、繋いだ側には見えない。再接続や取りこぼし (`seq` の飛び) の後も同じで、見る側が events から状態を組み直すには、gateway が控えに対して行う判断 (55 分の格子、`written` での起点の置き直し、横断条件での見送り) を写した複製が要る。

gateway は系列ごとに控え (`<[stats] dir>/keepalive/<会話>.<系列>.json`、DR-0027 決定 3) を持ち、そこに書いてある予定と期限で実際に送り直している。この控えが「その会話について gateway が次に何をするか」の正本で、知らせの `cache_*` もここから組まれる (`Keepalive::chain`)。経路が枠で塞がっているかは経路の印 (`RouteState` の `marks`、DR-0009 / DR-0020) が正本で、usage の `denials` はそれを写している。問われたらこの 2 つを写して答えればよく、新しい計測も新しい器も要らない。

本 DR は**読む口**を決める。呼び手の認証は DR-0043 (呼び手の JWKS で検証する) に従う (決定 4)。枠の遷移を events に流す提案 (DR-0039 の `credential_limited` / `credential_recovered`) は、本口を hook のたびに読めば代替できる (決定 1)。配達 (DR-0040 の hooks) には依存しない。

## 決定

### 1. `GET /llm-gateway/sessions/{session_id}` は、その会話の控え (keepalive の系列) と、その系列が使う経路の枠の印を写して答える

答えは `{session_id, series: [...]}`。`series` にはその会話の id を持つ控えが 1 系列 1 要素で並び、要素の欄は**控えに書いてある値と、控えから組める連鎖の見立て**、それに**その系列の面とモデルで試される経路の枠の印** (`routes`) だけ。欄の名前と意味は DR-0012 の `request` の知らせと DR-0020 の `denials` から借りる (同じ名前は同じ意味。時刻は Unix ミリ秒、長さだけ名前に単位):

```
GET /llm-gateway/sessions/0f3c6d1a-…
{"session_id":"0f3c6d1a-…","series":[
  {"prefix":"2cf24dba","ns":"personal","model":"claude-fable-5-1","route":"claude-kawazzz",
   "cache_notice":"kUu1xR4-tQ9nSp2Zc0dBvA","cache_expires_at":1785603570000,
   "cache_since":1785600000000,"cache_count":0,"next_keepalive_at":1785603300000,
   "cache_until":1785633300000,"cache_until_count":9,
   "routes":[
     {"route":"claude-kawazzz","credential":"claude-main"},
     {"route":"claude-b","credential":"claude-sub","limited":{"until":1785618060000}}
   ]}
]}
```

| 欄 | 出所 | 意味 |
|---|---|---|
| `prefix` | 控え | 系列の識別子 (DR-0012。`system` 先頭ブロックのハッシュ 8 桁)。知らせの `prefix` と突き合わせる鍵 |
| `ns` / `model` | 控え | 直前の実リクエストが通った面と、解決後の実モデル名 |
| `route` | 控え | 直前の実リクエストが通った経路の名前。知らせでは `credential` の欄に同じ値が載る (DR-0012「どの経路に当たったか」)。送り直しはこの経路が使える時だけ出る (DR-0024 §2 の横断条件) |
| `cache_notice` | 控え | この系列について最後に約束した寿命の名前。`cache_expired.of` と突き合わせる (受け取る側の規則は DR-0012) |
| `cache_expires_at` | 控え | gateway が「繋ぐ相手がまだ在る」と見なす期限。最後に送った 1 本の時刻 + 1 時間から 30 秒の余裕を引いた値で、送り直す直前に壁時計とこれを比べる (DR-0024「期限は壁時計で見る」)。知らせの `cache_expires_at` (余裕を引かない見込み) より 30 秒早い |
| `cache_since` | 控え | 連鎖の起点。実リクエストの時刻で、送り直しが `written` だった時はその時刻に置き直される (DR-0012) |
| `cache_count` | 控え | ここまでに送り直した本数 (実リクエストの直後は 0) |
| `next_keepalive_at` | 控え | 次の送り直しの予定時刻。経路が塞がって見送った後も、置き直した実際の予定が載る |
| `cache_until` / `cache_until_count` | 控えから `Keepalive::chain` で組む | 送り直しで継ぎ足せる終わりと、連鎖で送る総数 (55 分の格子、DR-0012 と同じ規則の見込み) |
| `routes` | 経路の印 (`RouteState::marks`) | この系列の `ns` と `model` で試される経路を試す順 (`Router::route_names`) に並べ、経路ごとに `{route, credential?, limited?}`。`route` は経路の名前 (`[routes.<name>]`)、`credential` はその経路が使う認証情報の名前 (`routes.<name>.credential`。持たない経路 (relay) では欄ごと出さない)。`limited` は、その経路に**上限の印 (`Reason::Limited`) がこの `model` に効いて**付いている時だけ出す `{until}` で、`until` はその時刻 (Unix ミリ秒) まで候補にしないという印の期限。効く印が複数あれば遅く開く方 (`RouteState::denial` と同じ規則)。印が無ければ欄ごと出さない |

`routes` 以外の欄は全部必ず出す (欠けさせると読む側が形を 2 通り扱う)。並びは `cache_since` の降順で、先頭が最新の系列。

答えは**控えと印の写し**で、問われた時点での判定 (期限が過ぎているか、経路が使えるか) はしない。判定は送る直前に gateway が行い、結果は `cache_expired` の知らせと控えの消去として現れる。読む側は `cache_expires_at` / `cache_until` / `limited.until` を壁時計と比べる。

`routes` に載せるのは**上限の印だけ**。混雑 (`busy`) は 429 のたびに置き直される 60 秒の退避で、読んだ瞬間にはもう意味が無い。`org_not_allowed` は設定の誤りで応答と log に見える。どちらも今までどおり印としては付き、usage の `denials` (DR-0020) で読める。載せる価値があるのは「隣のレーンがいつ戻るか」だけで、DR-0039 決定 1 が events に流す対象を `limited` に絞ったのと同じ線を引く。`reason` の欄は持たない (欄の名前 `limited` が理由を言っている)。`paced` (DR-0019) は印を残さず判定のたびに計算するので、ここにも usage にも現れない。

これで **DR-0039 の `credential_limited` / `credential_recovered` は本口の pull で代替できる**。読む側は hook のたびに本口を読み、前回の答えと比べて `limited` が現れた経路を「塞がった」、消えた (または `until` が過ぎた) 経路を「戻った」と読む。差分を取るのは読む側で、gateway は「最後に知らせた状態」を持たない (DR-0039 が頑張らないとした取りこぼしの問題が、pull では起きない: 印が別の経路で消えても次の答えには載らないだけ)。

`routes` の `limited` だけは**この unit の印**で、兄弟の unit (stable / unstable) とは違いうる。印はメモリだけに持ち (DR-0009)、各 unit が自分の空振りで付ける。Caddy は `lb_policy first` で生きている順の 1 つへ回す (DR-0028 決定 4) ので、会話を運んでいる unit と問いに答える unit は同じになり、載るのは実際にその会話の候補を絞っている印になる。控え (`series` の他の欄) は置き場を共有するので unit に依らない (決定 2)。

`routes` が空になるのは、その `ns` にその `model` を扱える経路が 1 つも無い時 (一覧に無いモデル、`daemon reload` で経路が消えた後) だけで、これも状態であってエラーではない。

控えが置かれるのは応答を読み切った後 (DR-0027 決定 8) なので、走行中の 1 本はまだ答えに反映されていない。応答が閉じてから問う (ccmsg なら turn end の hook、mod なら `turn.step` の後)。

控えの無い会話には `{"session_id":"…","series":[]}` を 200 で返す。gateway は会話そのものを知らず、知っているのは控えだけなので、「無い会話」と「控えの無い会話」を区別できない。`keepalive` 戦略の無い面の会話、`tools` を持たない 1 本だけの会話、pause した会話、期限か期間が尽きて畳まれた会話はいずれもこの形になる。空の配列は「この会話について gateway は何も繋いでいない」という答えであって、エラーではない。

**本文は載せない**。控えには送出直前の本文が丸ごと入っている (DR-0027 決定 4) が、外へ出すのは id・名前・時刻・本数まで。本文を見る口はダンプ (DR-0038) だけ。

### 2. 一覧 `GET /llm-gateway/sessions` を持つ。載るのは控えのある系列だけで、`ns` で絞れる

```
GET /llm-gateway/sessions?ns=personal
{"sessions":[{"session_id":"0f3c6d1a-…","series":[{…}]}, …]}
```

1 件の形は決定 1 と同じ。`ns` を付けると、その面を通った系列だけを残し、系列を 1 つも持たなくなった会話は落とす。並びは各会話の先頭の系列の `cache_since` 降順。

使い手は、daemon の再起動の後や webui の初期表示で全会話のリングを一括で描く側 (ccmsg) と、置き場を直接 `ls` する代わりに系列の数と予定を見たい運用者。読むのは控えの置き場で、兄弟の unit (stable / unstable) は同じ置き場を共有する (DR-0027 決定 3) ので、`routes` の印 (決定 1) を除いてどの unit に問うても同じ答えになる。

「控えは無いが直近 N 時間に request があった会話」は載せない。gateway はそれを覚えていない (events は流すだけ、stats に会話の軸は無い) ので、載せるには会話ごとの表を新しく持つことになる。

### 3. 系列の単位は控えと同じ (session_id, prefix)。問いの鍵は session_id、答えは系列の配列。agent_id は持たない

gateway が繋ぐ単位は (会話の id, `prefix`) の系列で (DR-0027 決定 1)、1 つの会話が同時に複数の系列を持ちうる: `sub = "keepalive"` の面では subagent の系列が main と別の `prefix` で並び、Claude Code の版が上がる等で `system` 先頭ブロックが変わった会話では、古い系列が期限か期間で畳まれるまで新旧が並ぶ。

- 問いの鍵は **session_id だけ**。呼び手は会話の id (`x-claude-code-session-id` に載せる値) は知っているが、`prefix` は gateway が本文から作る印で、知らせを受けるまで分からない。`prefix` で絞らせると、知らせを一度も受けていない (まさに問いたい) 場面で使えない
- 答えは**系列の配列**。main の系列を 1 つに潰して答えると、subagent の系列や新旧の系列を読み手から隠す。どれが自分の本流かは、知らせの `prefix` と突き合わせるか、`cache_since` の新しい方を取る (並びは決定 1)
- **`agent_id` の欄は持たない**。控えに無い。同種の subagent は `prefix` が完全一致して 1 つの cache を共有する (DR-0027 決定 5) ので、系列は agent ではなく種類の単位になり、最後に通った agent の id を書いても「この系列は誰のものか」の答えにならない

### 4. 呼び手の認証は DR-0043 (呼び手の JWKS で検証する) を掛ける。無認証の段階は持たない。ns 付きの形は持たない

本口は DR-0043 が署名を求める運用系の口の 1 つとして生まれる。呼び手 (ccmsg) は自分の鍵で各 request を JWT 署名し (`Authorization: Bearer <JWT>`、`aud` は `GET /llm-gateway/sessions/<session_id>` のように method + path)、gateway は設定に書いた呼び手の `jwks_url` で検証する。形と検証の順序は DR-0043 が正本で、本 DR は何も足さない。

DR-0043 の無い形 (運用系の他の口と同じく手前の境界だけを信頼する) で先に出す段階は持たない。本口は ccmsg が Caddy 越しに読む前提で、tailnet の外に居る ccmsg のために Caddy がこの path を外へ通す構成を許す。出すのは events の `request` の欄の部分集合 (id・面・モデル・経路名・`prefix`・時刻・本数) と経路の枠の印で、本文もトークン数も無いが、session_id を知っていれば他人の会話の状態が、一覧 (決定 2) では全会話の id が読める。events が同じ値を無認証で流しているのは tailnet の内側に閉じている前提があるから (DR-0041 決定 6 が JWKS 以外を外に出さない根拠と同じ) で、外へ通す口に同じ前提は当てられない。

`Access-Control-Allow-Origin` は付けない。呼び手はサーバ (ccmsg) で、ブラウザの画面は ccmsg が取った答えを描く。署名を持たないブラウザが直接読む使い方は無い。

gateway 自身の口は `/llm-gateway/` の直下にだけ置き、`/ns-<ns>/llm-gateway/…` は 404 のまま (DR-0006 の規約。`usage_is_not_namespaced` / `healthz_is_not_namespaced` の試験が固定している)。ns 認証 (`jwt`) の下に置く形は、読める情報が同じまま gateway の口に初めて面ごとの認証経路を増やすことになる。運用系の呼び手の認証は面に依らず DR-0043 の 1 つの仕組みで扱う。

### 5. 書き込みは足さない。pause は `POST /llm-gateway/keepalive/pause` のまま

`/llm-gateway/sessions` は GET だけ。控えを落とす操作は既存の `POST /llm-gateway/keepalive/pause` `{"session_id"}` (DR-0024 §2) をそのまま使う。

- 止めるのは keepalive であって会話ではない。会話は続き、次の実リクエストで控えが置き直される。`DELETE /sessions/{id}` や `/sessions/{id}/pause` と綴ると「会話を消す / 止める」と読めて、起きることと名前が合わない。動作はその動作の名前の下に置く
- 外から届く面 (子の HTTP) に台の挙動を変える口を増やさない (DR-0032 決定 2 の方向)。pause は既にそこに在る 1 つで、同じ操作の綴りを 2 つにしない
- ccmsg / mod は Caddy 越しに gateway へ届く側なので、pause を使うなら HTTP の形が要る。DR-0032 が候補に挙げた「pause を制御 socket へ寄せる」は、本 DR で HTTP の pause に使い手が 1 つ増えることを踏まえて判断する (本 DR では動かさない)

### 6. 口の在り処は `ANTHROPIC_BASE_URL` の末尾の `ns-<name>` の段を落とした根

呼び手が gateway の根を知る方法は、クライアントが既に持っている `ANTHROPIC_BASE_URL` (ccmsg は会話を起動する側なのでその env を知っている) から導く。転送系は `https://<host>/ns-<name>` (面を付けない形は `https://<host>`) で、gateway 自身の口は面に関わらず根の `/llm-gateway/…` に在る (決定 4)。末尾の段が `ns-` で始まればそれを落とし、残りに `/llm-gateway/sessions/<session_id>` を足す。`ANTHROPIC_AUTH_TOKEN` は付けない (面の認証は転送系のもので、運用系の呼び手の認証は DR-0043 の署名)。

```
ANTHROPIC_BASE_URL=https://gw.example.net/ns-personal
→ https://gw.example.net/llm-gateway/sessions/<session_id>
```

導いた根が gateway かは `GET <根>/llm-gateway/healthz` が `ok` を返すかで確かめられる (DR-0006)。gateway 側に探索の口 (`/ns-<ns>/api/hello` の応答に根を載せる等) は足さない。規則が 1 行で、既にある口で確かめられるため。

呼び手の使い方は「会話が動いたと知った時に 1 回問う」。ccmsg は Claude Code の hook (turn end / step / prompt) のたび、mod なら `turn.step` の後に 1 件の口を読み、daemon の再起動と webui の初期表示は一覧で一括する。events は繋がない (背景の裁定)。hook と hook の間に答えが変わるのは gateway の送り直し (`cache_count` / `next_keepalive_at`) と枠の印だけで、次の hook で読めれば足りる。status line の描画のたびに問う形にはしない — 残りは `cache_until` / `cache_expires_at` と壁時計から手元で描ける。

## 採らなかった案

- **ccmsg が events を購読して自分で状態を組む (gateway から押す)**: 起動前の分 (`--resume` の直前の控えと連鎖) が無く、再接続と取りこぼしのたびに空白が出る。組み直すには gateway の判断 (格子、`written` での起点の置き直し、見送り) を写した複製が要り、2 か所で同じ規則を保守することになる。ccmsg は hook で会話が動いた瞬間を知れるので、その時に取りに来れば「変わった」の合図は要らない (背景の裁定、決定 1)
- **枠の遷移を `credential_limited` / `credential_recovered` (DR-0039) で押す**: 取りに来る側は hook のたびに `routes` を読んで前回と比べれば同じことが分かる。押す形は「印が別の経路で消えた」「再起動で印が消えた」の取りこぼしを抱え (DR-0039 決定 1)、pull には無い (決定 1)
- **`routes` を会話の直下に置く (系列ごとでなく)**: 試される経路は面とモデルで決まり、どちらも系列の欄。会話が複数の系列 (main と subagent、新旧) を持つ時に 1 つに潰せない (決定 1)
- **`routes` に `busy` / `org_not_allowed` の印や `paced` も載せる**: `busy` は読んだ瞬間に意味が無い 60 秒の退避、`org_not_allowed` は設定の誤り、`paced` は印が無い。usage の `denials` と `llm_request` の `skipped` で読む (決定 1)
- **`limited` に `reason` の欄を持つ**: 載せる印が `limited` だけなので値が 1 つしか無い。欄の名前が理由を言っている (決定 1)
- **無認証で先に出し、認証は後から掛ける**: 外へ通す口に tailnet の内側の前提 (events が無認証でよい理由) を当てる期間を作ることになる (決定 4)
- **stats から逆算する**: 日次の積算に会話の軸は無く (DR-0011、DR-0029「やらないこと」)、時刻も予定も持たない (決定 1)
- **`/llm-gateway/self` に全会話を載せる**: `/self` は「走っているこのプロセス」の状態 (版・`boot`・落とした数・動いているダンプ) で、`daemon status` が各 unit に 1 回ずつ問う口 (DR-0028 決定 5)。控えは unit を跨いで共有する走行状態で性質が違い、`daemon status` のたびに全系列を読む形になる (決定 1・2)
- **`context_tokens` (直近の usage の `input` + `cache_read` + `cache_creation`) を載せる**: 控えに usage は無い。今の会話が何トークンかはクライアント自身の知識で、Claude Code の mod は `$.session.usage()` から `context.tokens` / `window` / `percent` を応答のたびに直接読める (kawaz/claude-context-notify の DR-0003 で実測済み)。gateway が持つ値は「控えを置いた最後の 1 本」の usage で、脇の 1 本や subagent の 1 本が混ざる会話では読み手の期待とずれる。欄を持つなら控えに usage を書き足すことになり、`keepalive` の面の本流にしか無い欄が公開契約に増える。events がトークン数を載せない (DR-0012) のと同じ線を引く (決定 1)
- **直近の cache の結果 (`hit` / `written` / `partial` / `none`) を載せる**: 控えに無い。控えが在ること自体が「最後の 1 本は乗った」(DR-0027 決定 8。`none` は畳まれて答えから消える) で、`written` は `cache_count` が 0 に戻ることで読める。1 本ごとの語は `response` の知らせが流している (決定 1)
- **`paused` を載せる**: 止めた状態は持たない (DR-0024 §2「見張りを畳むことが停止」)。pause した会話は `series: []` になる (決定 1)
- **`last_request_ts` (最後の実リクエストの時刻) を載せる**: 控えは連鎖の起点 (`cache_since`) しか持たず、`written` の送り直しで置き直される。実リクエストの時刻そのものは `request` の知らせの `ts` にしか無く、控えに時刻を 1 つ足して持たせる理由が「欄の名前を 1 つ増やす」以上に無い (決定 1)
- **`horizon_end` (`keepalive_horizon` の終わり、連続値) を載せる**: DR-0012 が events に連続値を載せない理由 (見る側が描くのは「いつ切れるか」で、切れる時刻は送り直しの刻みでしか動かない) がそのまま当てはまる。ハンドオフの期限は `cache_until` で決まる (決定 1)
- **`cache_breakeven_until` / `cache_breakeven_count` を載せる**: 控えに無く、答える時点で経路の単価表を引いて組む値。系列の状態ではなく (モデル, 経路) の性質で、経路が設定から消えていれば組めない。`request` の知らせが毎回載せている (決定 1)
- **`cache_ttl_secs` を載せる**: `keepalive` の系列は常に 3600 (DR-0024 決定 2) で、控えにも無い (決定 1)
- **「直近 N 時間に request があった会話」も一覧に載せる**: 会話ごとの表を新しく持ち、増え続ける鍵の掃除 (N の根拠) を決めることになる。会話の id は使うほど増える (DR-0029 が会話の軸を別設計にした理由) (決定 2)
- **控えの無い会話に 404 を返す**: 「無い会話」と「控えの無い会話」を gateway は区別できず、空の答えはエラーではなく状態 (決定 1)
- **問いの鍵を (session_id, prefix) や agent_id にする**: 読み手は `prefix` を知らずに問いたい。agent_id は控えに無く、系列は agent の単位ではない (決定 3)
- **`/ns-<ns>/llm-gateway/sessions/…` の形を (alias としてでも) 持つ**: gateway の口を面の下に置かない規約 (DR-0006) を崩し、呼び手の根の導出も「落とす」から「両方試す」になる (決定 4・6)
- **ns 認証 (`jwt`) の下に置く**: 読める情報は events と同じで、gateway の口に初めて面ごとの認証経路が増える (決定 4)
- **`DELETE /llm-gateway/sessions/{id}` / `POST …/{id}/pause` で pause を綴り直す**: 止まるのは keepalive で、会話ではない。同じ操作に 2 つの綴りを持たない (決定 5)
- **会話単位の SSE (`/llm-gateway/sessions/{id}/events`)**: 差分は hook のたびの pull で足り、押す経路は ccmsg の用途では使わない (背景の裁定)。流れを目で追いたい時は `/llm-gateway/events` を `session_id` で選べば足りる (決定 1)

## 影響

- `/llm-gateway/sessions` と `/llm-gateway/sessions/{session_id}` の GET が増える (決定 1・2)。`Keepalive` に控えを読んで答える口 (`pause` が既に行っている `load_all` + `session_id` での絞り込みと同じ読み方) が増え、答えは `Kept` の欄と `Keepalive::chain` から組む。本文は読み出した後に捨て、外へは出さない。`routes` は系列の `ns` / `model` で `Router::route_names` を引き、各経路の `Preset::denials` から `Limited` でその `model` に効くものを拾う (usage の `denials` を組む `usage_report` と同じ読み方)。新しい計測は無い
- 答えの欄名は DR-0012 / DR-0020 の語彙を借りるので、意味の正本はそちら。欄を変える時は DR で決め、events / usage と揃える。ccmsg と plugin (別リポ) はこの欄を公開契約として読む。`series` の `route` の欄は控えの名前に合わせており、`routes[].route` / `routes[].credential` は DR-0039 決定 2 の「`route` = 経路名、`credential` = 認証情報名」の分け方に揃えてある。events の `credential` が経路名から認証情報名に分かれる提案 (DR-0039 決定 2) が入れば、全部が同じ名前になる
- DR-0039 の `credential_limited` / `credential_recovered` は本口の `routes` で代替できる (決定 1)。DR-0039 は本 DR の後に、残る候補 (`llm_request` への改名、`credential_refresh_failed`、秘密の寿命の知らせ) だけで再評価する
- `/llm-gateway/events` の SSE はそのまま残す。ccmsg は繋がないが、curl で流れを目で追う確認用 (DR-0012) としての用途は変わらない
- 本口は DR-0043 の署名を求める口に入る (決定 4)。ccmsg が tailnet の外に居るなら、Caddy が外へ通す path に `/llm-gateway/sessions*` が加わる (DR-0041 決定 6 が JWKS だけに絞った線は、署名で守られた口の分だけ広がる。どの口を通すかは DR-0043)
- `/self` との役割分担: `/self` は「走っているこのプロセス」、`/sessions` は「unit を跨いで共有する控え」(とこの unit の印)。`daemon status` は `/sessions` を読まない
- 問いのたびに控えのファイルを読む (1 系列 8MB まで、DR-0027)。使い方は「hook のたびに 1 回」(決定 6) で、会話の turn の数に比例する。本文を読まずに欄だけ取る読み方 (メタと本文の分離) は実装の都合で、契約には現れない
- MANUAL の「運用系」に 2 節 (`GET /llm-gateway/sessions`、`GET /llm-gateway/sessions/{session_id}`) を足し、「keepalive の運用」の pause の段から参照する。「namespace」の節に、gateway 自身の口は面の下に無いことと根の導き方 (決定 6) を 1 行足す
- `keepalive` 戦略を書いていない面では、答えは常に `series: []`。読む側は空を「繋いでいない」と読み、状態の無い表示 (残り無し) に落とす。控えが無い会話では `routes` も載らないので、その会話の経路の枠を知りたい側は usage の `denials` (全経路分) を読む
- DR-0032 が候補に挙げた pause の制御 socket への移設は、HTTP の pause の使い手 (ccmsg) が増えることを踏まえて判断する (決定 5)

## 未決

(無し。起草時に次を決めた:

- `next_keepalive_at` は `Keepalive::chain` の格子の値ではなく、控えの実際の予定 (`fires_at`) を出す。見送り (経路が塞がっていた) の後は格子より先に進んでおり、「次にいつ送るか」の答えは控えの方 (決定 1 の部品)
- `cache_expires_at` は余裕 (30 秒) を引いた控えの値をそのまま出し、知らせの値と 30 秒ずれることを表に書く。gateway が実際に比べる値の方が「状態」として正しく、2 つ目の時刻を足して並べない (決定 1 の部品)
- 答えでは期限の判定をしない。判定は送る直前の 1 か所に置いたまま (DR-0024) で、読む側が壁時計と比べる (決定 1 の部品)
- `routes` は系列ごとに置き、載せる印は `limited` だけ、`{until}` の 1 欄で `reason` / `model` は持たない (`model` は系列の欄で決まっている) (決定 1 の部品)
- 一覧の絞り込みは `ns` だけ。モデルや経路で絞るのは受け取った側でできる (決定 2 の部品)
- 口の名前は複数形 `sessions` で、1 件は `sessions/{session_id}`。`login/{name}` と同じ「名前の下に個別」の形に揃える (決定 1・2 の部品))

## 関連

- [DR-0043](DR-0043-authenticate-operations-callers-by-their-jwks.md) — 運用系の口の呼び手の認証 (本口が最初の対象、決定 4)
- [DR-0012](DR-0012-request-events.md) — 欄名と意味の正本 (`prefix` / `cache_notice` / `cache_since` / `cache_count` / `next_keepalive_at` / `cache_until` / `cache_until_count`、Unix ミリ秒)
- [DR-0020](DR-0020-denial-reason-visibility.md) / [DR-0009](DR-0009-credential-denial-fallback.md) — 経路の印と usage の `denials` の形 (`routes` の `limited` の出所)、印はメモリだけに持つこと
- [DR-0027](DR-0027-keepalive-by-replay.md) — 控えの置き場と中身 (決定 3)、本文を持つこと (決定 4)、sub の系列 (決定 5)、乗った 1 本だけ控える (決定 8)
- [DR-0024](DR-0024-cache-strategy-and-keepalive.md) — 55 分の格子と横断条件、壁時計での期限判定、pause API
- [DR-0006](DR-0006-namespace-routing.md) — gateway 自身の口を `/llm-gateway/` の直下にまとめ、面の下には置かない規約
- [DR-0028](DR-0028-daemon-service-subcommands.md) / [DR-0038](DR-0038-timed-request-dump.md) — `/self` が答える範囲 (プロセスの状態、動いているダンプ)、Caddy の `lb_policy first`
- [DR-0032](DR-0032-daemon-reload.md) — 外から届く面に台の挙動を変える口を置かない方向と、pause の移設の候補
- [DR-0011](DR-0011-daily-usage-stats.md) / [DR-0029](DR-0029-stats-origin-axis.md) — 会話の軸を stats に持たない理由
- [DR-0039](DR-0039-credential-events-and-llm-request.md) / [DR-0040](DR-0040-hooks-deliver-events-to-urls.md) / [DR-0041](DR-0041-hooks-sign-deliveries-with-ephemeral-jwks.md) — 押す側の提案。ccmsg の用途では使わず、`credential_limited` / `recovered` は本口で代替する
