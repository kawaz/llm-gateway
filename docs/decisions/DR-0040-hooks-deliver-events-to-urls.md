# DR-0040: `[[hooks]]`: 選んだイベントを整形して URL へ配達する (`[webhook]` は廃止)

- Status: Proposed
- Date: 2026-10-21

## 背景

通知を出す口が無い。`[webhook]` (DR-0012) は ccmsg の受け口 1 つのための形で、送り先に `/webhook/llm-gateway` を固定で足し、合言葉の既定パスに ccmsg の名前が入っている (`config.rs` の `default_token_file`)。gateway が受け手を知っている密結合で、別の受け手 (bark 等の通知) に同じイベントを流すには受け手ごとに同じ形の口を増やすことになる。

kawaz の要件 (2026-10-21) のうち、本 DR が扱うのは 2 つ。gateway 自身が選んだイベントで URL を叩く `[[hooks]]` を足して通知 (bark 等) と ccmsg の配達を同じ枠組みに載せること、テンプレートはプアに固定すること (`{{field}}` / `{{field.field2}}` / `{{event}}` だけ、if / loop / 整形は持たない、無い欄は黙って空にしない)。hooks は本体を阻害しない (reference `proxy-dump-design` の「実装の原則」と同じ: 透過、非ブロッキング、失敗は log のみ)。

どの events が流れるか (credential の遷移と秘密の寿命の種類、`llm_request` の名前、`route` / `credential` の 2 欄) は [DR-0039](DR-0039-credential-events-and-llm-request.md) が決める。本 DR はその語彙を購読して配達するだけ。

## 決定

### 1. `[[hooks]]`: 選んだイベントを整形して URL へ配る

```toml
[[hooks]]
on = ["credential_limited", "credential_recovered"]   # 配るイベントの種類 (必須、1 つ以上)
match = { route = "claude-*", reason = "limited" }        # 任意: top-level の欄の値で絞る (AND、`*` glob)
url = "https://bark.example/bark-admin/push"           # 必須。テンプレート可
method = "POST"                                        # 既定 POST
headers = { "Content-Type" = "application/json", Authorization = "Bearer {{secret:bark}}" }   # 値はテンプレート可
body = '{"device":{"owner":"kawaz"},"title":"{{route}} is {{reason}}","body":"until {{until}}; next {{fallback}}","group":"llm-gateway"}'   # テンプレート
timeout = "5s"                                         # 既定 5s
follow_redirects = true                                # 既定 true
```

| 欄 | 意味 |
|---|---|
| `on` | 配るイベントの種類。SSE の `event:` 名と同じ語 (`llm_request` (DR-0039 決定 2) / `response` / `cache_expired` / `passthrough` / `credential_limited` / `credential_recovered` / `credential_refresh_failed` (DR-0039 決定 3) / `credential_expiring` / `credential_expired` / `secret_expiring` / `secret_expired` (DR-0039 決定 4))。知らない語は `check` が断る |
| `match` | 任意の絞り。`{ <欄> = "<値>" }` で、イベントの **top-level の欄**の値が一致する 1 件だけ配る。値は完全一致か `*` glob (`chatgpt-*`)、複数の欄は AND。欄は列挙せずどの top-level 欄でも書ける (`ns` / `credential` / `secret` / `upstream` / `reason` / `origin` / `status` …) ので、欄が増えても文法は変わらない。値は**文字列だけ** (TOML の数値 / 真偽は `check` が断る)。文字列以外の欄 (`status` の数値、`aborted` の真偽) は JSON の表記 (`"200"` / `"true"`) で比べ、配列 / object の欄には当たらない。欄を持たないイベント (`cache_expired` に `credential` は無い) は、その欄を書いた hook には当たらない。`credential` はどの種類でも認証情報の名前、経路は `route` (DR-0039 決定 1・2)。`check` は欄名を検証しない (どの種類にどの欄があるかは events の契約で、増える) |
| `url` | 送り先。`http` / `https` だけ。テンプレートを展開した結果を URL として読む |
| `method` | `GET` / `POST` / `PUT` / `PATCH` / `DELETE`。既定 `POST` |
| `headers` | 名前 → 値。値はテンプレート。`Content-Type` を書かなければ `body` がある時だけ `application/json` を付ける |
| `body` | テンプレート。無ければ本文なし |
| `timeout` | 1 回の送信を諦めるまで。既定 `5s` |
| `follow_redirects` | 3xx の `Location` を**同じ method と本文で**追う (既定 `true`、最大 3 回、同じ origin に限る)。cold start で 302 を返す受け口 (bark-admin) を 1 回で通すため。ブラウザの意味論 (POST の 302 を GET に変える) は採らない — 送り先は設定で決めた 1 つの受け口で、結果ページへ移る用途が無い。別 origin への `Location` は追わず失敗にする (合言葉を別の相手に送らない) |

配達の原則 (本体を阻害しない):

- **events の購読者の 1 つ**として走る (決定 5)。転送側は `Events::publish` で流すだけで、hook の有無も結果も知らない
- **hook ごとに別タスクと bounded queue**。購読タスクが受けた 1 件を、当たる hook の列 (既定 64 件) へ入れる。列が一杯ならその 1 件を**落として数え**、転送も購読も待たない。溢れるのは受け口が遅い時で、遅れて届く知らせに価値が無いのは SSE と同じ (DR-0012)
- 落とした数は `/llm-gateway/self` の `hooks.dropped` (全 hook の合計、起動からの累積、0 でも欄は出す) と `daemon status` の各行に出る。購読の段で追いつけずに落ちた分は従来どおり `events.dropped` に入る (2 段で数える: 購読の遅れと配達の遅れは別の事象)
- **失敗は log だけ**。2xx 以外の応答、通信失敗、timeout、テンプレートの展開失敗はすべて warning で、再送の仕組みは持たない。例外として**通信失敗と 5xx と timeout は 1 回だけ送り直す** (bark-admin の cold start が 1〜2 秒かかる実測に合わせる。4xx は同じ結果になるので送り直さない)。連続失敗は最初の 1 回だけ warning を出し、復帰時に失敗回数を記録する (`webhook.rs` と同じ形)
- **log に合言葉と本文を出さない**。展開前のテンプレート (`{{secret:…}}` のまま) と状態コードまで。受け口の応答本文も記録しない
- 1 件 1 配達。複数件をまとめる窓は持たない (「採らなかった案」)。接続は reqwest の pool で使い回す
- `[[hooks]]` は `daemon reload` で差し替えられる (購読タスクを止めて組み直す)。配達の途中の 1 件は送り切る

### 2. テンプレートの文法

`url` / `headers` の値 / `body` に書ける。文法は 3 種類の置き換えだけで、if / loop / 整形 / 既定値は持たない:

| 書き方 | 埋まるもの |
|---|---|
| `{{name}}` | イベントの欄 `name` の値 |
| `{{a.b}}` | 入れ子の欄 (object の中の object)。配列は辿らない (`{{fallback.0}}` は無い欄) |
| `{{event}}` | イベント丸ごと (JSON object)。`event` は予約語で、イベントの欄名には使わない |
| `{{secret:<id>}}` | `[secret_store]` の `<id>.json` の値 (`[upstreams] secret = "<id>"` と同じ id、同じ置き場、同じ最小形 `{"type":"static","payload":{"value":"…"}}`)。発火のたびに読む (書き換えは次の発火から効く、DR-0010 の版の照合) |

**埋め方は置かれた文脈で決まる**。`body` は、テンプレートの各 `{{…}}` を `null` に置き換えたテキストが JSON として読めるなら **JSON 文脈**:

- JSON の文字列リテラルの中に置かれた `{{…}}` は、値を**文字列にして JSON エスケープ**して埋める。文字列はそのまま、数値 / 真偽は 10 進 / `true` `false`、`null` は `null` の 4 文字、配列 / object はその JSON テキスト
- 文字列の外 (値の位置) に置かれた `{{…}}` は、値を **JSON 値として**埋める。文字列なら引用符付き、数値はそのまま、配列 / object はその JSON
- `body` 全体が `{{event}}` なら、イベントの JSON がそのまま本文になる (生 POST)。`[{{event}}]` なら 1 件の配列

`null` に置き換えたテキストが JSON として読めなければ**素の文脈**で、どの `{{…}}` も値を文字列にして (エスケープなしで) 埋める (文字列はそのまま、他は JSON テキスト)。`url` と `headers` は常に素の文脈で、URL エンコードも持たない (未決 3)。

**無い欄はエラー**。イベントにその欄が無い (`cache_expired` に `credential` は無い、`fallback` は契機が枠照会の時は無い、`until` は `recovered` に無い) か、`{{secret:<id>}}` が読めなければ、その hook の**その 1 件は発火せず** warning に残す (どの hook の、どのイベントの、どの欄が無かったか)。黙って空文字にすると、受け手が「値が空だった」と「テンプレートが間違っている」を区別できない。欄ごと出ないことのある欄 (DR-0012 の `skip_serializing_if` の欄) をテンプレートに書くと、その欄の無い 1 件は配達されない。全部の欄が要るなら `{{event}}` で丸ごと送る。

`{{` を文字として書く手段は持たない。閉じの無い `{{`、知らない予約語 (`{{foo:bar}}`) は `check` が断る。

### 3. `[webhook]` の廃止と ccmsg の受け口の書き方

`[webhook]` を設定から取り除く。設定に残っていれば `check` (と `daemon reload` の検証、起動時の読み込み) が **`[[hooks]]` への書き換え例を出して断る** (`[secrets] type` / `dir` の移転と同じ形、`config.rs` の `secret_specs`)。1 リリースの猶予を置くかは未決 1。

ccmsg の受け口は hooks でこう書く:

```toml
[[hooks]]
on = ["llm_request", "response", "cache_expired", "passthrough"]
url = "http://127.0.0.1:8643/webhook/llm-gateway"
headers = { Authorization = "Bearer {{secret:ccmsg-webhook}}" }
body = "[{{event}}]"
```

- 送り先はパスまで書く。gateway がパスを足さない
- 合言葉は `[secret_store]` の `ccmsg-webhook.json` に置く (`{"type":"static","payload":{"value":"<token>"}}`)。1 行テキストのファイルを直接指す形は持たず、秘密の置き場を 1 つにする
- `[{{event}}]` で受け口の契約 (常に配列、DR-0012) を保つ。1 件 1 配達になるので、短時間に起きた分が 1 通にまとまることは無くなる (各件が自分の `seq` / `boot` を持つのは同じ)
- 複数の受け口は `[[hooks]]` を受け口の数だけ書く。全部へ同じ知らせを流し gateway は宛先を選ばない、1 つが落ちていても他への配達を止めない、はそのまま成り立つ (hook ごとに別タスク)

### 4. bark への通知の例

DR-0039 の `credential_limited` / `credential_recovered` / `credential_refresh_failed` / `credential_expiring` / `credential_expired` を `on` に書く例:

```toml
[[hooks]]
on = ["credential_limited"]
match = { reason = "limited" }
url = "https://bark.example/bark-admin/push"
body = '{"device":{"owner":"kawaz"},"group":"llm-gateway","title":"{{route}} ({{credential}}): {{reason}}","body":"until {{until}}, next: {{fallback}}","level":"timeSensitive"}'

[[hooks]]
on = ["credential_recovered"]
match = { reason = "limited" }
url = "https://bark.example/bark-admin/push"
body = '{"device":{"owner":"kawaz"},"group":"llm-gateway","title":"{{route}} recovered","body":"{{by}}"}'

[[hooks]]
on = ["credential_refresh_failed"]
url = "https://bark.example/bark-admin/push"
body = '{"device":{"owner":"kawaz"},"group":"llm-gateway","level":"timeSensitive","title":"{{credential}} needs login","body":"{{reason}}; routes {{routes}}","url":"https://gw.example{{login_path}}"}'

[[hooks]]
on = ["credential_expiring", "credential_expired"]
url = "https://bark.example/bark-admin/push"
body = '{"device":{"owner":"kawaz"},"group":"llm-gateway","title":"{{credential}}: {{type}}","body":"expires_at {{expires_at}}; routes {{routes}}"}'
```

- 1 つ目の `{{credential}}` は認証情報を持たない経路 (relay) では無い欄なので、その経路の 1 件は発火しない (決定 2)。relay も鳴らすなら `{{route}}` だけにする
- `reason` を絞るのは、混雑 (`busy`) の 60 秒の退避で鳴らさないため。`org_not_allowed` も鳴らしたいなら `[[hooks]]` をもう 1 つ書く (`match` の値は 1 つで、OR は hook を分けて表す)
- 再ログインの催促は bark の `url` (タップで開く) に login ページを入れる。gateway は自分の外向きの URL を知らない (`login_path` はパスだけ、DR-0039 決定 3) ので、ホストは hook を書く人が前に付ける。`login_path` の無い認証情報 (codex_oauth、DR-0023 の対象外) ではこの hook は発火しない (決定 2) ので、codex も鳴らしたいなら `url` を外した hook を別に書く
- `fallback` は JSON 文字列の中に置かれているので `["claude-b","claude-c"]` のテキストが入る。枠照会が付けた印 (`by: "probe"`) には `fallback` が無く、この hook のその 1 件は発火しない (決定 2)。欠けても鳴らしたいなら `fallback` を本文から外す
- `until` は Unix ミリ秒のまま入る。人が読む形への整形はテンプレートに無く、イベントにも載せない (DR-0012 の「同じ瞬間を 2 通りで並べない」)。通知で読める時刻が要るかは DR-0039 の未決 3
- bark-admin の cold start の 302 は `follow_redirects` (既定) が同じ POST で追う

### 5. events (SSE) は変えず、hooks は購読者の 1 つ

`/llm-gateway/events` の形・配り方 (`tokio::sync::broadcast`、256 件、追いつけない見物人の分は落とす) は変えない。新しい 7 種 (DR-0039 決定 1・3・4) は `Notice` の種類として増え、SSE にも同じ `seq` で流れる。events の語彙が正本で、hooks はその上の配達 (絞り + 整形 + 送信) に徹する。hooks のためにイベントの欄を足す・変えることはしない — hook で要る値は、まず events の契約として正しいかを問う。

## 採らなかった案

- **`[webhook]` を残して `[[hooks]]` と併存させる**: 同じイベントを URL へ送る口が 2 つになり、合言葉の読み方・失敗の数え方・timeout を両方で保守する。`[webhook]` は `[[hooks]]` の 1 つの書き方 (決定 3) で表せるので、残す理由が無い
- **テンプレートに if / loop / 整形 (時刻の書式、配列の結合) を持つ**: 文法が増えるほど `check` で検証できる範囲が狭まり、展開の失敗が増える。整形は受け手 (bark の本文は人が読む、ccmsg は JSON を読む) の仕事で、gateway が持つと受け手の数だけ要望が積もる
- **無い欄を空文字にする**: 受け手が「値が空だった」と「テンプレートの欄名が間違っている」を区別できない。設定の誤りが黙って通る口を作らない
- **配達を同期にする (転送の中で送る)**: 受け口の遅れが転送の遅れになる。DR-0012 の「転送の邪魔をしない」のとおり、配達は切り離す
- **ccmsg 側で bark に流す (gateway に hooks を持たない)**: 通知の経路が ccmsg の生死に依り、ccmsg が落ちている時に限って「経路が尽きた」知らせが届かない。gateway 自身が知っていることは gateway から出す
- **複数件を 1 通にまとめる窓 (`[webhook]` の 200 ms / 100 件) を hooks に持つ**: テンプレートが 1 件の形で書かれているので、まとめるには配列の文法 (loop) が要る。1 件 1 配達で接続は増えるが、pool で使い回し、落とす数は `hooks.dropped` で見える
- **hook に名前 (`name`) を持たせる**: log と `/self` の識別は `url` (展開前) で足りる。識別のためだけの欄を足さない
- **hooks の絞りを `ns` / `credential` / `reason` の固定の欄で持つ**: 欄が増えるたび (`secret` / `upstream` / `origin` …) に設定の文法が増える。top-level の欄に対する一致 (`match`) なら events の語彙が増えても文法は変わらない (決定 1)

## 影響

- `webhook.rs` と `config::Webhook` が消え、`hooks.rs` と `config::Hook` が増える。`[webhook]` を書いた設定は `check` で断られ、書き換え例が出る。ccmsg の合言葉ファイル (`webhook-llm-gateway.token`) は `[secret_store]` の `ccmsg-webhook.json` へ写す (運用の手順、MANUAL に書く)
- `[secret_store]` は `[upstreams]` が空だと開かない (`passthrough.rs`)。`{{secret:…}}` を使う hook があれば開く
- `/llm-gateway/self` と `daemon status` の各行に `hooks.dropped` が増える。`[[hooks]]` は `daemon reload` で変えられる欄にする (`[webhook]` は restart を要する欄)
- 1 件 1 配達になるので、受け口への接続数は `[webhook]` の窓でまとめていた時より増える。ccmsg の受け口は配列を受けるのでそのまま動く
- MANUAL の webhook の節を hooks に置き換える。`docs/architecture-overview` の webhook の記述も同様

## 未決

1. **`[webhook]` の猶予**: 1 リリースの間は読み替えて警告だけにするか、最初から断るか。読み替える場合 `token_file` の 1 行テキストを `[secret_store]` へ写す処理を持つか
2. **`match` で `prefix` / `session_id` を絞る使い方を認めるか**: 文法上は top-level の欄なので書けるが、会話ごとに別の受け口へ、は DR-0012 の「gateway は宛先を選ばない」と逆の形になる。`check` で断るか、書けるが勧めない (MANUAL に書かない) か
3. **dotted path で配列の添字 (`{{fallback.0}}`) を許すか**、`url` に埋める値の URL エンコードを持つか
4. **hook ごとの列の長さ (既定 64)** と、`hooks.dropped` を hook ごとに出すか合計だけか

## 関連

- [DR-0039](DR-0039-credential-events-and-llm-request.md) — hooks が購読する events の語彙 (credential の遷移と秘密の寿命の種類、`llm_request`、`route` / `credential` の 2 欄)
- [DR-0012](DR-0012-request-events.md) — events の契約 (`seq` / `boot`、Unix ミリ秒、未知の `type` は無視、転送の邪魔をしない)。`[webhook]` の決定はこの DR で置き換わる
- [DR-0030](DR-0030-general-purpose-auth-gateway.md) §3 / §5 — `[secret_store]` と秘密の id (`{{secret:<id>}}` の参照先)
- [DR-0010](DR-0010-credential-cross-process-lock.md) — ファイルの書き換えと版 (`{{secret:<id>}}` の読み直し)
- [DR-0032](DR-0032-daemon-reload.md) — reload で差し替えられる欄の区分
- claude-rules-personal の reference `proxy-dump-design` (`model` の「実装の原則」) — 透過 / 非ブロッキング / 失敗は log のみ
- claude-rules-personal の reference `bark-push` — bark-admin の送り方 (cold start の 302、宛先は `device` タグ)
