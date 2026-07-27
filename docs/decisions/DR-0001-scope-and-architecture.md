# DR-0001: スコープとアーキテクチャ

- Status: Active (一部を [DR-0002](./DR-0002-component-architecture.md) が改訂)
- Date: 2026-07-27

> **改訂あり**: 「ボディは触らない」「ヘッダを足さないので事故は起きない」の 2 点は
> 実測により覆った。ボディの `model` 書き換えと、Bedrock 向けの `anthropic-beta`
> 除去が要る。また OpenAI 系プロバイダ対応が要求に追加された。詳細は DR-0002。

## Context

CLIProxyAPI (7.2.100, 54MB, 4 プロバイダ対応) を、kawaz の用途に必要な機能だけに
絞った自作 proxy に置き換える。判断の経緯と代替案の比較は
**`kawaz/llm-notes` の DR-0002** が正本。本 DR は**実装側のスコープ確定**を担う。

kawaz の要求 (原文):

> claude クライアントは認証のことを考えたくない。APIBASEURL だけ設定したら
> 動いて欲しい。でサブスク認証ヘッダやらベッドロックのキーやらは勝手に解決して
> 上手いことルーティングして欲しい。それだけ。

## Decision

### 実装する (v1 スコープ)

| # | 機能 | 詳細 |
|---|---|---|
| 1 | OAuth token リフレッシュ | `POST https://api.anthropic.com/v1/oauth/token`、`grant_type=refresh_token`、`client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e`。token 寿命 8 時間 |
| 2 | session → auth 固定 | session キーで同じ auth に貼り続ける。キーの導出はボディの `metadata.user_id` が最優先 (DR-0002 で仕様を確定)。取れなければリクエスト内容のハッシュ |
| 3 | モデル名ルーティング | モデル名 → upstream の**優先順位リスト**。上から試し、経路断なら次へ |

### 実装しない (v1 では持たない)

- **Claude Code 偽装** — cloak / beta フラグ注入 / device profile / UA 詐称。
  素通しで 200 が返ることを実測済み。**cpa ではこれが実障害の原因になった**
- **429 検知 → cooldown → 分散** — 2 日分のログ (218,537 行) で発生 0 件。
  「レート制限による分散」と「経路断のフォールバック」は別物で、後者だけ持つ
- **config の書き戻し / 管理 GUI / plugin 機構**
- **gemini / vertex / qwen 等のプロバイダ** — 使っていない
  (OpenAI 系は要求に追加された。DR-0002 の Phase 2)

### アーキテクチャ

```
                  ┌─────────────────────────────┐
  client ────────▶│ llm-gateway                 │
  (ANTHROPIC_     │                             │
   BASE_URL)      │  Router                     │
                  │   └ model名 → 優先順位リスト  │
                  │                             │
                  │  AuthProvider (trait)       │
                  │   ├ OAuthPool  (token更新)   │──▶ api.anthropic.com
                  │   └ BedrockKey (静的キー)     │──▶ bedrock-mantle.*.api.aws
                  │                             │
                  │  SecretStore (trait)        │
                  │   ├ PlainFile   ← v1        │
                  │   └ CacheWarden ← 将来       │
                  └─────────────────────────────┘
```

**ボディは `model` フィールドのみ書き換える** (DR-0002 で改訂)。
ヘッダは認証情報の生成に加え、Bedrock 向けに `anthropic-beta` の拒否フラグを除去する。

### SecretStore はプラガブルにする

```rust
trait SecretStore {
    fn get(&self, key: &str) -> Result<Secret>;
    fn set(&self, key: &str, value: Secret) -> Result<()>;
}
```

trait の形は DR-0002 で改訂した (`get`/`set` では refresh_token ローテートの
競合制御が呼び出し側に漏れるため、`acquire` + `Persistence` の 2 層にする)。

v1 の永続化は `PlainFile` (cpa 互換 JSON、平文)。kawaz 裁定 2026-07-27:

> cache-warden 側のその辺のサポートが現在まだ計画段階なので、get/set も
> 永続化問題がまだ解決してないので、そこに関してはプラガブルな設計にして、
> 現時点ではそれぞれ全て平文ファイルでも良いです。

cache-warden の暗号化永続 (Passkey PRF 検討中) が固まったら `CacheWarden`
バックエンドを足して差し替える。**proxy 側の変更は最小で済む形にしておく。**

### ルーティングは優先順位で表現する

cpa の `oauth-excluded-models` は「出す / 出さない」の二値しか表現できず、
その結果 fable-5 が単一障害点になった (実障害: Bedrock 側が落ちて 15 分 503)。

v1 では優先順位を持つ:

```
claude-fable-5:
  1. bedrock       # 通常はここ (Claude アカウントを消費しない)
  2. oauth-pool    # bedrock が落ちた時のみ
```

「Claude アカウントを消費しない」は**通常時の目的**であって、
Bedrock 障害時まで止まる理由にはならない。

## Alternatives Considered

- **案 A: cpa を設定で薄くして使い続ける**
  - 不採用理由: beta 注入は `disable-claude-cloak-mode` でも `cloak.mode: never`
    でも止まらないことを実測。config 書き戻しも無効化できない
- **案 B: cpa に PR を出す**
  - 不採用理由: 厚みが設計方針そのものなので削る PR は通りにくい。
    54MB / 4 プロバイダの保守に付き合うことになる
- **案 C: v1 から cache-warden 一本に決め打つ**
  - 不採用理由: cache-warden の永続化が未解決で、待つと proxy が作れない。
    trait を挟めば後から差し替えられる
- **案 D: v1 から 429 failover も入れる**
  - 不採用理由: 実測で発生 0 件。使わない機構を最初から持つのは cpa と同じ道

## Consequences

- **Anthropic 経路は upstream 仕様変更に強い**。偽装しないので Claude Code の
  バージョン追従が不要 (ただし ChatGPT 経路は偽装が要る。DR-0002)
- **cpa の管理 GUI が失われる**。auth の追加・確認は CLI サブコマンドが要る
- **429 が起きても何もしない**。手動でモデル/アカウントを変える運用。
  実際に困ってから足す
- **v1 は秘密が平文でディスクに残る**。cpa と同水準であって改善ではない。
  「ファイルに書かない」は `CacheWarden` 移行後の話
- **token リフレッシュの実装ミスは全アカウント再ログインを招く**。
  リフレッシュ前に auth JSON をバックアップする安全策を入れる
- auth JSON は **cpa 互換形式を保つ** (`access_token` / `refresh_token` /
  `expired` / `email` / `type` / `priority` / `disabled`)。
  併存させて段階的に切り替えられる

## 実測で確定した前提

詳細は `kawaz/llm-notes` の findings が正本。

| 検証 | 結果 |
|---|---|
| サブスク token が偽装なしで通るか | **200** |
| `anthropic-beta` の要否 | **不要** (有無に関わらず 200) |
| OAuth リフレッシュ経路 | `POST /v1/oauth/token` (標準的) |
| token 寿命 | 8 時間 |
| session キー | ボディ `metadata.user_id` 由来の `claude:<uuid>` (DR-0002 で確定) |
| 実 failover の発生 | **2 日で 0 件** |
| Bedrock が拒否する beta | 10 中 **5** (`oauth-2025-04-20` 他) |

## 関連

- `kawaz/llm-notes` DR-0002 — 自作を決めた判断 (代替案の比較が正本)
- `kawaz/llm-notes` findings/2026-07-27-thin-proxy-poc.md — 成立条件の実測
- `kawaz/llm-notes` DR-0001 — fable-5 の Bedrock 専用化 (この gateway が引き継ぐ)
