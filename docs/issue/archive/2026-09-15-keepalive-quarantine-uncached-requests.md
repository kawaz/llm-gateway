---
title: cache に乗らなかった request を研究用に一定期間・一定量だけ退避する
status: resolved
category: task
created: 2026-09-15T11:30:37+09:00
last_read:
open_entered: 2026-09-15T11:30:37+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-15T13:15:16+09:00
discard_reason:
pending_reason:
close_reason: ["done: v0.49.0 で実装 (DR-0027 決定 9、cache/keepalive/uncached.rs)、v0.49.1 として両 unit に展開済み (2026-09-15)。既定 50 件 / 7 日、[stats] uncached_keep / uncached_days。退避先 stats/keepalive/uncached/ は最初の該当 request が出た時に作られる"]
blocked_by:
origin: kawaz 承認 (2026-09-15)
---

# cache に乗らなかった request を研究用に一定期間・一定量だけ退避する

## 概要

cache に乗らなかった (= `cache: none` で応答した) request を、捨てずに研究用として一定期間・一定量だけ退避する。退避先は `<stats.dir>/keepalive/uncached/<session>.<prefix>.<sent_at_ms>.json`。

## 背景

v0.46.3 以降、応答が `cache: none` の request は控えず (入口、`Keeping::settled`)、自送信が none を返した系列は畳む (`Keepalive::not_landed`)。どちらも本文を捨てるため、`cache_control` を持つのに乗らなかった 3 系列 (48b85fe7 / 78937d64 / 96c83ec2、`docs/research/2026-09-14-claude-code-uncached-requests.md` の「不明」) のような事例を後から分析できない。

## 方針

- 退避先: `<stats.dir>/keepalive/uncached/<session>.<prefix>.<sent_at_ms>.json`
- 中身: 控えと同じ形 (本文・headers・model・route・ns) に加え、判定材料 (応答の usage、`Cache` の語、応答 status、入口か自送信か) を添える
- 保持: 「新しい順に N 件 (既定 50) かつ M 日 (既定 7 日)」。書く時と起動時に超過分を消す
- 上限の置き場: config `[[ns.<ns>.cache]]` ではなく `[stats]` 直下 (`uncached_keep = 50` 等)。研究用の退避であって cache 戦略ではないため
- プライバシー保護策: DR-0027 決定 4 と同じ (付けない)
- 書けなくても転送は止めない (warn のみ)

## 受け入れ条件

- [ ] `cache: none` で応答した request (入口・自送信いずれも) が `<stats.dir>/keepalive/uncached/<session>.<prefix>.<sent_at_ms>.json` に退避される
- [ ] 退避データに本文・headers・model・route・ns に加え usage / `Cache` の語 / 応答 status / 入口か自送信かが含まれる
- [ ] 新しい順 N 件 (既定 50) かつ M 日 (既定 7 日) を超えた分が、書き込み時と起動時に削除される
- [ ] 保持上限が `[stats]` 直下の設定 (`uncached_keep` 等) で変更できる
- [ ] 退避書き込みに失敗しても転送は継続し、warn ログのみが出る
