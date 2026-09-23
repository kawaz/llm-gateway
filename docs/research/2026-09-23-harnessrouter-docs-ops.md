# harnessrouter 調査: docs・運用・配布・プロジェクト運営

調査日: 2026-09-23。対象は `HarnessRouter/harnessrouter` の main (clone 済みの手元コピー)。観点は docs の構成、self-hosting、protocol の版管理と運営文書、conformance テスト、support matrix の自動生成、CONTRIBUTING / SECURITY / CI、テスト構成、ライセンス。比較対象は kawaz 側の `kawaz/llm-gateway`、`kawaz/ccmsg`、docs 標準 (`kawaz/claude-rules-personal` の `reference/docs-authoring/docs-layout.md`、`reference/justfile/`)。

## 1. 対象の要約 (事実)

### リポジトリの形

- 直下: `README.md` (384 行)、`CONTRIBUTING.md`、`SECURITY.md`、`CODE_OF_CONDUCT.md`、`LICENSE` (Apache-2.0)、`NOTICE`、`Dockerfile`、`docker-compose.yml`、`.env.example`、`.dockerignore`
- コード: `gateway/` (Python、`pytest.ini` + `tests/`)、`runner/` (Python、`pytest.ini` + `tests/`、各 harness の driver)、`ui/` (Next.js / TypeScript)、`docker/` (`entrypoint.sh`、`install-kits.sh`、`install-skills.sh`)
- `docs/`: `self-hosting-guide.md`、`benchmark.md` + `benchmark-results.json`、`support-matrix.md` + `support-matrix-results.json` + `support-matrix-notes.md`、`harness-verification.md`、`images/`。日付付きファイルや DR のような判断記録ディレクトリは無い
- `protocol/`: 仕様 (UHP) 一式。`README.md`、`CHANGELOG.md`、`GOVERNANCE.md`、`VERSIONING.md`、`IMPLEMENTATIONS.md`、`CONNECTING.md` (client 実装ガイド)、`SERVING.md` (server 実装ガイド)、`naming.md`、`schema/`、`versions/2026-08-11/` と `versions/2026-09-12/`、`conformance/`、`site/` + `vercel.json` (仕様サイト)
- `scripts/support-matrix/`: support matrix の計測・描画スクリプト

### README の作り (訴求)

- 冒頭はダーク/ライト切替の `<picture>` ロゴ、tagline、バッジ列 (GitHub stars の自前 SVG、License、Docker pulls、`UHP-Full` の conformance バッジ、`OpenAI Responses-Compatible`)
- 1 段落の価値提案 (`Build agent products without handling harness engineering.`) の直後に「Star this repo」CTA 画像 (desktop / mobile で `<picture>` の media 切替)
- 続いてアニメーション GIF (`docs/images/2026-09-13-harnessrouter-integration-comparison-cropped-v5.gif`) で「harness ごとに個別統合すると責務が 36→45 に増える / HarnessRouter なら 1 統合」を図示。alt に数値まで書いてある
- 見出し構成: `Switch harnesses. Optimize cost and latency.` → `Quickstart` (5 ステップ: 起動 / 初回起動待ち / console / provider 接続 / 最初のタスク) → API 統合 → pluggable backend → `Deployment choices` (self-host / Cloud への道 / Community Edition の中身) → `The Unified Harness Protocol` → `Resources` → `Star History` → `License`
- 見出しの前に `<a id="...">` の固定アンカーを複数置いており、見出し文言を変えてもリンクが切れない作り
- stars バッジは `.github/workflows/readme-stars.yml` が 5 分おき (cron `2,7,12,...`) に更新し、`readme-badges` ブランチの SVG を README が参照する

### self-hosting

- `docker-compose.yml` は 1 サービス・1 volume (`harnessrouter-data:/data`、SQLite / blob / secret store が全部ここ)。ファイル冒頭コメントに 3 行の手順 (`cp .env.example .env` → `docker compose up -d` → `http://localhost:3000`)
- `.env.example` は provider 接続を `HR_SECRET_GLOBAL_HARNESS_CONN_<NAME>={json}` の 1 変数 1 JSON で渡す形式。「env から読み、image にも git にも書かない。secret store は disk より env を先に読むので、env の key は使われるが永続化されない」とコメントで明記
- `Dockerfile` は `node:22-slim` (ui ビルド) → `python:3.12-slim` の multi-stage、`EXPOSE 3000`、`HEALTHCHECK`、`tini` 経由の `entrypoint.sh`
- `docs/self-hosting-guide.md` は目次付きの長文 (約 960 行): Install 6 ステップ、再起動 / アップグレード / バックアップ、Starter kits、Configuration、API 利用、公開 URL に出す時 (TLS / password)、hosted への移行、Architecture

### protocol の版管理と運営文書

- `protocol/VERSIONING.md`: 版は日付 `YYYY-MM-DD`。SemVer を採らない理由を明記 (「major/minor の約束は採番者の規律次第で実際には破られる。日付は守れない約束をしない」)。版内で許される変更 (optional field 追加、応答 field 追加、event 追加、vendor prefix 付き error code 追加、制約緩和) と禁止される変更 (削除・rename・型変更・必須 field 追加・制約強化) を MUST/MAY で列挙。旧版も公開・配信継続。末尾に「現行版の既知の妥協」節
- `protocol/GOVERNANCE.md`: 原則 4 つ (Prose before code / Three artifacts move together = 仕様・参照実装・conformance を同じ変更で更新 / Compatibility is a feature / The bar is a working implementation)。変更は UEP (GitHub issue、label `uep`、Problem / Proposal / Compatibility / Alternatives) → 10 営業日以内に 3 択回答 → 1 PR で仕様・schema・参照実装・conformance テスト (変更前 fail / 変更後 pass)・CHANGELOG を揃える checklist。「A declined field is not a pending one」節で却下の記録を再議論防止に使う。名称 (UHP) の商標と「conformance suite を通ったものだけが UHP-compatible を名乗れる」規定
- `protocol/CHANGELOG.md`: 版 (日付) ごとの節。冒頭で前版との互換性 (「Additive to 2026-08-11」) を先に述べ、`Added` 等の小節
- `protocol/IMPLEMENTATIONS.md`: 実装一覧 (HTML card)。掲載基準 (公開リリース / 対象 UHP 版 / 連絡先 / 役割) を明記し「掲載は認証ではない」と宣言。conformance 水準はレポートがある時だけ site 側で自動表示

### conformance テスト

- `protocol/conformance/` は独立 Python パッケージ (`pyproject.toml`、`uv.lock`、CLI `uhp-conformance`)。`pip install "git+...#subdirectory=protocol/conformance"` で外部から導入でき、schema を同梱する
- 任意の server に HTTP で当てる。class は `core` / `extended` / `full` の累積。75 checks。実際に約 6 本の agent タスクを走らせる (トークン費用と数分がかかることを「意図的」と明記)
- 結果は PASS / FAIL / SKIP / ERROR (ERROR = suite 自身のバグ)。「A skip is never a pass」: JSON の `conformant` は skip 1 件で false、別に `conformant_with_skips` と `skipped_not_verified` (id 列挙)、`suite_version` と `generated_at` を持つ
- 計測は `.github/workflows/conformance-measure.yml` (workflow_dispatch) が実行し、レポートを `protocol/conformance/reports/<slug>/` に landing。site がそこからバッジを生成し、再計測で落ちればバッジも消える。`conformance-remeasure.yml` もある
- suite 自体の単体テストは `protocol/conformance/tests/` (stub server を使う)

### support matrix / benchmark の自動生成

- `scripts/support-matrix/run.mjs` が Playwright で console を 1 ユーザとして操作し、harness × model ごとに 5 シナリオ (first / follow-up / model switch / artifact / recycle) を計測、1 組 1 JSON レコード。`fill-connection.py` で実際に使われた接続を刻み、`render.py` が `docs/support-matrix.md` を生成。再開可能 (記録済みペアは skip、`error` 付きは再実行)
- 規則として「FAIL 行は再現した provider のエラー文を持つ」「verified list は他インスタンスから継承しない」「素の `incomplete` は除外前に再試験」
- 何を証明すべきかは `docs/harness-verification.md` が正本で、CONTRIBUTING からも参照
- `docs/benchmark.md` も同様に生成物 (表の Notes 列に採点除外理由、表の下に「findings = 採点しない異常 run」の列挙)

### CONTRIBUTING / SECURITY / CI

- `CONTRIBUTING.md`: harness 検証の要求、連絡先の振り分け (Issues / Discord / 脆弱性は SECURITY)、「大きい変更は issue で説明してから作る」、protocol 変更は GOVERNANCE へ、PR の checklist、dev checks、ライセンス同意、「main は PR のみ・1 approval・全 check green・admin も bypass 無し」。自分の PR を自分で approve できないため、maintainer アカウントで動く agent は `open-pr.yml` で `github-actions[bot]` として PR を作る運用を明文化
- `SECURITY.md`: 脅威の前提 (インスタンスは渡した credential で任意コード実行し得る) を冒頭に書き、報告経路 2 つ (GitHub private vulnerability reporting / メール)、10 営業日の一次応答目標、scope / out of scope (同梱の agent CLI 自体は対象外)
- workflows: `tests.yml` (push / PR)、`release.yml` (Docker multi-arch build & push)、`retag-latest.yml`、`hub-cleanup.yml`、`open-pr.yml`、`readme-stars.yml`、`conformance-measure.yml`、`conformance-remeasure.yml`

### ライセンス

- Apache-2.0。`NOTICE` に著作権表示と「配布物に NOTICE を同梱する義務」の説明、第三者依存のライセンス列挙。GOVERNANCE で「Apache-2.0 はコードと仕様の権利を与えるが名称の権利は与えない」と商標を分離

## 2. kawaz のリポ群が参考にできる点

### 2-1. 生成される docs (結果 JSON → 描画スクリプト → md)

- harnessrouter の事実: `docs/support-matrix-results.json` / `docs/benchmark-results.json` を正本にし、`scripts/support-matrix/render.py` が md を生成。計測の規則は `docs/harness-verification.md` に分離し、「何を証明するか」と「どう走らせるか」(`scripts/support-matrix/README.md`) を別文書にしている
- kawaz 側の現状: llm-gateway の実測は `docs/findings/`・`docs/runbooks/2026-08-07-openai-native-verification.md` 等に散文と表で手書き。docs 標準も「検証マトリクスは findings に表形式で残す」まで
- 取り込んだ場合: 同じ観点を繰り返し計測する対象 (llm-gateway なら upstream ごとの転送可否・cache 挙動・beta flag の受理状況) を「結果 JSON + render」にすると、再計測時の diff が機械的に取れ、手書き表の drift が無くなる。empirical-verification の「マトリクス検証」を再実行可能な形で固定できる
- 取り込まない方がよい理由: 計測が 1 回きりの調査なら生成系はコスト過多。繰り返し計測する対象が 1 つ以上あると確認できてから導入するのが妥当 (推し: keepalive / cache 周りは繰り返し測っているので候補)

### 2-2. 「skip は pass ではない」をレポート形式に焼き込む

- harnessrouter の事実: `protocol/conformance/README.md` の Outcomes 節。PASS / FAIL / SKIP / ERROR を分け、`conformant` は skip 1 件で false、`skipped_not_verified` に id を列挙、`suite_version` と `generated_at` で出所を固定
- kawaz 側の現状: 規約としては test-integrity rule と testing reference に「未検証は明記」「ignore 増は可視化」がある。ただしレポート形式としての実装例は llm-gateway / ccmsg には見当たらない (未確認: 全 findings を通読はしていない)
- 取り込んだ場合: findings テンプレに「検証できなかった組合せ」の欄を必須化する、または実機検証スクリプトの出力 JSON に `skipped_not_verified` 相当を持たせる、という形で既存規約の実装例になる。ERROR (= 道具自身の故障) を FAIL と分ける区分は特に有用
- 取り込まない方がよい理由: 特になし。規約の具体化なのでコストが小さい

### 2-3. 版管理の文書化 (互換性の約束を MUST/MAY で列挙)

- harnessrouter の事実: `protocol/VERSIONING.md` が「版内で許す変更 / 版を上げないとできない変更」を列挙し、CHANGELOG の各版冒頭で前版との互換性を最初に述べる
- kawaz 側の現状: llm-gateway は crate の version を `justfile` の `bump-version` / `check-version-bumped` と release workflow (bump-semver) で上げる。外向きの互換性の約束 (設定ファイル形式、HTTP API、CLI) を列挙した文書は無く、CHANGELOG も無い (リリースノートは Release 側。未確認: GitHub Release 本文の生成方法)
- 取り込んだ場合: llm-gateway の設定ファイル (`config extends` DR-0013 等) と管理 HTTP API は利用者 (kawaz 自身の他環境) が依存するので、「patch/minor で変えないもの」を MANUAL か DR 1 本に書いておくと、破壊変更の判断が DR 単位で揃う
- 取り込まない方がよい理由: 利用者が実質 kawaz 1 人なので、約束文書の維持コストに見合う読者がいない可能性。日付版への移行は bump-semver / brew tap の仕組みと噛み合わないので不要 (日付版はプロトコル仕様だから意味がある)

### 2-4. 「三つを同じ変更で動かす」checklist

- harnessrouter の事実: GOVERNANCE の Implement 節。仕様・schema・参照実装・「変更前 fail / 変更後 pass」の conformance テスト・CHANGELOG を 1 PR で揃えないと ready ではない
- kawaz 側の現状: design-impl-bidirectional-check rule と、docs 標準の「DR の Status と INDEX を同じ commit で更新」、CLI の「実装・help・completion の 3 者追従」がある。DR 実装時の「テストが変更前に fail することを確かめる」は明文化されていない
- 取り込んだ場合: DR を `✅ 実装済` にする条件に「対応するテストが変更前に fail し変更後に pass する」を加えると、実装エビデンスの定義が明確になる。local-issue の close 条件とも合う
- 取り込まない方がよい理由: 全 DR に機械適用すると、命名・思想系 (`N/A`) にまで要求が及ぶ。実装対象の DR に限る必要がある

### 2-5. SECURITY.md (脅威前提 + 報告経路 + scope)

- harnessrouter の事実: `SECURITY.md` 22 行。冒頭で「このソフトは渡した credential で何ができるか」を書き、報告経路と scope / out of scope を示す
- kawaz 側の現状: llm-gateway / ccmsg ともに SECURITY.md は無い。docs 標準も「直下は README と LICENSE のみ」
- 取り込んだ場合: llm-gateway は subscription の OAuth token と Bedrock key を保持する public リポで、脅威前提 (「listen を loopback 以外に出すと token の代理行使を許す」) を 1 箇所に書く価値は高い。GitHub の private vulnerability reporting を有効化すれば報告経路の記述だけで済む
- 取り込まない方がよい理由: 直下ファイルを README / LICENSE に限る docs 標準と衝突する。ただし `SECURITY.md` は GitHub が直下 / `.github/` / `docs/` のいずれでも認識するので (未確認: 2026-09 時点の GitHub 仕様を裏取りしていない)、`.github/SECURITY.md` に置けば標準と両立できる可能性がある。推し: 脅威前提だけは MANUAL か README に書き、ファイル化は裏取り後に判断

### 2-6. README の固定アンカーと alt テキストの書き方

- harnessrouter の事実: 見出しの直前に `<a id="...">` を置き、見出し文言を変えても外部リンクが切れない。図の alt に「図が主張している数値」まで書く
- kawaz 側の現状: llm-gateway README は 54 行で見出しは英語の短文。図は ASCII のテキスト図 (alt 不要)。英日 2 本 (`README.md` / `README-ja.md`) で、justfile に `check-outdated-translations` がある
- 取り込んだ場合: README-ja と README で見出し文言が違っても共通アンカーでリンクを張れる、という利点がある。ただし kawaz の README は短く、外部からの深いリンクもほぼ無いので効果は小さい
- 取り込まない方がよい理由: 空アンカーは markdown の可読性を下げ、既存の短い README には不釣り合い

### 2-7. self-hosting の最小手順を設定ファイル自身に書く

- harnessrouter の事実: `docker-compose.yml` と `.env.example` の冒頭コメントに手順と「secret は env から読み永続化しない」方針が書いてあり、長文ガイドを読まずに起動できる
- kawaz 側の現状: llm-gateway は `just init-config` と MANUAL-ja で設定を案内。配布は brew tap (release workflow の `update-homebrew` job) と `llm-gateway service register` (DR-0028)。Docker 配布は無い
- 取り込んだ場合: `init-config` が生成するテンプレに「最小手順 + credential をどこに置き、何が永続化されるか」をコメントで書く形なら、docs と設定の drift が減る (secret-hygiene の op run 運用とも整合させられる)
- 取り込まない方がよい理由: Docker / compose 配布そのものは、macOS の keychain・launchd 前提の llm-gateway には合わない

### 2-8. 検証規則の正本を CONTRIBUTING から指す

- harnessrouter の事実: CONTRIBUTING の最初の実質節が「Verifying a harness」で、`docs/harness-verification.md` を読んでから harness を足せ、と書く。「green な 1 ターンではなく規則に照らして確認せよ」
- kawaz 側の現状: llm-gateway では upstream 追加時に何を実測すべきかが DR (DR-0014 等) と runbooks に分散している (未確認: 横断して一覧化した文書が無いことの完全な確認はしていない)
- 取り込んだ場合: 「新しい upstream / provider preset を足す時に何を実測して findings に残すか」を 1 本 (`docs/runbooks/` か `docs/design/`) にまとめると、Claude セッションが upstream 追加を任された時の入口になる。kawaz リポでは CONTRIBUTING の読者は主に Claude なので、CLAUDE.md か DESIGN から指すのが相当
- 取り込まない方がよい理由: 特になし

## 3. 参考にしない方がよい点と理由

- **コミュニティ誘導の装置** (stars の 5 分毎更新 workflow、Star CTA 画像、Star History、Discord、Docker pulls バッジ): 集客が目的の装置で、個人ツールには読者がいない。5 分毎の cron は Actions 分を消費する
- **UEP / 10 営業日応答 / Roles / 商標と名乗り規定 / IMPLEMENTATIONS の掲載基準**: 複数の外部実装者と合意形成するための仕組み。kawaz リポの判断記録は DR + QUESTIONS.md で足りており、提案者が kawaz と Claude だけの環境で proposal-first の儀式を足すと速度だけが落ちる。ただし「却下も理由付きで記録して再議論を防ぐ」は DR の「しない決定も 1 本」で既に実現済み
- **main 保護 + `open-pr.yml` で bot 名義 PR を作る運用**: 自己 approve 禁止を回避するための複数人運営の仕組み。kawaz は push-guard hook + `just push` の deps で品質 gate を掛けており、PR レビューを経ない前提なので不要
- **docs/ の平置き (日付なし・DR なし)**: harnessrouter は判断記録を GitHub Issues と CHANGELOG に置き、docs/ は利用者向け成果物だけにしている。kawaz の docs 標準 (decisions / findings / research / issue をリポ内に持つ) は AI セッションが記憶なしで文脈を回復するための構造で、目的が異なる。harnessrouter 側に寄せると失う
- **Apache-2.0 + NOTICE**: 特許条項・商標分離が必要な規模の OSS 向け。kawaz の MIT 規約を変える理由は無い
- **Docker 単一コンテナ配布**: サーバ製品向け。llm-gateway は macOS の keychain・launchd 前提、ccmsg は npm 配布で、どちらにも合わない
- **conformance を外部公開パッケージにする構成**: 仕様を他者が実装する前提があって初めて意味がある。ccmsg の「契約」を他者実装する予定が生まれない限り不要

## 4. 未確認・要裏取りの点

- `ui/` の TypeScript テスト構成 (test runner の種類と CI での実行有無) は `tests.yml` の job 一覧を見ただけで、中身を読んでいない
- `release.yml` の tag 規則と multi-arch の範囲、`retag-latest.yml` / `hub-cleanup.yml` の運用意図は見出し行しか見ていない
- `protocol/site/` と `vercel.json` による仕様サイトのビルドと、conformance バッジ生成の実装箇所
- GitHub が `SECURITY.md` を `.github/` 配下でも認識するか (2-5 の前提)。2026-09 時点の GitHub docs で裏取りが要る
- llm-gateway の GitHub Release 本文の作り方 (CHANGELOG 代替になっているか)
- ccmsg の Claude Code plugin 配布は、依頼文にあった `.claude-plugin/` がリポ直下に無かった。README によれば `ccmsg plugin install claude` で CLI が plugin を配る方式で、npm (trusted publishing / OIDC、`.github/workflows/publish.yml`) で本体を配布している。plugin 同梱物の置き場 (src 配下か) は未確認
- llm-gateway には docs 標準が定める `docs/DESIGN.md` が無く、`docs/design/architecture-overview.md` がその役を担っているように見える (標準との差分。意図的かは未確認)
