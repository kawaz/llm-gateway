---
title: メンテ終了になった humantime を置き換える
status: resolved
category: task
created: 2026-11-19T19:40:00+09:00
origin: CI の cargo-audit の警告 (2026-11-19、v0.73.3)
close_reason: implemented
---

# メンテ終了になった humantime を置き換える

## 概要

CI の cargo-audit が `humantime` 2.4.0 を unmaintained として警告した (RUSTSEC-2026-0542、2026-09-14)。理由は、humantime が包んでいる chrono がメンテ終了になったこと。今は警告扱いなので CI は落ちない。脆弱性ではない。

## 使っている所

`crates/llm-gateway/src/config.rs` だけで、設定の時間の値 (`30s`、`1h 30m` のような表記) を読み書きしている。

- `humantime::parse_duration`: 設定の読み込み
- `humantime::format_duration`: 設定の書き出しと案内文

依存は `llm-gateway` crate からの直接の依存だけで、他の crate を経由して入ってはいない (`cargo tree -i humantime`)。

## やること

- [x] 代わりの crate を選ぶか、自前で読み書きするかを決める。設定に書ける表記 (複合表記を含む) が変わらないことを条件にする
- [x] 置き換えて、既存の設定の試験がそのまま通ることを確かめる
- [x] `cargo audit` の警告が消えたことを確かめる

## 結果

自前の読み書き (`crates/llm-gateway/src/config/duration.rs`) に置き換えた。`crates/llm-gateway/src/dump.rs` の案内文 (`--timeout` の上限) も humantime の書き出しを使っていたので一緒に移した。

- **jiff を選ばなかった理由**: jiff は依存の木に既にあるが、friendly 形式は `0`、前後の空白、月・年 (`M` / `month` / `y`) を読めず、`SignedDuration` では日 (`2d`) も読めない。逆に負の長さ (`-5m` / `5m ago`) や ISO 8601 (`PT5M`) を読んでしまう。包んで差を埋めるより、書き方を直接持つ方が小さい
- **読み**: humantime 2.4.0 と同じ書き方を読む。humantime 2.4.0 と並べた差分試験 (無作為な文字列 200 万件と、有効な形に寄せた 200 万件) で、両方が読めた値の食い違いは 0 件。humantime が断って自前が読むのは、時以上の単位で秒未満になる小数 (`5.512wk`) と `1.0ns`、u64 のナノ秒を超える値だけで、今読める設定が読めなくなる側の差は無い
- **書き出し**: humantime と同じ形 (`2days 6h`、`1year 2months`)。無作為な長さ 200 万件で文字列の食い違い 0 件、読み戻しの食い違い 0 件
- `cargo audit` は警告なし (HEAD の Cargo.lock では RUSTSEC-2026-0542 が出る)
