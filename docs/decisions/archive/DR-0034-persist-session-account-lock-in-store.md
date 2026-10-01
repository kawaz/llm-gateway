# DR-0034: session の開始 account と「跨いだ」印を Store 層に永続化し、restart と unit 間で共有する

- Status: Superseded by [DR-0035](../DR-0035-judge-thinking-crossing-by-session-sources.md) (2026-10-01)。それまでは Accepted (kawaz 裁定 2026-09-30)、実装済
- Date: 2026-09-30

## 文脈

DR-0033 は thinking が account に束縛されるモデルの session を開始 account にロックし、`thinking_as_text` で跨いだ session には以後ずっと thinking を text として送る。その状態 (`AccountLock` = 開始 account と跨いだ印) は今、affinity の値 `Binding` にメモリで持っている (DR-0033 §4)。

この置き方には穴が 2 つある。

| 穴 | 起きること |
|---|---|
| 1 時間の沈黙 | 寿命が affinity と同じ `AFFINITY_TTL` (最後に通ってから 1 時間) なので、1 時間黙った session はロックと印を忘れる |
| restart / unit の移動 | メモリなので restart で消える。stable / unstable の 2 unit は別メモリで、同じ session が別の unit に届くとそちらは何も知らない。reload でも経路の変わった Binding ごと捨てる |

どちらも結果は同じで、次の 2xx を返した account に開始 account を決め直す (再ロック)。再ロック先が以前と別の account なら、履歴に載った以前の account の thinking は API に黙って捨てられ、`thinking_as_text` を選んでいても text にならない。跨いだ印を忘れた session は「跨いでいない session」として本文を素通しするため。DR-0033 の「影響」の「ロックが外れる場面」が指したのはこの穴で、本 DR で塞ぐ (元は issue `2026-09-30-persist-session-account-lock-across-restart-and-units`)。

## 決定

### 1. 永続化するのは `{ns, session, model, account, crossed, seen}` だけ。affinity は永続化しない

| 欄 | 中身 |
|---|---|
| `ns` / `session` / `model` | 鍵。affinity と同じ `(namespace, session key, 解決後のモデル名)` |
| `account` | 開始 account (credential 名、relay は経路名。DR-0033 §2) |
| `crossed` | 開始 account 以外へ送ったことがあるか。一度立てたら消さない |
| `seen` | 最後にこの session が 2xx で通った時刻 (Unix 時刻)。寿命の起点 |

- affinity (前回通った経路名を先頭へ寄せる優先、`Binding.route`) はメモリのまま寿命 1 時間で持ち、永続化しない。経路名の優先は prompt cache の都合で、失っても設定順に落ちるだけで正しさは壊れない。ロックは失うと thinking が黙って落ちる
- よって `Binding` から `account` / `crossed` を外し、ロックは affinity とは別の表として持つ。鍵は同じ
- 対象は DR-0033 §1 の `account_bound_thinking` に当たるモデルの session だけ。他のモデルの session のレコードは作らない

### 2. 置き場は Store 層の 1 品目。第一版は共有ファイル 1 つ + `.lock` の flock

- 置き場は `[stats] dir` 配下の `account-lock/` に `locks.json` と `locks.json.lock` を置く。`[stats] dir` は keepalive の控え (DR-0027 決定 3) と同じく兄弟の unit が共有する走行状態の置き場で、既定は `$XDG_STATE_HOME/llm-gateway/stats`。`[store] dir` は credential の置き場 (`[store] type = "file"`) で、credential 以外を混ぜない
- 形式は **1 ファイル 1 map** (JSON オブジェクト、鍵ごとに 1 レコード)。書き込みは既存の `write_atomically` (一時ファイル + fsync + rename) で丸ごと差し替える。jsonl の追記は採らない: 同じ鍵の更新が行として溜まり、読むたびに畳む処理と追記ファイルの切り詰めが要る。1 map なら読み手は mtime を 1 回見れば足りる (決定 4)
- 書き換えは DR-0010 と同じ形: `.lock` を flock で掴む → 最新を読み直す → 自分の変更を当てる → 書く → 手放す。`.lock` は消さない (本体は rename で inode が変わるので、本体ではなく脇のファイルを掴む)
- 読み直した上での当て方 (両 unit の書き込みが交差しても意味が崩れない規則):
  - 鍵が無い、または寿命切れ → 自分の account を開始 account として入れる
  - 鍵がある → 開始 account は **先に決まった方** (レコードの `decided_at` = 開始 account が決まった時刻が早い方) が勝つ (first-writer-wins を「先に書いた方」でなく「先に決まった方」と定義する。unit の内でも間でも、決まった順と flock を取る順は入れ替わりうるため。unit の内の書き込みは 1 本の書き手が FIFO で流す)。両 unit は同一ホストの時計を使う前提で、`decided_at` の比較は unit 間の時計ずれを考えない
  - 鍵がある → `crossed` は OR (開始 account が食い違っていれば遅く決めた側は跨いだことになるので真)、`seen` は大きい方
- 両 unit が同じファイルを見るので、restart と unit の移動を跨いでロックと印が残る

### 3. 寿命は `seen` から 24 時間。刈り込みは書き込みのついでに行う

- 寿命切れのレコードは読み手が無いものとして扱う。`crossed` は寿命内なら保持する (寿命内に消える経路は無い)
- 刈り込みは**書き込む側が flock の内側で**行う: 書き込みのたびに寿命切れのレコードを落としてから書く。担当 unit は決めない (どちらの unit も同じ規則で刈るので、先に書いた方が刈る)
- 起動時には刈らない。起動時の読み込みは読み取りだけにして、寿命切れは読み手の側で無視する。書き込みが 1 本も無い間はファイルに寿命切れが残るが、読み手には見えない

### 4. 読みはメモリの控え + mtime、書きは状態が変わった 2xx だけ

- **読み** (リクエストごと、`account_lock`): メモリに控えた map を引く。控えにはファイルの版 (mtime のナノ秒・inode・長さ。DR-0010 の mtime に、同じナノ秒内の差し替えを見分ける inode を足す) を添え、引く前に版を見て変わっていれば読み直す。credential の `FileStore::version` と同じ考え方で、1 本あたりの仕事は `stat` 1 回
- **書き** (2xx で `remember` が呼ばれた時): 次のどれかに当たる時だけ書く
  - 開始 account が決まった (鍵が無い / 寿命切れ)
  - `crossed` が偽から真になった
  - `seen` が前に書いた値から 5 分以上進んだ (`seen` の更新は間引く。寿命 24 時間に対して最大 5 分の誤差)
- メモリの控えはその場で更新し、ファイルへの書き込みは送信の経路を待たせないよう blocking の仕事として裏で行う。同じ unit の次の 1 本はメモリの控えを見るので、書き込みの完了を待たない
- flock を持つ区間は「読み直し → 当てる → 書く」だけで、送信や upstream の待ちを含めない
- **失敗は best-effort**: 読めなければメモリの控えのまま進み、書けなければ警告を残して進む (次に状態が変わった 2xx で書き直す)。ロックは推論の連続性を守る仕組みで、置き場の障害で転送を止めるほど重くない

### 5. 起動時に読み込み、reload ではファイルが正

- 起動時 (`Router` の組み立て時) にファイルを読んでメモリの控えに載せる。以後は決定 4 の版の照合で追う
- reload (DR-0032) は affinity を今までどおり「経路名 + credential + provider が同一の経路」の分だけ引き継ぎ、ロックの表には触らない。ロックは経路でなく account (credential 名) に結ばれていて、経路表を差し替えても意味が変わらないため。DR-0033 §4 の「寿命は affinity と同じ」「読み直しの引き継ぎも affinity と同じ規則」は本 DR で置き換わる
- メモリの控えとファイルが食い違ったら、次に版が動いた時点でファイルの内容に合わせる (別 unit が先に開始 account を入れていた場合がこれに当たる)

### 6. 観測: status にも events にも新しい欄は足さない

- 跨いだ 1 本は既に events の `request` に `thinking_as_text` / `thinking_dropped_by_switch` が出る (DR-0033 §5)。ロックが永続化されても出るべき 1 本は変わらない
- 置き場の読み書きの失敗は tracing の警告に出す (他の store 品目と同じ)
- `daemon status` にロックの件数などは載せない。見たければファイルそのもの (`account-lock/locks.json`) を読めば足りる

## 却下した案

- **affinity ごと永続化する**: 経路名の優先は prompt cache の都合で、1 時間で捨ててよい状態。永続化すると request ごとに経路名の更新を書くことになり、書き込みの頻度が 2xx ごとに跳ね上がる。reload の引き継ぎ規則 (経路が変わったら捨てる) もファイル側に持ち込むことになる
- **`AFFINITY_TTL` を延ばすだけ / ロックの寿命だけメモリで延ばす**: 1 時間の沈黙は塞げるが、restart と unit の移動は塞げない
- **Caddy 等の前段の sticky で session を 1 unit に寄せる**: unit の移動は減るが restart は塞げず、session key (本文や header から gateway が導く、`session.rs`) を前段が知らない。session の状態は gateway の責務
- **unit (writer) ごとのファイルに書いて読む時に merge する (DR-0031 (3) の形)**: `crossed` の OR と `seen` の最大は merge で作れるが、読むたびに全 writer のファイルを開いて merge することになり、判定の場所が読み手の数だけ増える。1 ファイルを flock の内側で更新すれば判定は 1 箇所で済み、`decided_at` の比較もそこだけで閉じる

## 未確定

- Store 層の interface を切るタイミング: issue `2026-09-15-store-layer-for-replaceable-persistence` / DR-0031 の進み次第。第一版はファイルを直に読み書きする実装を router の外 (1 モジュール) に置き、interface は後から切る。DR-0031 の 4 意味論のどれに載せるか (「掴む → 読み直す → 書く」は (1) の形だが、失敗の扱いは best-effort で (1) の fail-closed と違う) もその時に決める
- credential 名を設定で付け替えた時: 旧名を開始 account に持つレコードは、新しい設定のどの経路とも一致しないので、次の 1 本は `on_account_switch` に従って跨ぐ扱いになる。実害は「跨いだ扱いが 1 度増える」だけと見て、特別な扱いは置かない

## 影響

### 実物照合

| 箇所 | 今 | この DR で要ること |
|---|---|---|
| `AFFINITY_TTL` (`crates/llm-gateway/src/router.rs:37`) | affinity とロックの共通の寿命 1 時間 | affinity だけの寿命になる。ロックの寿命 24 時間を別の定数で持つ |
| affinity の表と `Binding` (`router.rs:401`, `:406-416`) | `Binding { route, seen, account, crossed }` | `account` / `crossed` を外し、`{ route, seen }` に戻す。ロックは別の表 (メモリの控え + ファイル) |
| `AccountLock` (`router.rs:421-427`, `crossing` は `:439-456`) | `Binding` から組み立てる | 形はそのまま。組み立て元がロックの表になる |
| reload (`router.rs:493-512`) | affinity を引き継いだ経路の分だけ残す (ロックも一緒に消える) | affinity は同じ。ロックの表には触らない (決定 5) |
| 候補の並べ替え (`router.rs:824-850`) | `Binding.account` で開始 account の経路を先頭へまとめる。affinity が寿命切れならロックも効かない | 開始 account はロックの表から引く。affinity (経路名の優先) が無くてもロックは効くようにする |
| `remember` (`router.rs:995-1035`) | affinity の錠の内側で開始 account と印をメモリに書く | affinity の更新に加え、ロックの表のメモリを更新し、決定 4 の条件に当たればファイルへの書き込みを裏で出す |
| `account_lock` (`router.rs:1038-1053`) | affinity を引き、寿命 1 時間で判定 | ロックの表を版の照合つきで引き、寿命 24 時間で判定 |
| 呼び出し側 (`crates/llm-gateway/src/gateway.rs:593-597`, `:708`) | `account_lock` / `remember` を await | 変えない (中身だけ変わる) |
| `Router` の組み立て (`gateway.rs:170`、keepalive の置き場は `:173-178`) | `Router::new(config, events)`。keepalive は `config.stats.resolve_dir()` を渡す | ロックの置き場 (`config.stats.resolve_dir()` の下) を渡し、起動時に読み込む |
| 置き場の先例 (`crates/llm-gateway/src/cache/keepalive/store.rs:128-131`, `:145-160`) | `[stats] dir` の下に `keepalive/` を切り、`.lock` を消さずに flock | 同じ流儀で `account-lock/` を切る |
| 原子的な書き込み (`crates/gateway-core/src/persist.rs:44-62`) | 一時ファイル + fsync + rename | そのまま使う |
| 版と排他の先例 (`crates/gateway-core/src/credential/file.rs:44`, `:71-80`, `:141-143`) | `.lock` の flock と mtime ナノ秒の版 | 同じ型 (ロックの表は単一ファイルなので `.lock` も 1 つ) |
| `[stats]` の読み直し (`crates/llm-gateway/src/config.rs:1267-1274`, `:1761`) | `[stats]` は読み直しで変えられない欄 | 置き場が reload で動かないことはこれで保たれる。追加の検査は要らない |

### 書き込みの頻度

ファイルへの書き込みは「束縛モデルの新しい session の初回」「跨いだ瞬間」「同じ session の 5 分ごと」だけで、リクエスト数ではなく session 数に比例する。flock の区間は小さな JSON の読み直しと rename だけなので、2 unit の競合は短い。

### 実装の段分け案

| 段 | 中身 | 完了条件 |
|---|---|---|
| 1 | ロックの表のファイル実装 (1 モジュール): 読み込み、版の照合つきの控え、flock の内側での当て方 (first-writer-wins / OR / 最大) と刈り込み | 試験で、2 つの置き場インスタンス (2 unit 相当) が同じディレクトリを見て、先に入れた方の account が残り、後の方の `crossed` が OR で入り、寿命切れのレコードが次の書き込みで消え、読み手には寿命切れが見えない |
| 2 | router の付け替え: `Binding` から `account` / `crossed` を外し、`remember` / `account_lock` / 候補の並べ替えをロックの表に向ける。書き込みの条件 (決定 4) と裏での書き込み | 試験で、affinity が寿命切れ (1 時間超) でもロックと印が効き、reload で経路が変わってもロックが残り、`seen` の更新が 5 分未満では書かれない。DR-0033 の既存の試験が通る |
| 3 | 起動時の読み込み (`gateway.rs` の組み立て) | 試験で、同じ置き場で作り直した `Router` (restart 相当) が前のロックと `crossed` を引き継ぐ |
| 4 | 実機: stable / unstable の 2 unit で同じ session を交互に通し、restart を挟んでも `thinking_as_text` の変換が続くこと | events の `request` に、restart / unit 移動の後も `thinking_as_text: true` が出続ける |

## 関連

- docs/issue/2026-09-30-persist-session-account-lock-across-restart-and-units.md (本 DR の元)
- docs/issue/2026-09-15-store-layer-for-replaceable-persistence.md (Store 層の品目として)
- [DR-0033](DR-0033-lock-session-to-first-account-and-send-crossed-thinking-as-text.md) (ロックの意味。§4 の寿命と読み直しの規則を本 DR が置き換える)
- [DR-0010](../DR-0010-credential-cross-process-lock.md) (`.lock` の flock と mtime の版)
- [DR-0027](../DR-0027-keepalive-by-replay.md) (兄弟の unit が `[stats] dir` を共有する先例)
- [DR-0031](../DR-0031-store-layer.md) (Store 層の意味論。載せる trait は未確定)
- [DR-0032](../DR-0032-daemon-reload.md) (reload の走行状態の引き継ぎ)
