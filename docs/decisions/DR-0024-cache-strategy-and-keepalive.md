# DR-0024: prompt cache 戦略の設定と idle keepalive

- Status: Accepted
- Date: 2026-09-03

## 文脈

過去 7 日の全セッション (142 件、15,776 リクエスト) を Fable 5.1 単価で集計すると、
費用の 6 割強がキャッシュ再構築 (5 分 TTL 失効後にプレフィックス全量を書き直す
write) で、その 66% は「失効から 60 分以内の再開」だった。トークン量では read が
93% だが、write 単価は read の 50 倍 (Fable 5.1) なので費用は write が支配する
(`scripts/cache-cost-sim.py`、docs/knowledge/2026-09-02-prompt-cache-and-thinking-facts.md)。

実験で確定した前提 (2026-09-03):

- 5 分でキャッシュ済みのプレフィックスに `ttl: "1h"` を付けてヒットさせても TTL は
  昇格しない (課金も効果もゼロ)。1h エントリはミス時に 2 倍単価で書いた時だけ生まれる
- 差分 > 0 の新しいブレークポイントに 1h を付ければ、その時点の全プレフィックスを
  覆う 1h エントリができる (write 課金は差分のみ)
- 拒否応答 (`stop_reason: refusal`) はキャッシュを残さない
- Claude Code は 5m のみ・明示ブレークポイント 3 個 (system×2 + 末尾 user) を使う

## 決定

### 1. namespace 単位で cache 戦略を設定する

routing と同じ「モデル glob + 先勝ち」の並びで、**main / sub の 2 軸 × 戦略** を書く:

```toml
[[ns.personal.cache]]
models = ["claude-fable-5-1*", "claude-mythos-5-1*"]
main = "keepalive"
keepalive_horizon = "12h"

[[ns.personal.cache]]
models = ["*"]
main = "keepalive"
sub = "passthrough"
keepalive_horizon = "8h"
```

戦略の語彙 (main / sub 共通):

| 値 | 動作 |
|---|---|
| `passthrough` | 本文に触らない (既定。Claude Code なら 5m) |
| `none` | `cache_control` を全て剥がす (再利用しない one-shot 用。write 割増 1.25 → 1.0) |
| `5m` | 全ブレークポイントの ttl を 5m に強制 |
| `1h` | 全ブレークポイントの ttl を 1h に強制 (差分 write が 2 倍、60 分以内の再開が read になる) |
| `keepalive` | 本文は 5m のまま。idle を検知してマーカー request を誘発し、その request だけ 1h を付ける (§2)。**main のみ受理**、sub に書いたら設定エラー |

照合は alias 解決後のモデル名。main / sub の判定は anthropic 方言の preset が
`metadata.user_id.parent_session_id` の有無から `origin` (main / sub / unknown) を決め、
core はその値だけを見る (DR-0014 の境界。unknown は main 扱い)。判定は
Anthropic Messages 形式を話す preset 全て (公式 / Bedrock / relay) が持つ —
読む対象は upstream ではなくクライアントの本文なので、経路を切り替えても
同じ 1 本が別の戦略に落ちない。他方言 (openai) は unknown = main 扱い。
そちらも変換前の本文を読めば判定できる余地はあるが、prompt cache の語彙が
違うので、必要になってから決める。

書き換えるのは `cache_control` だけで、**ブレークポイントは増やさない・
動かさない**。適用は各経路へ送る直前 (モデル名の書き換えの直後) で、
純粋関数として本文を整える — 経路を何度試しても同じ本文が出ていく。
効かせた戦略は tap の `cache_strategy` に出る。

### 2. keepalive の仕組み

1. main の実リクエストを転送するたびに、会話系列 (DR-0012 の `prefix` + session_id)
   ごとのタイマーを **送出時刻 + 4 分** で再武装する (debounce)。ツールループ中は
   次のリクエストが数秒で来るので発火しない
2. 系列は**直前の実リクエストが通った先** (namespace / モデル / route) も覚える。
   発火した時点でその route が締め出されている / 候補から外れているなら
   **合図を出さない** (nonce も発行しない)。出しても会話は別の credential へ
   流れ、そこにプレフィックスは無い — 延ばしたい cache には届かず、会話に
   無意味な 1 往復を挟むだけになる。塞がりは解けるものなので見張りは畳まず、
   +4 分で次を試す (`keepalive_horizon` は据え置き)
3. 発火したら nonce (32 バイトの乱数を base64url) を発行し、受け口 (DR-0012 の
   webhook / SSE) へ `cache_keepalive {type, ts, ts_iso, session_id, prefix,
   nonce, deadline, deadline_iso, marker}` を流す。deadline = 直前の送出時刻 +
   その本文が残した cache の寿命 − 30 秒。受け手 (ccmsg) が `marker` をその
   セッションへ注入する (`notify --as-session`)。文面は
   `[llm-gateway cache keepalive nonce=<nonce>] Reply with exactly this token and
   nothing else: LLMGW-KEEPALIVE-<nonce>` — 返る形が決まっていれば、受け取った
   側がその 1 行を畳んで見せずに済む (空白を含まないので 1 つの語として拾える)。
   「何も出力するな」とは頼まない: 自分の振る舞いについての指示は完全には
   従わせられず、断り書きが 1 行返ってきた (実測)
4. **最後の user メッセージのどれかの text ブロックがマーカーを含む** request が
   来たら、nonce を単回消費する。先頭一致では見ないのは、合図が
   `[SYSTEM NOTIFICATION …]` に包まれて届くため。1 時間を付けるのは次の 3 つを
   全部満たすときだけで、外れた分は本文に触らず素通しする:

   | 扱い | 条件 | 理由 |
   |---|---|---|
   | `applied` | 下の 3 つとも外れない | 差分だけが 2 倍単価で、以降 1 時間 read になる |
   | `late` | deadline を過ぎていた | 元の cache は既に消えている |
   | `rerouted` | 直前の実リクエストと別の route へ出ていく | その upstream にプレフィックスが無い |
   | `drifted` | 直前の実リクエストからプレフィックス (`tools` + `system`、`cache_control` を除く) が変わっていた | 変わった位置から先は全部書き直しになる |

   3 つとも**「この往復はどのみち全量 rebuild になる」**を言っている。そこで
   2.0 倍を払うより、1.25 倍で書き直して次の合図で差分だけ 1 時間にするほうが
   安い。route は発火時にも確かめているが (2)、出した後で締め出された分は
   ここでしか捕まえられないので送出直前にもう一度見る
5. 再武装の間隔は**直前の本文が残した cache の寿命**で決まる。1 時間を付けた
   マーカーの後は +55 分、実リクエストと**1 時間を付けなかったマーカー**
   (`late` / `rerouted` / `drifted` = 5 分の cache しか残っていない) の後は +4 分。
   合図の往復は「人が動かした 1 本」ではないので、見張る期間も通った先も
   延ばさない。最後の実リクエストから
   `keepalive_horizon` を超えた系列には合図を継ぎ足さない。合言葉を持たない
   リクエストが来たら pending を捨てる (= 人が戻ってきた)
6. 対象は origin が sub でない、`tools` を持つ (= 会話の本流) リクエストだけ。
   合図の届け先 (`webhook.base_url`) を書いていない設定では合図を出さず、
   `llm-gateway check` が該当 namespace を警告に挙げる
7. 状態はプロセス内メモリ。再起動で消えても次の実リクエストで再武装される

### 3. 損益の根拠

ping 1 回 = プレフィックス全量の read、再構築 1 回 = 同量の 5m write。比は
read 単価で決まる: Fable 5.1 / Mythos 5.1 で 50 倍 (55 分間隔なら ~46 時間分の
ping が 1 回の再構築に相当)、read 0.1 倍のモデルで 12.5 倍 (~11.5 時間)。
`keepalive_horizon` はこの分岐点と「再開される見込み」から人が決める。

週次試算 (main、Fable 5.1): 現状 $4,254 → `1h` $2,620 → `keepalive` $1,691
(想定) / ≈ $4,305 (全マーカーが deadline 超過した最悪、= 現状並み)。
deadline guard が壊れて遅延マーカーに 1h を付けると +$1,620 の損になるため、
guard は実装の生命線。

## 不採用案

- **idle 後に同一本文を replay して 1h を付ける**: 差分ゼロで新エントリが生まれず、
  ヒットしても TTL は昇格しない (実測)。5 分過ぎてからの replay は全量 2 倍書き
- **末尾以外にブレークポイントをずらした replay**: 新しい境界 = ほぼ全量の 2 倍書き
- **「最後になり得ない request」を判定して 5m に落とす折衷**: 判定できるのは
  classifier / count_tokens / 強制 tool_choice 程度で、1h 割増は差分にしか掛からない
  ため節約幅が小さく複雑さに見合わない
- **戦略名を `always1h-main` のように結合名で列挙**: main/sub × 戦略の直交 2 軸で
  表せるので結合名は持たない

## 禁則 (透過 proxy として本文に触る範囲)

触るのは `cache_control` の `ttl` (と `none` での除去) のみ。thinking / system /
tools / messages の中身には触らない — Fable 5.1 は system / tools / 過去メッセージが
変わると過去の thinking 署名が無効化される (キャッシュ無効化より重い)。

## 関連

- DR-0012 (webhook / prefix)、DR-0014 (preset 境界)、DR-0016 (本文に触る際の教訓)
- docs/knowledge/2026-09-02-prompt-cache-and-thinking-facts.md
- scripts/cache-cost-sim.py
