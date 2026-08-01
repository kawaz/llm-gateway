# Claude Code は枠を自分で見て、リクエストを送る前に諦める

## 判明した事実

- **Claude Code は、モデル別の枠 (fable の weekly_scoped) が上限だと、リクエストを
  送らずに打ち切る。** gateway には 1 本も届かない
- **その判断材料は gateway を通らない。** Claude Code は枠を
  `GET https://api.anthropic.com/api/oauth/usage` へ**直接**聞いている (2026-08-01、
  透過プロキシで実測)。`ANTHROPIC_BASE_URL` の向き先には現れない
- したがって **gateway 側からは介入できない**。枠を書き換えて見せることも、
  「別の経路が空いている」と伝えることもできない
- この事前チェックが働くのは **`ANTHROPIC_AUTH_TOKEN` が空** のとき。値が入っていると
  Claude Code は API キーとして扱い、サブスクの枠を見なくなる (代わりに `/status` の
  サブスク表示も出なくなる)

## 実用的な示唆

`ANTHROPIC_AUTH_TOKEN` の有無は、次の 2 つを**同時に**切り替える。片方だけは選べない。

| `ANTHROPIC_AUTH_TOKEN` | 枠の事前チェック | `/status` のサブスク表示 |
|---|---|---|
| 空 (現在の運用) | **する** (枠上限で送信前に諦める) | 出る |
| 何か入っている | しない | 出ない |

gateway は受け取った値を捨てて credential のトークンに差し替える (DR-0006) ので、
入れる値は何でもよい。`/status` を使わない面では、ダミーを入れておくと
「別 credential や Bedrock が空いているのに fable が使えない」状況を避けられる。

## 検証の詳細

### 症状 (kawaz 報告)

`ANTHROPIC_AUTH_TOKEN` なしで繋いでいるセッションで、Bedrock 経由の fable が開いて
いるにもかかわらず、fable を試しすらせず枠判定で止まった。

### そのときの枠 (2026-08-01 13:30 頃)

| credential | weekly_all | fable (weekly_scoped) |
|---|---|---|
| claude-kawazzz | 100% | 80% |
| claude-emrd | 100% | 57% |
| claude-zunsystem | 73% | **100%** |

### gateway 側のログ (同時刻)

| 観測 | 結果 |
|---|---|
| fable の転送 (直近 200 件) | **すべて `route="bedrock-use1"` で 200** |
| 「どの経路も断られています」 | `claude-opus-5` と `claude-sonnet-5` のみ、`routes=3` |
| 症状が出たセッションからの fable リクエスト | **ログに無い** (届いていない) |

`routes=3` は OAuth 3 枚だけを候補にした判定で、opus / sonnet の routing は Bedrock を
含まない (fable 専用に絞ってあるため) から正しい。fable の routing は Bedrock を含み、
実際に Bedrock で通っている。**gateway は fable を止めていない。**

### 結論

gateway に届いていないリクエストを gateway は止められない。症状は Claude Code 側の
事前チェックによるもので、その判断は gateway を経由しない直通の問い合わせに基づく。

## 関連

- `docs/decisions/DR-0006-namespace-routing.md` — 受け取った認証情報を捨てて差し替える
- `docs/decisions/DR-0007-usage-visibility.md` — gateway 自身も同じ口から枠を読む
- `docs/issue/archive/2026-08-01-oauth-denial-skips-bedrock-fallback.md` — この調査の発端
