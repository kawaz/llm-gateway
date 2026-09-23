---
title: config のパス値で `~` と環境変数を展開する
status: resolved
category: task
created: 2026-09-23T16:48:56+09:00
last_read:
open_entered: 2026-09-23T16:48:56+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-23T17:08:26+09:00
discard_reason:
pending_reason:
close_reason: ["implemented: v0.50.0 で展開を実装", "done: 稼働 config (dotfiles) の binary_path を ~ 表記へ直し、unstable を 0.50.0 で再起動・疎通確認済み", "done: runbook 側に絶対パスは残っていない"]
blocked_by:
origin: 自リポ TODO
---

# config のパス値で `~` と環境変数を展開する

## 概要

config のパス値 (`[server] binary_path`、`[credentials] dir`、`[stats] dir`、`extends` のパス等、`PathBuf` で受けている全項目) で `~` と環境変数 (`$HOME`、`${XDG_DATA_HOME:-~/.local/share}` の形) を展開する。

## 背景

今は `PathBuf` をそのまま受けるため、稼働 config (dotfiles) に実マシン依存の絶対パス (例: `.../target/release/llm-gateway`) が焼き込まれていて、マシンを変えると壊れる。docs (`docs/runbooks/2026-09-09-migrate-launchd-to-service.md`) も実値を写すしかなく、`sanitize-local-paths` の対象 (docs-lint `absolute-home-path`) に恒常的に引っかかる。

展開規則は XDG の既定 (`$XDG_DATA_HOME` 無しなら `~/.local/share`) と一貫させ、`config.rs` の `xdg_dir()` を再利用する。展開は読み込み時 1 回、`llm-gateway check --config` の出力には展開後の実パスを出す (存在確認もそれで行う)。実装後に runbook と dotfiles の config を `~/` 表記へ直す。

関連: DR-0028 (binary_path)、DR-0011 (stats dir)、DR-0013 (extends)。

## 受け入れ条件

- [ ] `[server] binary_path` / `[credentials] dir` / `[stats] dir` / `extends` のパス値で `~` と環境変数 (`$HOME`、`${XDG_DATA_HOME:-~/.local/share}` 形) が展開される
- [ ] 展開ロジックが `config.rs` の `xdg_dir()` を再利用し、規則が XDG の既定と一貫している
- [ ] 展開は config 読み込み時に 1 回だけ行われる
- [ ] `llm-gateway check --config` の出力・存在確認が展開後の実パスに対して行われる
- [ ] 実装後、runbook (`docs/runbooks/2026-09-09-migrate-launchd-to-service.md`) と dotfiles の config を `~/` 表記に直す
