# DR-0006: 既定 namespace を特別扱いせず、`/v1` を `/ns-default` へ内部ルーティングする

- Status: Active
- Date: 2026-07-29

## Context

`/v1/messages` のように namespace を書かないパスは、いま `Config` の
`#[serde(flatten)] default_namespace: Namespace` に解決される。つまり **config の
トップレベルがそのまま既定 namespace の定義**になっている。

この形が 2 つの問題を生んでいた。

### 認証をかけ忘れても誰も気づかない

`Namespace::accepts()` は `auth_token` が未設定なら無条件で通す。

> 設定に書いていなければ誰でも通す。127.0.0.1 で待ち受けている前提で、
> 同じマシンの他プロセスと区別したいときだけ書く。

この前提は前段に Caddy を置いた時点で崩れた。名前付き namespace
(`ns.personal` / `ns.emrd`) には `auth_token` を書いたが、トップレベルには
書いていなかったため、**`/v1/messages` が認証なしで外から叩ける**状態になっていた。

canddy 側のセッションが実機で確認済み (2026-07-28)。Authorization ヘッダを
付けずに POST して 200 が返り、課金が発生した。到達元は tailnet で、
参加デバイスには公開 IP を持つクラウドホストが複数含まれる。

「書かなければ通す」という fail-open が、公開経路の追加という**設定ファイルの外**で
起きた変化によって穴に変わった。設定を 1 行足せば塞がるが、次に namespace を
足す人が同じ穴を開けられる構造は残る。

### 設定が二重管理になる

トップレベルに `[filter]` `[[routing]]` `[aliases]` があり、`[ns.personal]` にも
同じ種類の設定が並ぶ。単一用途なら namespace を意識せずに済むという当初の狙いは、
namespace を使い始めた時点で「同じことを 2 箇所に書く」に変わっていた。

## Decision

**既定 namespace を特別扱いしない。** `/v1/...` は `/ns-default/...` へ内部ルーティング
されるものとして扱い、`default` も名前付き namespace の 1 つにする
(kawaz 裁定 2026-07-28: 「トップのヘルスチェックとか以外は自動で /ns-default 内部
ルーティングされるで良いでしょう。その方が色々整理がしやすい」)。

トップレベルの `#[serde(flatten)] default_namespace` は廃止し、`[ns.default]` として
他と同じ書式で書く。

### `ns.default` の書き方は 3 通りから選べる

| 書き方 | 用途 |
|---|---|
| 直接そこに設定を書く | 単一用途。今までのトップレベル設定と同じ内容を `[ns.default]` に置く |
| 別の namespace へ委譲する | `/v1` を `/ns-personal` と同じ扱いにしたいとき |
| 全部拒む (deny-all) | 名前を明示したリクエストしか受けないとき |

### namespace を持たない口

`/healthz` は namespace の外に置く。前段のロードバランサが数秒ごとに叩く死活監視で、
認証も credential も要らない。業務のエンドポイントを死活監視に使うと、
そのエンドポイントの認証方針が監視側に縛られる (実際 `/v1/models` を監視に使っていた
ために、既定 namespace に認証をかけると監視が 401 で落ちて全断する状態になっていた)。

namespace に属さない口は今後もこの基準で判断する。**業務のデータに触れず、
認証を要求する理由がないものだけ**を namespace の外に置く。

## 検討が要る点

- **委譲の循環**。`ns.default` → `ns.personal` → `ns.default` を防ぐ必要がある。
  alias の循環検出 (`router.rs` の「エイリアスが循環しています」) と同じ問題なので、
  同じ扱いにできるか確認する
- **deny-all の表現**。`auth_token` を必須にするのとは別の概念 (トークンを持っていても
  通さない)。既存の `filter` で表せるか、新しい書き方が要るかを決める
- **fail-open をやめるか**。本 DR は既定 namespace の特別扱いを外すが、
  「`auth_token` 未設定の namespace は誰でも通す」という `accepts()` の挙動自体は
  変えていない。名前付き namespace で書き忘れれば同じ穴が開く。これを
  fail-closed にするかは別途裁定が要る

## 移行

既存の config はトップレベルに設定を持っているので、そのままでは読めなくなる。
`[ns.default]` へ移す作業が要る。稼働中の 3 プロセス (8401 / 8402 / 旧 8317) が
同じ形式を読むので、config の入れ替えとプロセスの再起動をまとめて行う。

## 関連

- [DR-0002](./DR-0002-component-architecture.md) — namespace を含むコンポーネント構成
- `docs/QUESTIONS.md` の AUTH-Q1 — 認証の穴を塞ぐ順序 (本 DR は恒久対処、
  AUTH-Q1 は今開いている穴への即時対応)
