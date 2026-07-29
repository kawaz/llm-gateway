# Issue Index

| date | category | status | issue | 概要 |
|---|---|---|---|---|
| 2026-07-29 | design | open | [upstream-tcp-keepalive-library-default](./2026-07-29-upstream-tcp-keepalive-library-default.md) | upstream 接続の TCP keepalive をライブラリ既定任せにしている |
| 2026-07-29 | design | open | [request-body-full-buffer-before-forward](./2026-07-29-request-body-full-buffer-before-forward.md) | リクエストボディを全量メモリに載せてから転送している(応答側との非対称) |
| 2026-07-29 | bug | open | [refresh-task-panic-strands-in-flight](./2026-07-29-refresh-task-panic-strands-in-flight.md) | refresh の detached task が panic すると in_flight の acquire が永久待ちになる (低優先) |
