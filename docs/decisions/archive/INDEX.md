# 退避した Decision Records

後続の DR に置き換えられた DR。各ファイルの `Status: Superseded by DR-NNNN` が正本で、現役の文書からはここも各ファイルも名指ししない (置き換え先を指す)。

| DR | 説明 | 置き換え先 |
|---|---|---|
| [DR-0033](DR-0033-lock-session-to-first-account-and-send-crossed-thinking-as-text.md) | thinking が account に束縛されるモデルの session を開始 account にロックし、開始 account が全滅した時の振る舞いを `on_account_switch` で選ぶ。跨いだ session は以後 thinking を text として送る | [DR-0035](../DR-0035-judge-thinking-crossing-by-session-sources.md) |
| [DR-0034](DR-0034-persist-session-account-lock-in-store.md) | 開始 account と「跨いだ」印を `[stats] dir` の共有ファイル (`account-lock/locks.json`) に永続化し、restart と unit 間で共有する | [DR-0035](../DR-0035-judge-thinking-crossing-by-session-sources.md) |
