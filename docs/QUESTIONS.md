# 裁定・確認待ち一覧 (ユーザ用)

## 運用規約

<details>
<summary>ゼロコンテキストエージェント向け（本セクションは消さない）</summary>

- 裁定/確認待ち項目を 1項目=1ラベル=1セクション で記載
- ラベル形式: XX-Q1（XX は 2-3 文字、バッチやセッション内で一意、Qn単独の使い回し禁止、長期一意性は不要)
- 依頼形式: 「👺XX-Q1 の裁定お願いします」（参照用途ではラベルに👺を付けない。誤陽性がユーザのハイライト/アラームを汚す）
- チャット提示と同一ターンで本ファイルに記録 + path 指定 commit (push はリリース窓に同乗)
- 裁定が下りたら該当セクションを即削除し、内容は正規の記録先 (DR / issue / journal / close_reason) へ反映。本ファイルは常に「現在待ち」だけを持つ
- 参照は[]()で提示（リポ内は相対、リポ外はフルパス）
- 初版質問/依頼は長文で書かない（ユーザが説明を求めらたら本ファイルに説明を追加し、チャットで👺ラベルで再依頼）
- **選択肢・確認項目は `- [ ] a: …` 形式（チェックボックス + ラベル）で書く**。Q / C で記法を分けない。回答は「チェックを付ける」でも「XX-Q1a」と言葉で返すでも通る（複数まとめてチェックし「チェックしたよ」の一言で済ませる運用を想定）

</details>

## 裁定待ち

（現在なし）

## 確認待ち

### JW-C1β: 秘密鍵の置き場 (暫定 b で鋳造済み)

未回答のまま (1) を進めるため、b (`~/.config/llm-gateway/private/<kid>.jwk`、ディレクトリ 700、dotfiles の `.gitignore` に追加済み) で 3 ns 分 (`personal-2026-09` / `bare-2026-09` / `emrd-2026-09`) を鋳造した。a に寄せるなら JWK を 1Password に移してファイルを消すだけで済む (公開鍵は config に貼るので影響なし)。

- [ ] a: 1Password の personal vault に移す (統括が `op item create` を打ってよいなら指示を。値を context に乗せずに移すには kawaz の手元が確実)
- [ ] b: このまま `~/.config/llm-gateway/private/` で運用

### JW-C2: JWT の鋳造と貼り付けを kawaz の手元で実行

`settings.json` / codex config への書き込みは auto モード分類器が拒否した (JWT は Secret-Store Writes、`ANTHROPIC_DEFAULT_SONNET_MODEL` の変更も Self-Modification)。以下を `!` で実行してほしい (値は表示しない。`.bak-jwt-20260929` を残す)。gateway は ns が Open のうちは検査しないので、貼った直後から新旧どちらの値でも通る。同じ run で既定 Sonnet を `claude-sonnet-5-5[1m]` に切り替える (gateway 経由の疎通と stats 記録は統括が確認済み、`docs/research/2026-09-29-sonnet-5-5-adoption.md`)。

```bash
umask 077; P=~/.config/llm-gateway/private
for ns in personal bare emrd; do
  f=~/.claude-$ns/settings.json; cp "$f" "$f.bak-jwt-20260929"
  llm-gateway auth sign --sub "claude-$ns" --ttl 180d < "$P/$ns-2026-09.jwk" > "$P/.tok"
  jq --rawfile t "$P/.tok" '.env.ANTHROPIC_AUTH_TOKEN = ($t | rtrimstr("\n")) | .env.ANTHROPIC_DEFAULT_SONNET_MODEL = "claude-sonnet-5-5[1m]"' "$f" > "$f.new" && mv "$f.new" "$f"
done
llm-gateway auth sign --sub codex-personal --ttl 180d < "$P/personal-2026-09.jwk" > "$P/.tok"
C=~/.codex/config.toml; cp "$C" "$C.bak-jwt-20260929"
perl -i -pe 'BEGIN{open F,"<","'"$P"'/.tok"; chomp($t=<F>); close F} s{^(base_url = ".*/ns-personal/v1")$}{$1\nhttp_headers = { Authorization = "Bearer $t" }}' "$C"
rm -f "$P/.tok"; echo done
```

- [ ] a: 実行した (統括が `jq .env.ANTHROPIC_AUTH_TOKEN | tr -cd . | wc -c` で 3 分割 = JWT 形式かだけ確認して次へ)
- [ ] b: 代わりに Bash permission rule を足すので統括が実行してよい

### JW-C1γ: 監督者の 0.59.2 化の窓

`llm-gateway version` は `supervisor.running = 0.56.0`、`restart_needed = true`、動作に支障なし。`service stop` は抱えている stable / unstable も落とすので、gateway 経由の全セッション (この統括を含む) の走行中 request が切れる。Open 期間方式に変えたので personal 切替とは独立になった。

- [ ] a: 各 ns の `auth = "jwt"` 切替 (restart が要る) の最初の窓で一緒にやる (統括推し)
- [ ] b: 今すぐやってよい (数秒の断)
