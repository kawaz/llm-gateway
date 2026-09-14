# 合図方式の keepalive を降ろす

DR-0027 の段階 B。gateway から合図方式 (marker / nonce / `cache_keepalive` /
peers 中継) の実装を落とし、設定語 `keepalive` が自送信を指すようになった。
**稼働中の設定は `main = "replay"` と `[server] peers` を書いたままなので、
新しい binary はその設定を読めずに起動に失敗する。** 入れ替えの前にこの手順で
設定を直す。

## 直すもの

| 対象 | 今 | これから |
|---|---|---|
| `[[ns.*.cache]]` の `main` | `"replay"` | `"keepalive"` |
| `[server] peers` | 2 台を列挙 | 欄ごと削除 |
| `<stats dir>/keepalive/127-0-0-1-*.json` | 合図方式の見張り | 削除 |

`~/.config` は dotfiles (`~/.dotfiles`) の working copy。config を触ったら
向こうで commit する。

## 手順

### 1. 設定から `replay` と `peers` を落とす

両方の config (`config-11301-unstable-new.toml` / `config-11302-stable.toml`) で:

```bash
cd ~/.config/llm-gateway
sed -i '' 's/^main = "replay"$/main = "keepalive"/' config-11301-unstable-new.toml config-11302-stable.toml
```

`peers` は前後にコメント行が付いているので、手で消す:

```toml
# 対称に動く一式を自分含みで書く (keepalive の停止を渡す先、DR-0024 §2 追補)
peers = ["127.0.0.1:11301", "127.0.0.1:11302"]
```

新しい binary で読めることを確かめる (`peers` が残っていれば
`unknown field 'peers'`、`replay` が残っていれば戦略の値として弾かれる):

```bash
<repo>/main/target/release/llm-gateway check --config ~/.config/llm-gateway/config-11301-unstable-new.toml
<repo>/main/target/release/llm-gateway check --config ~/.config/llm-gateway/config-11302-stable.toml
```

`sub = "keepalive"` が書けるようになったが、既定は据え置き (DR-0027 決定 5)。
実測を見てから決めるので、この移行では触らない。

### 2. 入れ替えて起こす

いつもの順 (unstable を先、確認してから stable)。控え
(`<会話>.<系列>.json`) はそのまま読み戻される — 形は変わっていない。

### 3. 合図方式のファイルを掃除する

置き場 (`~/.local/state/llm-gateway/stats/keepalive/`) に、合図方式の見張りが
待ち受けごとのファイルで残っている。新しい gateway は自分の命名
(`<会話>.<系列>.json`) だけを読み、それ以外は無視するので**放置しても動く**が、
読まれないファイルを残す理由も無い。

```bash
ls ~/.local/state/llm-gateway/stats/keepalive/127-0-0-1-*.json
rm ~/.local/state/llm-gateway/stats/keepalive/127-0-0-1-*.json
```

(2026-09-14 時点で 11300 / 11301 / 11302 / 11399 の 4 つ。`<会話>.<系列>` の形を
したファイルと `.lock` は**消さない** — 止まっている会話の控えそのもの。)

### 4. 効いていることを確かめる

```bash
# 自送信が origin=keepalive で積まれているか
llm-gateway stats --by origin

# 知らせに連鎖が乗っているか (系列の 2 本目から出る)
curl -sSN http://127.0.0.1:11301/llm-gateway/events | grep -m3 cache_until

# 止める口が生きているか
curl -sS -X POST http://127.0.0.1:11301/llm-gateway/keepalive/pause \
  -H 'content-type: application/json' -d '{"session_id":"<id>"}'
```

`cache_keepalive` / `keepalive_paused` の 1 通はもう流れない。`resume` と
`paused` の口も無くなっている (止めた会話は、実リクエストが 1 本来れば自動で
戻る)。

## 残り

ccmsg 側の `cache_keepalive` 受け口と、nonce を 1 行で返させる常時ロードの
ルール (`llm-gateway-cache-keepalive`) は別 issue で落とす (DR-0027 決定 7)。
gateway が合図を出さなくなっただけなので、残っていても害は無い。
