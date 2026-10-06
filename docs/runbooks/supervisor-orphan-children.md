# Runbook: 監督者が落ちて残った子を畳む

- Last Updated: 2026-10-06

## 適用ケース

- 監督者 (`llm-gateway daemon supervise`) が SIGKILL・異常終了で消えた後、OS (launchd / systemd) が上げ直した監督者の下で台が上がらない
- `llm-gateway daemon status` で、`enabled: true` なのに `running: false` のまま `restarts` が増え続けている
- 監督者のログ (`llm-gateway service log`) に `stopped ... is held by pid <N>, not by a child of this supervisor` が出ている

## 前提

- 監督者が行儀よく降りる (SIGTERM / SIGINT、`llm-gateway service stop`) 限り、子は道連れになって残らない。残るのは監督者が SIGKILL や異常終了で消えた時だけ (DR-0028 決定 11)
- 監督者は残った子を**引き取りも畳みもしない**。pid は使い回されるので、残っているプロセスが前の監督者の子だと再起動を跨いで確かめられず、確かめられないものを畳むと無関係のプロセスを落としうるため。監督者がするのは、ポートが塞がれていることと、握っている pid を見せるところまで
- 握っている pid は `lsof` で引く。`lsof` が無い環境 (Linux の最小構成等) では `pids` が `null` になり、塞がっていることだけが分かる

## 手順

1. **塞がれていることを確かめる。**
   ```bash
   llm-gateway daemon status --all
   ```
   期待結果: 上がらない台の行が次の形になっている。
   ```json
   {"unit":"stable","enabled":true,"running":false,"restarts":7,
    "last_exit":"exit status: 1; 127.0.0.1:11302 is held by pid 12345, not by a child of this supervisor (possibly left behind by a supervisor that died); find it with `lsof -nP -iTCP:11302 -sTCP:LISTEN`",
    "port":{"listen":"127.0.0.1:11302","held":true,"pids":[12345],"foreign":true}}
   ```
   `port.foreign: true` が「ポートを握っているのは今の監督者の子ではない」。`port.held: false` なら塞がれてはいないので、このランブックの対象ではない (`last_exit` と `llm-gateway daemon log <unit>` で上がらない理由を見る)

2. **握っているプロセスを特定する。** ポート番号は `port.listen` のもの。
   ```bash
   lsof -nP -iTCP:11302 -sTCP:LISTEN
   ps -o pid,ppid,lstart,command -p 12345
   ```
   期待結果: `COMMAND` が `llm-gatew…`、`ps` の `command` が `<binary> daemon run <unit>` で、`ppid` が `1` (親の監督者が消えて init / launchd に引き取られている)。**これ以外のプロセスだったら畳まない** — 別のプログラムがそのポートを使っているので、台の `[server] listen` を変えるか、そのプログラムの側を止めるかを決める

3. **残った子を畳む。** 子は SIGTERM で行儀よく終わる (書きかけの応答を書き終えてから降りる)。
   ```bash
   kill -TERM 12345
   lsof -nP -iTCP:11302 -sTCP:LISTEN   # 何も出なくなるまで待つ
   ```
   10 秒ほど待っても残るなら `kill -KILL 12345`

4. **台が上がったことを確かめる。** 監督者は backoff の間隔 (最大 60 秒) が明けた時に起こし直す。待たずに今すぐ起こすなら `llm-gateway daemon restart <unit>`。
   ```bash
   llm-gateway daemon status <unit>
   ```
   期待結果: `running: true`、`port.foreign: false`、`port.pids` が今の `pid` と一致する

## 確認

- `llm-gateway daemon status --all` の全行で `port.foreign` が `false` (または `port` 欄が無い = 待ち受け先を読めない台)
- 手前に Caddy 等の振り分けがある構成では、畳んでから上がるまでの間はもう一方の台に流れる。両方の台が同時に塞がれていた場合は、片方ずつ畳んで上がりを確かめてから次へ進む

## 関連

- DR-0028 決定 11 (監督者が落ちても子は残る。次の監督者は拾わずに起こし直す)
- `docs/runbooks/2026-09-09-migrate-launchd-to-service.md` (監督者 1 本に移す手順)
