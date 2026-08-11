# cache_control は checkpoint ではない / rewind の実体

関連: [2026-08-11-model-effort-cache-mapping.md](./2026-08-11-model-effort-cache-mapping.md)

## 1. cache_control の正体 (課金・レイテンシ最適化の印、semantics ゼロ)

`cache_control: {type: "ephemeral"}` は「ここまでの prefix の計算結果 (KV cache) を
サーバに取っておき、次に同じ prefix が来たら再計算せず安く速く返す」ための印。

- TTL は `{type: "ephemeral"}` = 5 分 (書込 1.25 倍) / `{..., ttl: "1h"}` = 1 時間
  (書込 2 倍)。読出はどちらも 0.1 倍。「5 分か 1 時間かの指定」という理解で正しい
- **会話状態には一切影響しない**。pin でも checkpoint でも rewind 用の印でもない。
  付けても外しても応答内容は同じで、変わるのは請求額と初速だけ
- 「breakpoint」も巻き戻し地点ではなく「prefix のどこまでをキャッシュ対象にするか」
  の区切り。1 リクエストに最大 4 個

## 2. API 側に rewind という概念は無い

Messages API はステートレスで、毎回 `messages[]` の全履歴を送る。したがって
「巻き戻し」は **client が履歴を途中まで切り詰めて送り直すだけ**で成立する。
その時、切り詰めた地点までの prefix は前回と同一なので、キャッシュが生きていれば
自動的に読出料金 (0.1 倍) でヒットする。つまり cache と rewind は無関係だが、
rewind した時にキャッシュが効くのは prefix 一致の自然な帰結。

## 3. Claude Code の rewind (checkpoint) の仕組み

API ではなくクライアント側の機能。

- **各ユーザプロンプトの前**に自動でチェックポイント作成 (ファイルスナップショット +
  会話位置)。セッション内最新 100 個、セッション削除 (既定 30 日) と共に消える
- `/rewind` のメニューは「セッション中に送ったプロンプト」の一覧 = **ユーザ発言単位**
  でしか選べない。復元は 会話+コード / 会話のみ / コードのみ / 前後の Summarize
- 追跡外: bash による変更 (rm/mv/cp)、background subagent の編集、外部変更、
  symlink/hardlink

**ccmsg 経由の受信は checkpoint を作らない**。rewind メニューの述語が
「origin があるなら human でないと除外」で、ccmsg 受信の実体 (task-notification) は
全て弾かれるため (ccmsg セッションが Claude Code バイナリ内 JS を直接読んで確定、
2026-08-11)。origin はコアが配送経路で付与するので plugin から介入不可。
一方、会話の実体はセッション jsonl なので、任意行で切り詰めて別 sid にコピーし
`--resume` すれば任意点への「会話のみ rewind」は成立する (同セッションが実測)。
詳細は kawaz/claude-ccmsg の `docs/findings/2026-08-11-checkpoint-rewind.md`。
