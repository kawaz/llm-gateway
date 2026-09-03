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

照合は alias 解決後のモデル名。main / sub の判定は anthropic preset が
`metadata.user_id.parent_session_id` の有無から `origin` (main / sub / unknown) を決め、
core はその値だけを見る (DR-0014 の境界。unknown は main 扱い)。

### 2. keepalive の仕組み

1. main の実リクエストを転送するたびに、会話系列 (DR-0012 の `prefix` + session_id)
   ごとのタイマーを **送出時刻 + 4 分** で再武装する (debounce)。ツールループ中は
   次のリクエストが数秒で来るので発火しない
2. 発火したら nonce を発行し、webhook (DR-0012 の口) に
   `cache_keepalive {session_id, prefix, nonce, deadline}` を流す。deadline =
   最後の実リクエスト送出時刻 + 5 分 − 30 秒。受け手 (ccmsg) がそのセッションへ
   マーカー文を注入する (`notify --as-session`)。文面は固定 prefix + nonce +
   「無視して 1 語で返せ」
3. マーカーを末尾 user ブロックに持つ request が来たら、nonce を単回消費し、
   **deadline 内なら**全ブレークポイントに `ttl: "1h"` を付けて転送。deadline を
   過ぎていれば付けない (全量 2 倍書きを避ける。1.25 倍の再構築で済ませる)
4. マーカー request では 4 分でなく **+55 分** で再武装し、最後の実リクエストから
   `keepalive_horizon` を超えたら止める。実リクエストが来たら pending を捨てて
   4 分に戻す
5. 状態はプロセス内メモリ。再起動で消えても次の実リクエストで再武装される

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
