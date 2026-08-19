# DR-0020: 外した理由を出力に載せる (events の `skipped` / usage の `denial`)

- Status: Accepted
- Date: 2026-08-19

## 背景

経路を外した理由 (`crate::denial::Reason` の `Limited` / `Busy` / `Paced`、
`denial.rs:48-58`) は、どの出力スキーマにも載っていない。現状の露出は
tracing ログだけで、`select` の中の `info!` (`router.rs:508-516`) と
gateway の `warn!` に出るきり。

pace_cap (DR-0019) で外れた経路は特に見えない。`Route::paced_out()`
(`router.rs:81`) は **upstream へ当てずに手前で候補から外す**ので、その経路に
関する痕跡がどこにも残らない。events に出るのは実際に転送した 1 経路と、
全滅時の合成 429 だけなので、「他の経路も生きていたのに、なぜこの経路が
使われなかったのか」が外から分からない。自主的に控えた (`Paced`) のか、
upstream に断られた (`Limited` / `Busy`) のかの区別は、pace_cap の設定を
詰めるときにいちばん知りたいことなのに、ログを追う以外の手段がない。

## 決定

### 1. 転送結果の event に `skipped` 欄を足す

`Event` (`events.rs:29-47`) に、経路選定で外した経路の一覧を optional 欄で
足す。要素は「どの経路を」「なぜ」の 2 つ:

```json
{
  "ts": 1755500000,
  "ts_iso": "2026-08-19T12:00:00+09:00",
  "session_id": "…",
  "ns": "personal",
  "model": "claude-fable-5",
  "credential": "codex-kawaz",
  "status": 200,
  "skipped": [
    { "credential": "codex-emrd", "reason": "paced" },
    { "credential": "codex-old", "reason": "limited" }
  ]
}
```

- **1 リクエスト = 1 event の意味論は変えない**。外した経路ぶんの event を
  別に流すのではなく、通った 1 件の中に「そこへ至るまでに何を外したか」を
  同梱する。読む側は今までどおり event を数えればリクエストを数えられる
- `reason` の JSON 表現は `Reason` に対応する小文字文字列 —
  **`"limited"` / `"busy"` / `"paced"`** の 3 語。設定・出力の語と内部の語を
  一致させる (`Reason` に語が増えたらここも増える)
- 外した経路が無ければ **欄ごと出さない** (`skip_serializing_if`)。素通しの
  リクエストで空配列が並ぶと、見る側の目が滑る

全滅時の合成 429 event (`router.rs:540`、`credential` は呼び出し側が
`"-"` を渡す) にも同じ欄が載る。この場合は全経路が `skipped` に並ぶので、
「429 を返したが、その内訳は 2 本が `paced`、1 本が `limited`」まで 1 件で
読める。

### 2. usage に現在の締め出し状態を足す

`CredentialUsage` (`quota.rs:366-385`) に、その credential が**今**締め出されて
いるかを optional 欄で足す:

```json
{
  "name": "codex-emrd",
  "type": "oauth",
  "support": "observed",
  "snapshot": { "…": "…" },
  "denial": { "reason": "limited", "until": 1755510000 }
}
```

- 値は `RouteState::denial()` (`denial.rs:202`) を読むだけ。usage は元々
  「今どうなっているか」を出す口なので、締め出しも今の状態として並ぶ
- 締め出し中でなければ **欄ごと省略**する
- **`Snapshot` の中ではなく `CredentialUsage` の直下に置く**。snapshot は
  upstream が返した枠の写しで、出所は upstream にある。締め出しは gateway
  自身の判定 (しかも `Paced` は upstream が関与していない) なので、出所の
  違う値を同じ入れ物に混ぜない。読む側が「これは誰が言ったことか」を欄の
  位置で見分けられる形にする

`Paced` は経路の状態に印を残さない (DR-0019 §3) ので、この欄に現れるのは
`Limited` / `Busy` だけになる。pace_cap で外れたことを知りたい側は events の
`skipped` を見る — 印を残さないのは namespace をまたいで効かせないための
設計判断なので、usage 側に出すために印を残す方へは倒さない。

### 3. 履歴のカウンタは作らない

「この credential が `paced` で外れた回数」のような集計は持たない。events は
起きたことを流すだけで状態を持たない口 (DR-0012)、usage は今の状態を出す口
(DR-0007) で、どちらも履歴の置き場ではない。数えたい人は events を集めて
数える。

## 互換方針

**既存の欄は 1 つも変えない。追加はすべて optional**。DR-0012 が `prefix` を
後から足したときと同じ形で、未知の欄を無視する読み手はそのまま動く。ccmsg の
webhook 連携も無変更で通る。

DR-0017 は「events は恒常消費者を持つ公開契約で、フィールド追加が下流を
揺らす」を理由に tap を分離したが、これは**揮発的な観測を events に混ぜない**
ための判断であって、events 自身の恒久的な語彙を増やすことを禁じてはいない。
`skipped` は「なぜこの経路になったか」という、events が既に答えている問い
(`credential` 欄) の裏面にあたるので、events の契約の内側に置く。

## 採らなかった案

- **全滅時 (429) の event にだけ理由を載せる**: 痕跡が消えるのは
  **部分的に外れた場合**の方 (全滅なら 429 という形で外に出ている)。いちばん
  見えない場面が抜ける
- **denial を独立した event として流す**: 1 リクエスト = 1 event が崩れ、
  読む側は「これは転送の event か、外した知らせか」を毎件見分けることになる。
  リクエストを数える用途 (prompt cache の残りを数える等、DR-0012 の本来の
  用途) が壊れる
- **理由を tap (DR-0017) だけに出す**: tap は見ている時だけ動く揮発的な口。
  pace_cap の効き方は後から振り返って設定を詰めたいものなので、恒常的に
  流れる events に載っている必要がある
- **`denial` を `Snapshot` の中に入れる**: upstream 由来の枠の写しに、
  gateway 自身の判定が混ざる。`Paced` に至っては upstream が一切関与して
  いない値で、出所の異なるものを同じ入れ物に置くと読む側が区別できない
- **denial の履歴カウンタを usage に持つ**: usage は今の状態を出す口で、
  カウンタは別の性質 (単調増加・再起動でのリセット・窓の定義) を持ち込む。
  events から数えられるものを二重に持たない
- **`reason` を数値やそのままの Rust 表記で出す**: 出力の語彙は英語の小文字
  文字列で揃っている (DR-0008)。読み手が対応表を引かずに読める形にする

## 実装スコープ

- `events.rs`: `Event` に `skipped: Vec<Skipped>` (空なら省略) と、要素型
  `Skipped { credential, reason }` を足す。`Reason` の直列化表現 (小文字
  3 語) もここか `denial.rs` で定める
- `router.rs`: `select` (`router.rs:502-546`) の候補ループで、`paced_out()` と
  `Availability::Denied` で外れた経路を理由つきで集める。`Availability::Denied`
  は現在 `until` しか運んでいない (`denial.rs:105-112`) ので、理由を通す配線が
  要る。集めた一覧を `Event` まで運ぶ経路 (`Event::new` の引数を増やすか
  `Origin` を広げるか) は実装側の判断でよい
- `gateway.rs`: 転送結果の emit (`gateway.rs:589-593`) に一覧を渡す。
  `usage_report` (`gateway.rs:602`) で `CredentialUsage` に `denial` を詰める
- `quota.rs`: `CredentialUsage` に `denial` 欄を足す
- `denial.rs`: `RouteState::denial()` はそのまま使う (変更不要)
