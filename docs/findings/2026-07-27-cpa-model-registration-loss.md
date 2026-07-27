# cpa が稼働中に fable-5 の Bedrock 登録を内部で失う

- Date: 2026-07-27
- 環境: CLIProxyAPI 7.2.100 (Homebrew) / 個人面 8317

## 判明した事実

1. **cpa は稼働中に、設定ファイルに書かれた upstream 登録を内部で失うことがある**。
   fable-5 (Bedrock 専用) が `/v1/models` の一覧から消え、
   `auth_unavailable: no auth available (providers=claude, model=claude-fable-5)`
   の 503 を返す状態になった
2. **設定ファイルも upstream も正常なまま起きる**。config-personal.yml は無傷
   (`claude-api-key` エントリ・`oauth-excluded-models` とも)、
   Bedrock に `x-api-key` で直接投げれば 200
3. **プロセスの再起動を伴わない**。cpa は 18:29 起動でそのまま稼働しており、
   19:05:55 には fable-5 が正常処理されていた。19:29:27 から 503 に転じた
4. **影響は当該モデルに限定される**。opus-5 / sonnet-5 / haiku / gpt-5.6-sol は
   全て 200 のままだった
5. **`launchctl kickstart -k` で復旧する**。8317 は 1 秒で LISTEN 復帰し、
   `/v1/models` に fable-5 が戻り、全 5 モデルが 200 になった

## 実用的な示唆

- **設定に書かれた経路を内部エラーで候補から除去する設計は、この形の障害を生む**。
  llm-gateway では、設定にある経路は常に試行対象として残し、連続失敗は
  「一時的に後回しにする」までに留める (DR-0002 に反映済み)
- **単一障害点は実際に落ちる**。`oauth-excluded-models` で OAuth を塞ぐ構成は、
  Bedrock 登録が失われた瞬間に fable-5 が全滅する。llm-notes DR-0001 が
  「単一障害点である」と予告していたリスクが、別の原因 (プロセス消失ではなく
  内部登録の喪失) で 2 度目の顕在化をした
- **`/v1/models` の内容は障害の切り分けに使える**。「設定にあるのに一覧に無い」
  = cpa の内部状態の問題、と即座に判定できた
- 根本原因 (なぜ登録が失われたか) は**未特定**。エラーログは
  `error-logs-max-files: 10` のローテートで消えていた。再発したら
  この値を上げてから観測する

## 検証の詳細

### 切り分けの手順と結果

| 確認項目 | 結果 | 意味 |
|---|---|---|
| Bedrock に `x-api-key` 直投げ | 200 | upstream・キーとも正常 |
| config-personal.yml の内容と mtime | 無傷 / 18:29 | 設定の書き戻しではない |
| 他モデル (opus-5/sonnet-5/haiku/sol) | 全て 200 | cpa 全体の障害ではない |
| `/v1/models` の一覧 | **fable-5 のみ欠落** | cpa 内部の登録喪失と確定 |
| cpa プロセスの起動時刻 | 18:29 (再起動なし) | 起動失敗ではない |

### 時系列

```
18:29:31  cpa (personal) 起動
18:48-18:52  fable-5 正常 (auth=claude:apikey:2130dc198f56)
19:05:55  fable-5 正常 (session-affinity: cache miss, new binding)
19:29:27  503 発生
19:30:04  503 継続 (自然回復せず)
19:33     launchctl kickstart -k → 1 秒で復帰、全モデル 200
```

### 復旧コマンドと確認

```bash
launchctl kickstart -k gui/501/com.kawaz.cliproxyapi-personal
# 8317 の LISTEN 復帰を待ってから /v1/models と実リクエストで検証
```

もう一方の面 (8318、業務用インスタンス) は無傷であることを再起動の前後で確認した
(`kickstart` の対象を personal のラベルに限定しているため)。

## 関連

- [DR-0002](../decisions/DR-0002-component-architecture.md) — 「設定に書かれた経路は内部エラーで消さない」の根拠
- `kawaz/llm-notes` DR-0001 — fable-5 の Bedrock 専用化 (単一障害点であることを予告していた)
