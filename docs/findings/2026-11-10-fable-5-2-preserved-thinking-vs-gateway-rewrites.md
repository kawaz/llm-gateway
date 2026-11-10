# Fable 5.2 の preserved thinking と gateway の書き換えの突き合わせ

- Date: 2026-11-10
- 対象: llm-gateway (v0.70.1 の working copy)、stable unit の設定 `~/.config/llm-gateway/config-11302-stable.toml`
- 一次資料: [Preserved thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking) (2026-11-10 取得)、Claude Code 2.1.318 同梱 skill の `claude-api/shared/model-migration.md` (Fable 5.1 の Breaking change 2・3) / `preserved-thinking-migration.md` / `preserved-thinking-migration/causes.md`、lab-claude の `docs/findings/2026-11-10-fable-5-2.md`
- 調査は読み取りのみ (コード・設定・ログ・`/llm-gateway/sessions`・`thinking-sources/sources.json`)。upstream への新しいリクエストと dump は使っていない

## 判明した事実

### 一次資料 (docs) が言っていること

- **Fable 5.2 は prefix check をアカウントの作成日に関係なく強制する** (docs の表で 5.2 は両列とも「400 error」。5.1・Opus 5.5・Sonnet 5.5・Haiku 5.5 は 2026-08-31 より前のアカウントでは「No error」)。lab-claude の「全アカウントで強制」という伝聞は docs と一致した
- **Fable 5.2 の thinking は account に束縛される** (docs「Thinking blocks stay with the account that produced them」: Sonnet 5.5 / Haiku 5.5 / Fable 5.2 / Mythos 5.2。別アカウントから送ると 400 にならず黙って捨てられる。beta header 付きなら `organization_binding_mismatch`)
- **5.1 → 5.2 のモデル移行は推論を保つ** (docs のモデル跨ぎ表: 5.2 は 5.1・Opus 5.5・Sonnet 5.5・Haiku 5.5 などの block を読める。5.2 の block を読めるのは 5.2 / Mythos 5.2 だけ)。モデル跨ぎで API が落とした block は prefix の編集に数えない
- 編集に**当たらない**もの: `cache_control` の付け外し・移動、`system` / `tools` / `messages` 以外の欄の変更 (`model`、`max_tokens`、`thinking` 設定そのもの、`output_config`)、リクエストヘッダ (ただし server 側で tool を注入する beta はヘッダだけで tool の集合を変える)、JSON の鍵順、文字列 content と text block 1 個の違い、thinking block を先頭から・末尾から・全部外すこと
- 編集に**当たる**もの: `system` / `tools` の作り直し、前の turn の書き換え・並べ替え・削除、thinking block を途中から外すこと、**外した block を後で戻すこと** (causes.md の Keep list: 「putting a removed block back, which invalidates the blocks produced while it was gone」)、`role: "system"` message を top-level `system` へ移すこと
- docs の「Libraries, proxies, and gateways」: `anthropic-beta` と `thinking.block_binding` を素通しし `input_transformations` を返す / `role: "system"` message を動かさない / tool を止めるときは `tools` を消さず `tool_choice: none` / 400 を隠さない (捕まえて thinking を剥がして送り直すなら、そうしたと記録する)

### gateway の書き換え一覧と 5.2 での影響

| # | 書き換え | コード | 何を変えるか | 5.2 での影響 | 根拠 | dump で見える欄 |
|---|---|---|---|---|---|---|
| 1 | `thinking_display` (DR-0016) | `crates/llm-gateway-server/src/lib.rs:754` (`apply_thinking_display`)、呼び出し `:942` | `thinking` object がある Messages 本文に `display` を足す / 上書きする。他の鍵 (`block_binding` を含む) は残す | **影響なし** (`thinking` 設定は prefix の外) | docs FAQ「`thinking` configuration aren't part of the checked prefix」、causes.md Keep list | `request_rewrites.thinking_display` (変えた時だけ) |
| 2 | cache 戦略 (DR-0024) | `crates/llm-gateway/src/gateway.rs:1524`、`crates/llm-gateway/src/cache.rs:44` | 既存の `cache_control` に `ttl` を足す / 外す / `cache_control` を外す。ブレークポイントの追加・移動はしない | **影響なし** | docs・skill とも `cache_control` の付け外しは編集でないと明記 | `cache_strategy` / `cache_ttl_secs` |
| 3a | `thinking_as_text` の変換そのもの (DR-0035 §5) | `gateway.rs:1516`、`crates/llm-gateway/src/thinking.rs:49` | 跨いだ連なりの全リクエストで、履歴の thinking を text に置き換え、`redacted_thinking` と空の thinking を落とす | **その 1 本は 400 にならない** (thinking block が 1 つも残らないので検める対象が無い)。代わりに推論は毎 turn 失われ、運べるのは summarized の本文だけ | docs「Removing ... all of them」は許容 | `thinking_as_text: true` |
| 3b | **変換が途中で止まること** | `crates/llm-gateway/src/thinking_sources.rs:31` (`SOURCE_TTL_MS` = 24h)、`:144` (`crossed`)、session key は `crates/llm-gateway/src/session.rs:98` | 変換中に 5.2 が作った block は「text に置き換わった履歴」の上で署名される。変換が止まると、クライアントが持つ元の履歴 (thinking block 入り) がそのまま送られる | **400 の見込み (未実測)**。prefix の text block が消え、外していた thinking block が戻るので、変換中に作られた 5.2 の block が全部 mismatch になる。5.2 は全アカウント強制なので 400 | causes.md Keep list の「putting a removed block back」、docs「What counts as an edit」 | 400 の応答本文 (`*.response.body`)。止まった 1 本には `thinking_as_text` が付かない |
| 3c | モデル跨ぎの判定 (DR-0035 §1) | `thinking_sources.rs:144` | `source.model != model` なら常に跨ぎ扱い | **推論を不要に捨てる** (5.1 → 5.2 は API 側で推論が保たれるのに、gateway が text 化する)。400 ではない | docs のモデル跨ぎ表 | `thinking_as_text: true` |
| 4 | account 束縛の対象 (DR-0035 §1) | `crates/llm-gateway/src/config.rs:442` (既定 `["claude-sonnet-5-5"]`)。stable 設定は `account_bound_thinking` を書いていない | `claude-fable-5-2` は account 束縛として扱われない。開始 account を先頭へ寄せる並べ替え (`crates/llm-gateway/src/router.rs:957`) も、跨ぎの判定も効かない | **thinking が黙って落ちる**。fail over (401/403/429/529、`gateway.rs:2601`)、affinity 切れ (1 時間、`router.rs:39`) 後の reset 順・spend_down の繰り上げで別 account へ移ると、本文は無改変のまま送られ、API が前の account の 5.2 block を捨てる。events の `thinking_dropped_by_switch` も立たない | docs の account 束縛節 | 見えない (gateway は beta header を足さないので `input_transformations` も出ない) |
| 5 | beta フラグの学習 (DR-0003) | `gateway.rs:1141`〜`:1192`、`crates/llm-gateway/src/preset/anthropic/negotiation.rs:30`、`crates/llm-gateway/src/preset/anthropic/beta.rs:116` / `:130` | credential ごとに覚えた拒否フラグを `anthropic-beta` から落とす。400 に「invalid beta flag」/「unsupported beta」があり名指しが無ければ**送った全部**を 24 時間覚える | 公式 Anthropic 経路は既定 Passthrough なので**今は影響なし**。ただし (a) `thinking-binding-controls-2026-08-01` を落とすと、本文に `block_binding` があれば 400 (`Extra inputs are not permitted`)、(b) server 側で tool を注入する beta を落とすと tool 集合が変わり prefix 編集になる。prefix mismatch の 400 文面は判定語を含まないので、それで学習が走ることはない | skill model-migration.md の Breaking change 3、causes.md の「Request headers are not part of it either, except ...」 | **記録されない** (dump は受けたヘッダだけを残し、送ったヘッダも `request_rewrites` も持たない) |
| 6 | keepalive の自送信 (DR-0027) | `gateway.rs:1356`、`crates/llm-gateway/src/cache/keepalive.rs:721` | 控えた本文 (変換・cache 整形後の形) の `max_tokens` だけを 1 にし、控えた経路名の credential へ送る | **影響なし** (`max_tokens` は prefix の外、同じ account)。応答は読み捨て | docs / skill の Keep list | dump に出ない (`send_keepalive` は tee を渡さない)。events の `origin: keepalive` と status |
| 7 | model の別名解決 | `gateway.rs:765`、`crates/llm-gateway/src/egress.rs:566`、`router.rs:1377`・`crates/llm-gateway/src/discovery.rs:279` | `fable` → `claude-fable` → `claude-fable-*` の最新 (`created` 最大) に `model` を書き換える。`claude-fable-5` は 5.1 に固定 | `model` は prefix の外で**それ自体は影響なし**。ただし別名で送るクライアントは discovery の更新で 5.1 → 5.2 に切り替わり、3c の跨ぎ判定で text 化が始まる | docs | `model: {received, sent}` |
| 8 | 未知欄・新しい形の素通し | 受け口は `serde_json::Value` のまま運び (`lib.rs:900`)、`AnthropicWire::encode` (`crates/llm-gateway/src/preset/anthropic/wire.rs:69`) は `serde_json::to_vec` で書き戻すだけ。`serde_json` は `preserve_order` (`Cargo.toml:53`) | `thinking.block_binding`、`context_management`、`role: "system"` message、`clear_at`、`tool_addition` は触らない。`role: "system"` を top-level へ移す処理は無い。応答は Passthrough で `input_transformations` もヘッダ `anthropic-thinking-prefix-mismatch` もそのまま返る | **影響なし** | コード | — |
| 9 | ヘッダの除去 | `egress.rs:89` (`strip_for_upstream`) | hop-by-hop と認証系、`accept-encoding`、`x-forwarded-*` だけ。`anthropic-beta` は残す | **影響なし** | コード | — |
| 10 | 400 の扱い | `gateway.rs:2601` (`is_route_denial`) | 400 は経路を替えず、そのままクライアントへ返す。thinking を剥がして送り直す処理も無い | **影響なし** (docs の「400 を隠さない」を満たす) | コード | 400 の応答本文 |

### 実機で見えたこと (読み取りのみ)

- stable の `/llm-gateway/sessions` には `claude-fable-5-2` の系列が並び、personal ns の fable の経路候補は 2 account の同格グループ (`llm-gateway models` でも同じ並び)。5.2 の会話が account を跨ぎうる構成になっている
- `~/.local/state/llm-gateway/stats/thinking-sources/sources.json` で 5.2 の tuple を持つ連なりは 6 本、うち **4 本が 5.1 → 5.2 の跨ぎ** (5.1 の tuple が先にあり、5.2 の tuple が後から入った)。4 本とも main の連なりで、personal ns の `on_thinking_crossing = "thinking_as_text"` により、5.2 に移った時点から全リクエストの thinking が text で送られている。docs の表では 5.1 → 5.2 も、5.1 の account 跨ぎも推論を保つので、この 4 本は gateway が変換しなければ推論を持ち越せていた
- stable.log に `bound to a different conversation` / `Invalid signature` の行は無い (gateway は 400 の本文をログに書かないので、無いことは 400 が無かった証拠にはならない)
- 別件: stable.log に 1 時間ごとに `the price table does not describe this model gap=claude-fable-5-2 ... prices it as the older generation` が出ている。5.2 は単価が 5.1 と違う ($8 / $40) ので、stats の費用と keepalive の horizon (損益分岐から起こす) が 5.1 の単価で計算されている

## 結論

### 今すぐ直すべき (2 件)

1. **3b: `thinking_as_text` の変換が止まると 400 になる穴** (一番危ない)。変換中の会話で 5.2 が作った block は、変換後の履歴に結ばれている。変換が止まる経路は少なくとも 3 つある
   - 出所の寿命切れ: 最後に thinking 付きの 1 本が 2xx で通ってから 24 時間で出所を忘れる。keepalive の自送信は `seen` を延ばさない (horizon は 5.1 系で 22 時間)。24 時間以上黙った会話を `--resume` すると、次の 1 本は跨ぎ無しと判定され、元の履歴がそのまま送られる
   - fork: `--resume --fork-session` は session id が変わる (`docs/findings/2026-11-04-resume-fork-reuses-prompt-cache.md` の運用)。session key が `claude:<session id>` なので出所が空の新しい連なりになり、元の履歴がそのまま送られる
   - 出所ファイルの消失・破損、`on_thinking_crossing` を `thinking_as_text` 以外へ変える reload、同じ session を別方針の namespace が通すこと

   5.1 では古いアカウントに強制が無かったので顕在化しなかった (未実測。資料からの推論)。5.2 は全アカウント強制なので、上の 4 本の連なりは今この地雷を抱えている
2. **4: `claude-fable-5-2` が account 束縛の対象に入っていない**。2 account の同格グループで回しているので、fail over や affinity 切れの後に黙って推論が落ちる。ただし 4 だけを直すと跨ぎ扱いの連なりが増え、3b の地雷も増える。**3b と同時か、3b の後に入れる**

### dump / 実機で確かめるべき (3 件)

- 3b の 400 が本当に出るか、出たとき Claude Code が thinking を剥がして送り直すか (下の「統括に頼む実機操作」1)
- Claude Code 2.1.321 が `thinking-binding-controls-2026-08-01` を送っているか、本文に `thinking.block_binding` を載せているか (dump の `*.request.json` の受けたヘッダと `*.request.body`)。送っていれば 5 の学習が落とした時の害が具体化する
- credential の `denied_beta` に何が溜まっているか (5)

### 影響なし (8 件)

1 (`thinking_display`)、2 (cache 戦略)、3a (変換した 1 本そのもの)、6 (keepalive)、7 (別名解決そのもの)、8 (未知欄の素通し)、9 (ヘッダの除去)、10 (400 を返す)

3c (モデル跨ぎの判定が広すぎる) は 400 ではないが、上の 4 本で実際に推論を捨てている。直すべきものに数えるかは 3b の直し方次第なので、下の提案に含める

## 直し方の提案 (設計判断は統括へ。DR は起こしていない)

### 3b の候補 (どれを採るかで性質が違うので並べる)

- **(A) 変換が止まらないようにする**: 出所の寿命を keepalive の自送信でも延ばす、寿命を延ばす。寿命切れは塞げるが、fork (別 session id) とファイル消失は塞げない
- **(B) この 400 を受けたら変換して 1 回だけ送り直す**: 400 の本文が `bound to a different conversation` を含み、本文が thinking を運ぶ時に、`thinking::as_text` を当てて同じ経路へ再送し、出所に跨ぎを記録する (以後も変換が続く)。docs の「400 を隠さない」に沿うよう、warn と events に再送した事実を残す。fork・寿命切れ・ファイル消失をまとめて塞げる。代償は毎回 1 往復と、prefix が変わることによる cache の書き直し 1 回
- **(C) 跨いだ連なりは text 化でなく `thinking-binding-controls-2026-08-01` + `prefix_mismatch_behavior: "drop_block"` を gateway が付ける**: 本文の `messages` を変えないので、変換が止まるという状態自体が無くなる。代わりに跨ぐ前の推論は text としても残らない (DR-0035 が `thinking_as_text` で救おうとしたもの)。beta header と `thinking` 設定の追加は prefix の外だが、gateway がクライアントの指定 (`block_binding`) を上書きしない作りが要る

### 3c

- 跨ぎの判定を「生成したモデルの block を、送る先のモデルが読めるか」に寄せる。docs の表 (5.2 は 5.1・Opus 5.5・Sonnet 5.5・Haiku 5.5 等を読める、5.2 の block は 5.2 / Mythos 5.2 しか読めない) を `account_bound_thinking` と同じく upstream の性質として最上位の設定に持つ形が考えられる。読める移行では本文を変えない (API が読めないものだけ落とすので、gateway が先回りして変換する必要が無い)

### 4

- stable / unstable の設定の最上位に `account_bound_thinking = ["claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-2", "claude-mythos-5-2"]` を書く (既定値を置き換える配列なので、Sonnet 5.5 も書き直す)。コードの既定 (`config.rs:442`) も docs に合わせる。`claude-haiku-5-5` も docs では account 束縛だが既定に入っていない

### 5・dump

- dump の `request_rewrites` に `anthropic-beta` の書き換え (落としたフラグと理由: 既定の拒否 / 学習 / 400 後の再送) を足す。今は gateway の書き換えのうちこれだけが dump から辿れない
- 名指しの無い beta 拒否で「送った全部」を覚える規則は、公式 Anthropic 経路で起きると `oauth-2025-04-20` や `thinking-binding-controls-*` まで 24 時間落とす。公式経路では起きていない (未確認) ので優先度は低い

## 統括に頼む実機操作

1. **3b の再現** (upstream に数本、5.2 の小さなリクエスト。費用が掛かる): `thinking_as_text` の namespace で、1 turn 目を `claude-fable-5-1`・2 turn 目以降を `claude-fable-5-2` で回して変換を起こし、その会話を `claude --resume <id> --fork-session` で 1 turn 進める。dump を開始しておき、変換中の試行に `request_rewrites.thinking_as_text: true`、fork 後の試行にそれが無く、応答が 400 (`bound to a different conversation`) になるかを見る。Claude Code が 400 の後に thinking を剥がして送り直すか (2 本目の request の本文に thinking が残っているか) も同じ dump で分かる
2. **dump で受けたヘッダと本文を見る**: 普段の 5.2 の会話を短時間 dump し、`*.request.json` の `anthropic-beta` に `thinking-binding-controls-2026-08-01` があるか、`*.request.body` の `thinking` に `block_binding` があるか、`role: "system"` message が来ているかを見る
3. **credential の `denied_beta`**: `~/.local/state/llm-gateway/credentials/` の claude 系 credential ファイルから `denied_beta` の鍵だけを読む (秘密を含むファイルなので本調査では開いていない)
4. **unstable の設定**: stable との差分確認を試みたが auto mode classifier に止められた (unstable 設定を読むコマンド)。迂回はしていない。4 の設定変更は両 unit に要る
5. **account 跨ぎの黙った drop の観測** (任意): 4 の前後で、5.2 の会話を意図的に別 credential へ寄せた 1 本に `anthropic-beta: thinking-binding-controls-2026-08-01` を付けて送り、応答の `input_transformations` に `organization_binding_mismatch` が出るかを見る (gateway はヘッダと応答を素通しするので、クライアント側で付ければ見える)

## 検証の詳細

### 書き換え箇所の洗い出し方

- `as_object_mut` / `get_mut` / `remove` / `insert` を `crates/llm-gateway/src` と `crates/llm-gateway-server/src` で検索し、テスト以外で本文・ヘッダを変える箇所を全部読んだ。Anthropic 経路で本文を変えるのは 1・2・3a・6・7 の 5 箇所、ヘッダを変えるのは 5・9 と認証 (`crates/llm-gateway/src/preset/anthropic/auth.rs`、`authorization` を差し替えるだけで本文に触らない) の 3 箇所
- dump の `request_rewrites` は `gateway.rs:1556` (試行ごと) と `crates/llm-gateway/src/dump/capture.rs:214` (受け口) で組まれ、鍵は `model` / `thinking_display` / `thinking_as_text` / `cache_strategy` / `cache_ttl_secs` の 5 つ。DR-0038 決定 7 の列挙と一致し、ヘッダの書き換えはどちらにも無い

### 出所 (`sources.json`) の 5.2 の連なり

| 連なり | tuple (account は伏せる、決まった順) | 跨ぎ (M = 5.2) |
|---|---|---|
| 1 | B・5.2 | 無し |
| 2 | A・5.1 → B・5.2 | あり (モデル) |
| 3 | B・5.2 | 無し |
| 4 | A・5.1 → B・5.1 → B・5.2 | あり (モデル) |
| 5 | A・5.1 → B・5.1 → B・5.2 | あり (モデル) |
| 6 | A・5.1 → B・5.2 | あり (モデル) |

考察: 4 本とも personal ns の main の連なりで、5.2 の tuple は 2026-11-09〜10 の数時間の間に入った (Claude Code 2.1.321 への更新で会話の途中から 5.2 に移った)。今の判定 (`crossed`) では、この 4 本は以後 24 時間 (最後の 2xx から) 変換され続け、fork するか 24 時間黙ると 3b に当たる。
