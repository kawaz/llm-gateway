# DR-0016: namespace 単位の thinking.display 強制上書き

- Status: Accepted
- Date: 2026-08-14

## 背景

Anthropic API の `thinking.display` は Fable5/Opus5/Sonnet5/Opus4.8/4.7 で
デフォルト `"omitted"` (thinking ブロックが空文字 + signature のみ)。
Claude Code は `showThinkingSummaries: true` を設定していても、omitted
デフォルトのモデルに `display: "summarized"` を送らない既知 bug がある
(anthropics/claude-code#49268、open)。このため 2026-08-11 頃から transcript
の thinking が大量に空になり、ccmsg webui 等の下流で思考が見えなくなった。

subscription OAuth 経由でも server が `display` を尊重することは実機確認済み
(2026-08-14: haiku に `display:"omitted"` を注入すると空 thinking + signature
が返った)。gateway は ingress で request body を解析しているので、ここで
`display` を強制できる。

## 決定

- **namespace 設定に optional な `thinking_display` を追加** (値は
  `"summarized"` / `"omitted"` のみ、他は config validation エラー)
- 設定された ns への Messages リクエストで:
  - body に `thinking` object があれば `display` をこの値で上書き
  - `thinking` が無ければ `{"type": "adaptive", "display": <値>}` を注入
    (adaptive は 5 系の推奨形。thinking を持たない旧モデルは ns filter で
    既に隠しているため副作用は実質ない)
- 未設定の ns は従来どおり無改変で透過 (デフォルト挙動の変更はしない)
- 適用は provider を問わず ingress の正規形 (Anthropic Messages 形) に対して
  行う。OpenAI preset は変換時に thinking を Responses の reasoning へ写す
  既存経路のままで、display は Anthropic 固有フィールドとして変換で落ちる

## 採らなかった案

- **route / credential 単位の設定**: display は「クライアントに思考を見せるか」
  という ingress (ns) の関心で、接続先の関心ではない
- **常時強制 (設定なしで summarized 固定)**: 透過 proxy としての既定挙動を
  黙って変えることになる。omitted を意図するクライアントの意図も潰すため、
  opt-in の設定にする
- **Claude Code 側の fix を待つだけ**: #49268 は 30 日以上 open。下流
  (transcript / webui) の思考可視性が日々失われるため workaround を先に置く。
  fix が出て CC が display を送るようになっても、本設定は「CC が送る値を
  上書きする」だけで競合しない (不要になったら config から消せばよい)
