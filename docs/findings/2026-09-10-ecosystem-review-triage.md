# エコシステム外部レビュー (2026-09) 指摘の採否

外部レビュー (`claude-rules-personal` の `docs/research/2026-09-10-ecosystem-review/`、
個別 `llm-gateway.md` + 共通 `common.md`) の指摘を、本リポの実物 (コード / DR / issue) と
照合した結果。issue `docs/issue/2026-09-10-ecosystem-review-2026-09.md` の受け入れ条件 1・2 に対応する。

## 判明した事実

### 採否表

| # | 指摘 (要約) | 判定 | 根拠 |
|---|---|---|---|
| L-1 | README-ja / README の翻訳ペアが無く日本語 1 本 | **採用** (★1) | `README.md` 61 行 1 本。die / hyoui / cache-warden / stable-which / bump-semver / kuu は 6 リポとも `README-ja.md` + `README.md` のペア。規約は rules-personal `reference/docs-authoring/docs-layout.md:50-51` |
| L-3 | DR-0027 を Accept して着手するか、見送りを明記する | **裁定待ち** | DR-0027 は Status: Proposed (9/8) のまま。ただし Accept 可能とする根拠はレビューの誤読 (下記「L-3 の照合」) |
| L-3b | `keepalive-via-messaging-socket` (open) が DR-0027 と両立しない | **採用** (L-3 の裁定に従属) | `docs/issue/2026-09-08-keepalive-via-messaging-socket.md` は status: open、DR-0024 §2 の届け先差し替え = 合図方式の延命。DR-0027 決定 1 の「合図方式を廃す」と正面衝突 |
| L-4 | DR-0028 の「未確定」3 項目を実装後に閉じる | **採用** (★2) | DR-0028 は Status: Accepted / 実装 v0.44.0、決定 9 まで伸びているのに `## 未確定 (実装前に確かめる)` の 3 項目がそのまま残る (227-234 行) |
| L-5 | `gateway.rs` 7,070 行 / server `lib.rs` 4,089 行が DR-0014 の三境界で割れていない | **採用** (★1、方針記録のみ) | 実測 `crates/llm-gateway/src/gateway.rs` 7,070 行 (`mod tests` が 1872 行目 = 実装 1,871 / テスト 5,199)、`crates/llm-gateway-server/src/lib.rs` 4,089 行の単一ファイル |
| C-1 | (common P-23) 外部コマンドに渡すユーザ由来の値が `-` 始まりでフラグ解釈される | **採用** (★1) | `crates/llm-gateway-cli/src/service.rs:504-508` の `tail -n +1 -F <path>` に `--` が無い。`Command::new` は他に version.rs:129 / service.rs:51 / login.rs:212 / supervisor.rs:534 |
| C-2 | (common P-17) `service register` の焼き込みパスを stable-which で選び冪等にする | **既に対応済み** | DR-0028 決定 7 と `feat(cli): 焼き込むパスを stable-which で選び、register を冪等にする` (v0.44.0 系列) |
| C-3 | (common P-9 / P-10 / P-11 / P-12 / P-44) | **本リポの作業なし** | いずれも llm-gateway を出所として rules-personal の reference に反映する候補。反映先が向こう側 |

件数: 採用 4 (L-1 / L-4 / L-5 / C-1) + 従属 1 (L-3b) / 既に対応済み 1 / 却下 0 / 裁定待ち 1 (L-3)。

### レビューが実物と食い違っている点

- L-3 の「DR-0027 の未確定は 9/9 の findings で回答済み」は**成立しない**。DR-0027 の
  `## 未確定 (実装前に確かめる)` に並ぶのは (a) Bedrock / OpenAI 経路で同じ延命が成立するか、
  (b) 系列ファイルのサイズ上限と掃除、(c) `sub = "keepalive"` の既定値 — の 3 つで、
  findings `2026-09-08-cache-ttl-refresh-on-hit.md` はどれにも答えていない。findings が
  答えたのは「再送本文をどこまで削れるか」(§ 見出しにそう書いてある) で、これは決定 1 の
  本文の形の話であり未確定節の項目ではない。日付も 9/9 ではなく 9/8
- L-4 の「新 issue 2 件はこの未確定の延長」は半分だけ正しい。`daemon-restart-order-and-grace`
  は監督者の停止順と猶予なので未確定②(監督者が落ちた時の子) の隣にあるが、
  `service-status-running-false-while-loaded` は `launchctl print` 出力の解析ずれ (launchd
  実装のバグ) で、未確定③(systemd の検証環境) とは別系統

### レビューが触れていない所見

- DR-0027 を採択すると `docs/issue/2026-09-08-keepalive-daily-counters.md` の受け入れ条件が
  空振りする。条件が `applied` / `late` / `foreign` の発火回数を数える形になっているが、
  この語彙は合図の戻り方の分類なので replay では消える。採択時は discard ではなく
  「ping 本数と read トークンを日別に持つ」への書き換えが要る

## 実用的な示唆 / ベストプラクティス

着手順の推し:

1. **L-3 の裁定を先に取る** (統括推し: DR-0027 を Accepted にする)。他の keepalive 系 issue
   3 件 (`keepalive-via-messaging-socket` / `keepalive-daily-counters`、および今後の合図の bug)
   の扱いが全部この 1 つで決まる。採択なら順序は DR-0027 修正案どおり §3 → §1 → §6 → §7、
   `keepalive-via-messaging-socket` は discard、`keepalive-daily-counters` は上記のとおり書き換え。
   見送るなら DR-0027 に見送り理由と再開条件を書き、合図方式の bug fix を続ける根拠にする
2. **L-4** (DR-0028 の未確定を決定 10〜12 として閉じる)。`cli-daemon-subcommands` reference の
   元ネタなので、ここで閉じた結論は他リポにも効く。「監督者が落ちた時の子」は
   `crates/llm-gateway/src/daemon/supervisor.rs` を読んで実装の事実 (道連れか孤児か) を書く。
   systemd は「未検証、macOS のみ実証」と適用範囲外に書く
3. **C-1** (`tail` の引数に `--` を足す)。1 行。他 4 箇所の `Command::new` も同時に見る
4. **L-1** (README-ja 化)。急がない。DR-0008 は「docs / DR / チャットは日本語」なので、
   日本語を原本 (`README-ja.md`) にして `README.md` を英訳にする形は DR-0008 と矛盾しない
5. **L-5** は着手しない。次に `gateway.rs` を大きく触る時に DR-0014 §1 の語彙 (ingress =
   server crate の parse / authorize、egress = `backend/`、exchange = 観測フック) で割る。
   分割だけの PR は作らない (テスト 5,199 行の移動で diff が読めなくなる)

## 検証の詳細

### L-1 の照合

- `ls README*`: `README.md` のみ (61 行、日本語)。節は「何をするか / 何をしないか / ステータス /
  ドキュメント / ライセンス」
- 姉妹リポ 6 つ (die / hyoui / cache-warden / stable-which / bump-semver / kuu) は全て
  `README-ja.md` + `README.md` を持つ。llm-gateway だけが例外
- 規約: rules-personal `reference/docs-authoring/docs-layout.md` のテンプレ表に
  「README ja (リポ直下) / README en (リポ直下)」の 2 行がある

### L-3 の照合

- `docs/decisions/DR-0027-keepalive-by-replay.md` 3 行目: `- Status: Proposed` / `- Date: 2026-09-08`
- 未確定節 (159 行目〜) の 3 項目は上記のとおり findings で未回答
- findings `2026-09-08-cache-ttl-refresh-on-hit.md` の 53 行目に
  `### 再送本文をどこまで削れるか (DR-0027 の未確定への回答)` — 中身は「`max_tokens` は 1 に
  してよい (cache key は tools / system / messages なので全量ヒット、応答 1 トークン、
  非 stream 約 1.0 秒)」「`thinking` を外す必要はない (adaptive のまま `max_tokens: 1` が通る)」。
  つまり**本文そのまま + `max_tokens=1`** はここで確定している。レビューの結論部分は正しく、
  「未確定節が回答済み」という根拠の置き場所だけが誤り
- 「DR-0027 が通れば丸ごと消える機能を直している」も事実。jj log の順 (新しい順):
  `docs: DR-0027 keepalive を自送信 (replay) に置き換える提案` の**後**に
  `fix(keepalive): 受け取り済みの合言葉を自分のものと覚えて、控えの誤設置を止める` →
  `docs(keepalive): 合図の戻りの語彙に spent を足す` →
  `test(keepalive): spent 判定テストの doc コメントを実態に合わせる` →
  `issue(close): keepalive-foreign-standby-fires-immediately -> archive` が積まれている
- `docs/issue/2026-09-08-keepalive-via-messaging-socket.md` frontmatter は `status: open` /
  `category: design`。本文は「keepalive の合図を ccmsg 経由でなく Claude Code の
  messaging socket へ直接注入する (DR-0024 §2 の届け先差し替え)」= 合図方式の延命策

### L-4 の照合

- DR-0028: `- Status: Accepted` / `- Date: 2026-09-09` / `- 実装: v0.44.0 (段階 A/B/C)`。
  決定は 1〜9 (決定 7「OS への登録は監督者 1 つだけ」、決定 9「版は置いてある版と走っている版を
  並べて出す」は実装後に追加されたもの)
- `## 未確定 (実装前に確かめる)` (227 行目) の 3 項目:
  (1) 子のログの置き場と回転 (今は plist が `~/.local/state/llm-gateway/logs/{stable,unstable}.log`
  へ流している。監督者が受けて unit 名で分けるのか、子が自分で書くのか、回転は誰が持つか)、
  (2) 監督者が落ちた時に子をどうするか (道連れか、生かして次の監督者が拾うか)、
  (3) systemd 側の検証環境 (手元は macOS のみ)
- 派生 issue: `2026-09-09-daemon-restart-order-and-grace.md` (restart --all の順序が登録の逆順
  固定、SIGTERM 後の猶予 10s で畳めず `signal: 9`)、
  `2026-09-09-service-status-running-false-while-loaded.md` (`service.running` が
  pid も取れているのに false、`launchctl print gui/<uid>/<label>` の解析ずれと推定)

### L-5 の照合

| ファイル | 行数 |
|---|---|
| `crates/llm-gateway/src/gateway.rs` | 7,070 (`mod tests` = 1872 行目) |
| `crates/llm-gateway-server/src/lib.rs` | 4,089 |
| `crates/llm-gateway/src/lib.rs` | 136 |

crate は `llm-gateway` (core) / `llm-gateway-server` / `llm-gateway-cli` の 3 つ。DR-0014 §1 の
三境界 (ingress / egress / exchange) は語彙としては定義済みだが、ファイル分割はその線で
割れていない。指摘の数値・主張ともに正確

### C-1 の照合

`Command::new` の呼び出しは 5 箇所:

| 場所 | 呼ぶもの | ユーザ由来の値 |
|---|---|---|
| `crates/llm-gateway-cli/src/service.rs:504` | `tail -n +1 -F <path>` | log path (unit 名から組み立て)。**`--` が無い** |
| `crates/llm-gateway-cli/src/service.rs:51` | `Runner::run` の `step.program` + `step.args` | 汎用実行点 (`launchctl` 等) |
| `crates/llm-gateway-cli/src/version.rs:129` | 焼き込みバイナリのパスそのもの | path |
| `crates/llm-gateway-cli/src/login.rs:212` | `open <url>` | url (gateway が生成) |
| `crates/llm-gateway/src/daemon/supervisor.rs:534` | `unit.binary_path` | 登録簿の値 |

実害は薄い (いずれも自分の状態ディレクトリ / 設定由来) が、P-23 が挙げる型そのもの。
`tail` は `.arg("--")` を path の前に入れれば済む
