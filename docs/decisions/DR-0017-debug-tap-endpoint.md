# DR-0017: デバッグ用 tap endpoint (購読時のみ動く観測口)

- Status: Accepted
- Date: 2026-08-14

## 背景

一時的な観測 (例: thinking 指定の分布調査) のたびにログ出力コードを足して
リリースし、不要になったら外す運用は不便で、外し忘れの常設ノイズも生む
(kawaz 指摘 2026-08-14)。「見たい時だけ繋ぐ」観測口を常設し、コード変更
なしで詳細データを取れるようにする。

## 決定

- **`GET /llm-gateway/tap`** を追加。SSE で 1 exchange = 1 JSON 行を流す
- **subscriber が 0 なら一切の追加コストを発生させない** (atomic な購読数で
  gate し、シリアライズ自体をスキップ)。複数 subscriber には全員に配る
  (broadcast)。遅れた subscriber はバッファを溢れさせず切断する
- **既定はメタデータ + パラメータ要約のみ** (ts / ns / model / route /
  status / thinking 指定の原値 / tool_choice 種別 / stream 有無 / body
  サイズ / credential 等)。**本文は既定で含めない**
- 本文は **query で opt-in**: `?include=request_body,response_body`。
  切り詰め上限は既定 64KiB、`&max_body=<bytes>` で変更可
- **127.0.0.1 からの直接接続のみ許可**。判定は「peer が loopback かつ
  `X-Forwarded-For` 等の proxy ヘッダを持たない」。gateway 自体は
  127.0.0.1 bind だが Caddy (tailnet 公開) が localhost から中継してくる
  ため、proxy ヘッダの有無で直結と中継を区別する
- 用途例: `curl -N 'http://127.0.0.1:11301/llm-gateway/tap?include=request_body' > dump.jsonl`

## 採らなかった案

- **既存 `GET /llm-gateway/events` (DR-0012) の拡張**: events は ccmsg 連携
  等の恒常消費者を持つ公開契約で、フィールド追加が下流を揺らす。tap は
  「その時見たいものを流す」揮発的な口として分離する
- **常設の詳細ログ**: 外し忘れがノイズ・容量・機微性の常設コストになる。
  本 DR の動機そのもの
- **認証トークンによる保護**: 127.0.0.1 直結限定 + 本文 opt-in で足りる
  (境界設計は DR-0006 と同じ tailnet 信頼 + それより内側の loopback 限定)。
  token 管理の手間が利便を食う

## 運用ノート

- 本文には会話内容がそのまま含まれる。dump したファイルの置き場・削除は
  取得者の責任 (リポや公開物に貼らない)
- DR-0016 実装時に入れた thinking 観測の常設 INFO ログは、tap 導入後に
  削除する (tap で代替)
