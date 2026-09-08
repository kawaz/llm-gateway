# DR-0026: discovery が codex backend に名乗る版は定数で持つ

- Status: Accepted
- Date: 2026-09-08

## 文脈

ChatGPT サブスクの Codex backend は `GET /backend-api/codex/models` を
`client_version` 付きで受け、各モデルの `minimal_client_version` と突き合わせて
**古すぎる相手には空の一覧を返す**。

gateway には この口を叩く経路が 2 つある:

- **クライアント中継** (`/models` の中継、DR-0025 §7): 聞きに来た codex CLI が
  クエリに載せてきた版をそのまま渡す。ここは元から正しい
- **定期 refresh** (`discovery::fetch`、DR-0022): 誰も聞きに来ていないので名乗る版を
  自分で決める必要がある。ここが `env!("CARGO_PKG_VERSION")` を渡していた

gateway の版 (0.43.0) は `minimal_client_version` (gpt-6-astra で 0.153.0) に遠く
届かないため、**codex 経路の catalog は常に空**だった。空だった catalog は config の
`models` 宣言へのフォールバック (`router::with_declared_fallback`) に拾われ、
一覧は出ていたので気づきにくかった。config のコメントにあった
「空配列を返すアカウントがある」という記述は、この誤診である。

実測 2026-09-07 (同一 credential): `client_version=0.43.0` → 0 件、`0.153.4` → 9 件
(gpt-6-astra / gpt-reserve / gpt-5.6-sol / terra / luna / gpt-5.5 / gpt-5.4-mini /
gpt-5.3-codex-spark / codex-auto-review)。

## 決定

### 1. 定期 refresh で名乗る版は、codex CLI の版をコード定数で持つ

`discovery::CODEX_CLIENT_VERSION` に codex CLI の版 (現在 `0.153.4`) を置き、
`fetch()` はこれを渡す。gateway の版を名乗らない。

これは **単価表 (`preset/pricing.rs`) と同じ運用**。upstream 側の事実を写した定数で、
リリース時に手で更新する。

**リリース時の更新手順**: `codex --version` の出力に合わせて
`crates/llm-gateway/src/discovery.rs` の `CODEX_CLIENT_VERSION` を更新する。

### 2. catalog が取れた経路では catalog が勝つ。公開一覧の調整は `exclude` で行う

`with_declared_fallback` の現状維持。config の `models` は
**一覧が空だったときのフォールバック専用**であり、catalog を絞る手段ではない。

修正の結果 catalog が非空になり、`gpt-reserve` / `codex-auto-review` のような
出したくないモデルも一覧に載る。これは既存の `[ns.<名>.filter] exclude` /
`[routes.<名>] exclude` (`config::Namespace::allows`) で隠す。

**なぜ `models` を絞り込みに使わないか**: `models` は「一覧が取れないときに何を
公開するか」の宣言であって、「一覧のうち何を見せるか」の宣言ではない。1 つの鍵に
2 つの意味を持たせると、一覧が取れる / 取れないで挙動が変わる設定になる。
絞り込みには既に `exclude` という専用の手段がある。

## 却下した案

- **gateway 自身の版を名乗り続ける**: upstream が見ているのは「この一覧を使う
  クライアントが新しい機能を扱えるか」であり、gateway の版はその問いに答えていない。
  常に 0 件になる
- **config で上書き可能にする**: 上書きしたくなる場面が今のところ無い。設定項目は
  一度生やすと消せない (DR-0013 に配列の削除手段が無いのと同じ性質) ので、
  要求が出てから足す
- **直近に codex クライアントから受けた `User-Agent` / `client_version` を覚えて使う**:
  refresh が「誰かが最近来たか」に依存し、起動直後や codex を使わない期間に
  catalog が空へ戻る。定期 refresh は外部の来訪と独立に動くべき

## 影響

- codex 経路の catalog が非空になる。`/v1/models` の公開一覧が変わるので、
  出したくないモデルは `exclude` に足す
- catalog に載ったモデルのうち `gpt-reserve` / `gpt-5.5` / `gpt-5.4-mini` は
  単価表に無く、gap warning が出る。単価を足すか `exclude` するかは運用の判断
