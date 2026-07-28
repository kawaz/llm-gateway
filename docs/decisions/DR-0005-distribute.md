# DR-0005: 配布する (GitHub Release + Homebrew tap)

- Status: Active
- Date: 2026-07-28

## Context

[DR-0002](./DR-0002-component-architecture.md) の「配布しない」節は、kawaz 裁定
(2026-07-27「当面配布しない。個人よう。」) に基づいて release workflow・version bump
gate・英訳ペアを持たない構成を決めていた。

2026-07-28、kawaz がこの裁定を覆した。

> 個人仕様だから云々は覆されています。

## Decision

配布する。DR-0002 の「配布しない」節は本 DR で無効になる。

| 項目 | DR-0002 | 本 DR |
|---|---|---|
| GH Release / tag / 配布 artifact | 無し | **作る** |
| `check-version-bumped` gate | 無し | **持つ** |
| Homebrew tap | (言及なし) | **作る** |
| codesign | する | する (変更なし) |
| notarize | 無し | **する** |
| README / DESIGN の英訳ペア | 無し | 未確定 (下記) |

### notarize が要るようになった理由

DR-0002 が notarize を不要とした根拠は「ローカルビルドは quarantine されない」だった。
配布物はダウンロード経由で手元に来るので quarantine される。前提が変わったので要る。

### 署名の位置づけは変わらない

codesign は元から cache-warden の peer 認証に乗るためのもので、配布のためではなかった
(DR-0002 に経緯がある)。配布を始めても理由が 1 つ増えるだけで、既存の設計は動かない。

### ビルドターゲット

linux x86_64 / aarch64 + darwin x86_64 / aarch64 の 4 つ。

Rust コードに `cfg(target_os)` は 1 箇所も無く、OS 依存は justfile 側 (launchd 登録と
codesign) に閉じている。linux で動かない理由が無いので、配布先を macOS に狭めない。

### 構成の借り元

`kawaz/hyoui` の release workflow を骨格にする (version 正本は `Cargo.toml`、
「最新 release」と「最新 tag」の両方を超えたときだけ release、tap は同一 workflow が
deploy key で push)。macOS の署名と notarization は `kawaz/cache-warden` から移す。

## 未確定

- **public 化のタイミング**。英訳ペア (README / DESIGN) の要否がこれに連動する。
  private のままでも GH Release と tap は動くので、リリース基盤の構築は先行できる。

## 関連

- [DR-0002](./DR-0002-component-architecture.md) — コンポーネント構成。「配布しない」節のみ本 DR で無効
