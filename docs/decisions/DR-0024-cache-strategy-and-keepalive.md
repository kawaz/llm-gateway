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

- **`ttl: "1h"` を書いたブレークポイントは 1h エントリしか読まない**。同じ位置に
  5m エントリがあっても素通りし、その位置までを 2 倍単価で全量書き直す。逆に
  5m フラグ (ttl 指定なし) は 1h エントリも読む
- したがって **「実リクエストは 5m、マーカーだけ 1h」は成立しない**。マーカーが
  毎回「前回の 1h 境界から先」を 2 倍で書き直すので、常時 1h より高くつく
  (実機 2026-09-03 05:38 のマーカー: プレフィックス一致・同一 route・5 分以内で
  read 46K / 1h write 664K)
- 5 分でキャッシュ済みのプレフィックスに `ttl: "1h"` を付けてヒットさせても TTL は
  昇格しない (課金も効果もゼロ)。1h エントリはミス時に 2 倍単価で書いた時だけ生まれる
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

`keepalive_horizon` は時間 (`"12h"`、整数 + `h`) か、**分岐時間に対する比率**
(`0.3`、0 より大きい実数) で書く。比率は単価の違うモデルに同じ判断基準を当てる
ための書き方で、分岐時間の求め方は §3。単価表に無いモデルでは時間に直せないので
既定 (8 時間) へ落とし、起動時と `llm-gateway check` で名前を挙げる。

戦略の語彙 (main / sub 共通):

| 値 | 動作 |
|---|---|
| `passthrough` | 本文に触らない (既定。Claude Code なら 5m) |
| `none` | `cache_control` を全て剥がす (再利用しない one-shot 用。write 割増 1.25 → 1.0) |
| `5m` | 全ブレークポイントの ttl を 5m に強制 |
| `1h` | 全ブレークポイントの ttl を 1h に強制 (差分 write が 2 倍、60 分以内の再開が read になる) |
| `keepalive` | `1h` と同じ本文にした上で、idle を検知してマーカー request を誘発し cache を繋ぐ (§2)。**main のみ受理**、sub に書いたら設定エラー |

照合は alias 解決後のモデル名。呼び出し元の判定は anthropic 方言の preset が行い、
core はその値だけを見る (DR-0014 の境界):

| origin | 見分け方 | 当てる戦略 |
|---|---|---|
| `sub` | `metadata.user_id` に `parent_session_id` がある | `sub` |
| `oneshot` | 親を持たず、`system` 先頭ブロックの請求ヘッダの `cc_entrypoint` が `cli` 以外 (`sdk-cli` = `claude -p` 等) | `sub` |
| `main` | 親を持たず、`cc_entrypoint=cli` か請求ヘッダ無し | `main` |
| `unknown` | `metadata.user_id` が無い / 読めない | `main` |

`oneshot` を `sub` 側に寄せるのは、**1 回きりの呼び出しには続きが来ない**から。
続きを当て込んだ扱い (1 時間持たせる・合図を出す) をしても報われず、割増だけが
残る。`sub` に `keepalive` を書けないので、見張りの対象からも自動的に外れる。判定は
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

`keepalive` は **`1h` + idle 時の refresh ping**。本文の扱いは `1h` と同じで
(実リクエストもマーカーの往復も全ブレークポイントが `ttl: "1h"`)、違うのは
会話が止まったときに 1 往復を誘発して cache を次の 1 時間へ繋ぐかどうかだけ。

1. main の実リクエストを転送するたびに、会話系列 (DR-0012 の `prefix` +
   session_id) ごとのタイマーを **送出時刻 + 55 分** で再武装する (debounce)。
   会話が動いている間は次のリクエストのたびに先送りされるので発火しない
2. 系列は**直前の実リクエストが通った先** (namespace / モデル / route) も覚える。
   発火した時点でその route が締め出されている / 候補から外れているなら
   **合図を出さない** (nonce も発行しない)。出しても会話は別の credential へ
   流れ、そこにプレフィックスは無い — 延ばしたい cache には届かず、会話に
   無意味な 1 往復を挟むだけになる。塞がりは解けるものなので見張りは畳まず、
   +55 分で次を試す (`keepalive_horizon` は据え置き)
3. 発火したら nonce (32 バイトの乱数を base64url にした 43 文字) を発行し、
   受け口 (DR-0012 の webhook / SSE) へ `cache_keepalive {type, ts, ts_iso,
   session_id, prefix, nonce, deadline, deadline_iso, marker}` を流す。
   deadline = 最後の実リクエスト送出時刻 + 60 分 − 30 秒。受け手 (ccmsg) が
   `marker` をそのセッションへ注入する (`notify --as-session`)。文面は
   ``[llm-gateway cache keepalive] token=`LLMGW-KEEPALIVE-<nonce>`; reply with a
   single line containing only that token, nothing before or after`` — 合言葉
   (`LLMGW-KEEPALIVE-` + nonce) は文面に 1 度だけ出てきて、送る印・返させる語・
   戻りを探す印の全部を兼ねる。返る形が決まっていれば、受け取った側がその
   1 行を畳んで見せずに済む (空白を含まないので 1 つの語として拾える)。
   「何も出力するな」とは頼まない: 自分の振る舞いについての指示は完全には
   従わせられず、断り書きが 1 行返ってきた (実測)
4. **最後の user メッセージのどれかの text ブロックが合言葉を含む** request が
   来たら、その nonce を単回消費する。ブロックの先頭一致では見ないのは、合図が
   `[SYSTEM NOTIFICATION …]` に包まれて届くため。本文の扱いは普通の 1 本と同じ
   (戦略が 1 時間を付ける) で、知らせに出す語だけを分ける — deadline 内なら
   `applied` (狙いどおり cache が繋がった)、過ぎていれば `late` (cache は既に
   消えていて、この 1 本が書き直す)
5. 再武装は実リクエストでもマーカーの往復でも +55 分。ただし**見張る期間と
   通った先を延ばせるのは実リクエストだけ**で、最後の実リクエストから
   `keepalive_horizon` を超えた系列には合図を継ぎ足さない。合言葉を持たない
   リクエストが来たら pending を捨てる (= 人が戻ってきた)
6. 対象は origin が sub でない、`tools` を持つ (= 会話の本流) リクエストだけ。
   合図の届け先 (`webhook.base_url`) を書いていない設定では合図を出さず
   (= `1h` と同じ振る舞いになる)、`llm-gateway check` が該当 namespace を
   警告に挙げる
7. 見張り (系列 → 通った先 / 次の発火予定 / 期間の終わり / 立ち位置) は
   **置き場にも落とす**。動いている会話なら次のリクエストで張り直るが、
   止まっている会話は誰も張り直さない — そこを繋ぐのが keepalive なので、
   リリースのたびに全部落とすと意味がない。置き場と書き方は日次集計と同じ
   流儀で、`<stats dir>/keepalive/<待ち受け>.json` へ一時ファイル経由で
   差し替える (DR-0011 の「書き手ごとのファイル」)。
   起動時に読み戻し、予定の時刻が**未来ならその残りで、過ぎていても cache が
   生きている間なら即座に**出す。cache が消えた系列と期間の終わった系列は
   捨てる。読めないファイルは warning 1 行で空から始める
8. **出したままの合言葉 (nonce) は落とさない**。再起動を跨いで戻ってきた合図は
   「出した覚えのない合言葉」= `foreign` になり、控えとして吸収される
   (下記の収束規則がそのまま働く)。合言葉を残すと、再起動後に「自分が出した」
   と誤って数えて出し続ける側になり、相方と 2 本になる

#### 多プロセス運用: 観測だけで 1 本に収束させる

gateway は同じ設定の複数プロセスが LB の後ろで対称に動く。優劣を付けたくない
ので、**共有する状態を持たずに合図を 1 本へ収束させる**。使うのは既に持って
いる nonce 表だけで、payload は増やさない:

| 見えたもの | すること |
|---|---|
| 自分が出した合言葉が戻った (`applied` / `late`) | 次を +55 分で仕込む (= そのまま出し続ける) |
| **出した覚えのない合言葉** (`foreign`) | 次を **+57 分**で仕込む (控えに回る) |
| 合図を出した後、何も戻ってこない | **次を仕込まない** (その会話は別のプロセスへ流れた印) |
| 実リクエスト | 次を +55 分で仕込む (見張る期間と通った先も更新) |

57 分は cache が消える手前 (60 分 − 30 秒) より前。相手が生きていれば、こちらが
出す前に相手の次の合図 (55 分) が届いて、また後ろへ下がる。相手が居なくなった
ときだけ、控えていた側が引き継ぐ。合図が 2 本出るのはフェイルオーバー直後の
1 周期だけで、放っておいても 1 本に戻る。

**LB の前提**: `first` (優先度) か sticky を推奨。round-robin でも正しく収束
するが、どのプロセスも「自分の合図が戻らない」状態を経るので、収束するまでの
重複が増える。

### 3. 損益の根拠

`1h` は差分だけが 2 倍単価になり、60 分以内の再開が全量 write から read に変わる。
ping 1 回はプレフィックス全量の read で、再構築 1 回は同量の write。比は read 単価で
決まる: Fable 5.1 / Mythos 5.1 で 50 倍 (55 分間隔なら ~46 時間分の ping が 1 回の
再構築に相当)、read 0.1 倍のモデルで 12.5 倍 (~11.5 時間)。`keepalive_horizon` は
この分岐点と「再開される見込み」から人が決める。

`keepalive_horizon` を比率で書いた場合は、この分岐時間に比率を掛けた長さになる:

```
分岐時間 = (1 時間 write の単価 ÷ cache read の単価) x 55 分
        = (input x 2.0 ÷ cache read) x 55 分
```

Fable 5.1 は cache read が input の 0.025 倍なので 80 回ぶん = 73.3 時間、
Opus 5 は 0.1 倍なので 20 回ぶん = 18.3 時間。同じ `0.3` がそれぞれ 22 時間と
5.5 時間になる。過去 7 日の実測では **0.2〜0.35** が目安
(`scripts/keepalive-horizon-sim.py`)。

週次試算 (main、Fable 5.1、`scripts/cache-cost-sim.py` の実績から):

| 内訳 | 額 |
|---|---|
| `1h` の割増 (差分 31M トークン × $7.5/MTok) | ≈ $230 |
| 60 分を超えた gap を埋める ping (221 gap × 平均 3 回 × $0.1) | ≈ $70 |

## 不採用案

- **実リクエストは 5m のまま、マーカー request だけ 1h を付ける**: 1h フラグは
  1h エントリしか読まないので、マーカーが毎回「前回の 1h 境界から先」を 2 倍で
  書き直す (実測: read 46K / 1h write 664K)。常時 1h より高い
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
