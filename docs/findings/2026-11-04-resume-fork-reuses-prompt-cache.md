# --resume (fork あり / なし) は親セッションの prompt cache を読める (親が生きていても)

- Date: 2026-11-04

## 判明した事実

走っているセッション A を動かさずに、別プロセスから `claude -p --resume <A> ...` で別タスクを走らせても、A の prompt cache が効く。同じ CONFIG_DIR (settings / plugins / system prompt)、cwd、model なら、fork ありも fork なしも A の全文脈を cache から読み、新しく書くのは追加指示ぶんだけになる。

- A 本体を進めるのと同じ cache 効率で動く
- 親が生きていても fork できる。新しい session id が振られ、`--no-session-persistence` なら jsonl は増えない
- fork の後の親の次ターンも cache が壊れない

## 実用的な示唆 / ベストプラクティス

A の context を消費せずに A の文脈で作業させたい場合 (mod からのハンドオフ作成など)、別プロセスから次を起動すればよい。cache が効いた状態で動く。

```
claude -p --resume <自分の session id> --fork-session --no-session-persistence '<ハンドオフ指示>'
```

Agent tool の `subagent_type: "fork"` も同じ構図になる。プロセス内なので system / tools は完全一致するはずだが、未実測。

## 検証の詳細

### 条件

- Claude Code 2.1.318、`claude-haiku-4-5-20251001`
- unstable unit 経由の ns-personal (keepalive 1h 戦略、DR-0024)
- 一時 CONFIG_DIR / cwd / model を全条件で共通にした

### 結果

| 条件 | prefix | gateway の cache 欄 | cache_read / cache_creation / input |
|---|---|---|---|
| A を作る (基準の 1 本) | f3359f43 | written | 0 / 21,747 / 10 |
| 直後に `-p --resume A --fork-session --no-session-persistence` | 同一 | partial | 21,747 / 958 / 10 |
| `-p --resume A --no-session-persistence` (fork なし) | 同一 | hit | 22,705 / 0 / 10 |
| A 自身を `-p --resume A` で続行 (基準) | 同一 | partial | 21,747 / 977 / 10 |
| A 続行から 308 秒後に fork | 同一 | partial | 22,724 / 204 / 10 |
| A を stream-json で常駐させ (親 PID 生存)、別プロセスから fork | b5d00e0f (親と同一) | - | 親 0 / 21,679、fork 21,679 / 532 |
| 同上、fork の後に親の次ターン | 同一 | - | 21,679 / 612 |

考察: どの条件でも A の cache_read がそのまま読まれ、新規書き込みは追加指示ぶんに収まった。

### 注記

- gateway の `prefix` は system prompt 先頭ブロックだけの hash で、system / tools 全体が同一である証明ではない。実証は cache_read の数 (A の 21,747 がそのまま読まれた)
- A の jsonl の `api-request` 記録では `shapeHash` が初回と続行で一致し、`tail` は 1 から 3 になった
- 5 分超でも hit したのは ns-personal が keepalive 1h 戦略のため。cache 切れ (cold miss) の条件は作れていない
- fork なしが `hit` で creation 0、fork が `partial` で creation 約 1,000 なのは、fork が新 session id 由来の差分を末尾に足すためと推測している (未確認)
- 対話 TUI は初回セットアップのテーマ選択で止まるため検証できず、stream-json 常駐で代替した

関連: DR-0024 (keepalive)、DR-0042 (セッション状態の口、Proposed)、`docs/research/2026-10-21-claude-code-api-request-records.md`
