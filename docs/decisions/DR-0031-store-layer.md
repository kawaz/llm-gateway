# DR-0031: 永続化の器を一貫性の意味論で 4 つの trait に切り、file backend をその 1 実装にする

- Status: Accepted (kawaz 裁定 2026-09-24、未実装。gateway-core 分割の段 1 で (1) から切る)
- Date: 2026-09-24

## Context

unit 間の連携は今、全部「共有ファイル + flock」で実装されている。複数ホストでの HA (Raft 等で全体が同じ状態を持つ構成) に進むには、この器自体を差し替えられる必要がある。また DR-0030 §5 は静的 secret の置き場を「Store 層の interface を切って設定でプラガブル」と裁定しており、その interface の正本が要る。

今の器は 4 種類あり、それぞれ求める一貫性が違う:

| 器 | 実装 | 何を保証しているか |
|---|---|---|
| credential | `credential/file.rs` (`FileStore`) | `.lock` を flock で掴み、最新を読み直してから refresh して書く。版 (更新時刻のナノ秒) で他者の書き込みを検知する (DR-0010) |
| keepalive の控えと発火 | `cache/keepalive/store.rs` | 系列ごとの `.lock` を非ブロッキングで掴めた 1 台だけが送る。掴めなければ待たずに引く (DR-0027) |
| 日次集計 | `stats.rs` | writer ごとのファイルに自分の分だけ書き、閲覧時に `merge` で合算する (DR-0011 / DR-0029) |
| usage スナップショット | `persist.rs` + `quota.rs` | writer ごとのファイル (`usage-latest.<writer>.json`) に一時ファイル経由の原子的な rename で書き、起動時に自分の分だけを読み戻す。他の writer の分は読まず、時刻の比較も無い (LWW ではない) |

締め出しと upstream status (DR-0021) はメモリのみで、永続化していない。

これらを「ファイルを読む / 書く」という形で抽象化すると、backend を替えた時に各器が暗黙に頼っていた保証 (flock の排他、writer 別ファイルによる書き込み競合の不在) が抜け落ちる。**抽象化の単位は、器が必要とする一貫性の意味論に置く。** そうすれば backend が何であっても、各 trait の契約を満たせば器の側は変わらない。

## Decision

### 1. trait は 4 つの意味論で切る

| # | 意味論 | 載るもの | 名前案 |
|---|---|---|---|
| 1 | 単一 writer の更新 | credential、静的 secret、`issued` の JWKS | `Persistence` (既存を位置づけ直す) |
| 2 | リース | keepalive の系列ごとの発火担当 | `LeaseStore` |
| 3 | 合算可能なカウンタ | 日次集計、日次のレート制限バケット (DR-0030 §3) | `CounterStore` |
| 4 | LWW スナップショット | usage スナップショット、締め出し | `SnapshotStore` |

名前と下の signature は**案**で、確定は gateway-core 分割の段 1 (`docs/design/gateway-core-split.md`) で行う。本 DR が固定するのは意味論と契約 (各操作が fail-closed か best-effort か、fencing token、merge 則、LWW の時刻源) で、Rust の型の細部は縛らない。

データの形 (Graph / Blob / Secret) では切らない。同じ「秘密」でも credential の refresh と JWKS の読み出しは同じ単一 writer の更新であり、形で切ると同じ意味論の実装が複数に割れる (Alternatives)。

### 2. 各 trait の契約

**fail-closed** は「失敗したら操作全体を失敗として呼び出し元に返し、推測した値で進めない」、**best-effort** は「失敗しても警告を残して進み、値の欠落や遅れを許す」を指す。

#### (1) 単一 writer の更新 — `Persistence`

「掴む → 最新を読む → 更新して書く」を 1 単位として、同じ鍵を同時に書き換えるのは 1 者に限る。

```rust
trait Persistence {
    type Value;
    type Guard<'a>: Locked<Value = Self::Value>;  // drop で手放す
    fn lock(&self, id) -> Result<Self::Guard<'_>>; // 取れるまで待つ
    fn load(&self, id) -> Result<Self::Value>;     // 権利なしの読み出し
    fn version(&self, id) -> Result<Option<Version>>;
    fn list(&self) -> Result<Vec<Id>>;
}
trait Locked {                // 権利を持つ間だけ書ける
    type Value;
    fn reload(&self) -> Result<Self::Value>;
    fn store(&self, value: &Self::Value) -> Result<()>;
}
```

- **順序の契約**: 書き換えは「`lock` で権利を取る → その権利の下で最新を読み直す → 書く → 権利を手放す」の 1 単位で行う。読み直しは権利を取った**後**にする (取る前に読んだ内容を土台にすると、待っている間に相手が書いたものを消す。DR-0010 のロック区間)。上の案は書き込みを guard のメソッドにして、権利なしでは書けない形にしている
- `lock` / `store`: **fail-closed**。refresh は失敗したら古い token を使い続けるのでなく失敗を返す (二重 refresh で refresh token を焼く事故を防ぐ)
- `load`: **fail-closed**。静的 secret と JWKS も、読めなければ認証を通さない
- `version`: 版を持たない置き場は `None` を返してよい (DR-0010、DR-0022)。`None` 同士は「変わっていない」とみなし、控えをそのまま使う (DR-0010 の意味論のまま)。版の取得に失敗した時も版なしと区別せず同じ扱いにする
- 値の型は関連型 (または型パラメータ) にして、OAuth credential と静的 secret と JWKS が同じ backend に乗るようにする。静的 secret と JWKS は refresh しないので、読み手は `load` + `version` だけを使う。流用するのは DR-0010 の「版が変わったら控えを読み直す」機構だけ

新しい trait は並べず、既存の `credential::Persistence` を guard 付きに拡張して、段 1 で core へ移すときにこれを (1) として位置づける (`gateway-core-split.md` §5)。

#### (2) リース — `LeaseStore`

鍵 (keepalive の系列) ごとに、期限付きの担当を 1 者だけに与える。

```rust
trait LeaseStore {
    fn try_acquire(&self, key, ttl) -> Result<Option<Lease>>;  // 待たない
    fn renew(&self, lease: &Lease, ttl) -> Result<Option<Lease>>;
    fn put_fenced(&self, lease: &Lease, value) -> Result<Fenced>; // 古い token なら拒む
}
struct Lease { key, fencing_token: u64, expires_at }
enum Fenced { Written, Stale }
```

- `try_acquire`: 他者が持っていれば**待たずに `None`**。待って取れた頃には相手が送り終えている (DR-0027)。backend の障害は **fail-closed** 側に倒し、取れなかったものとして扱う (送らない)。二重に送るより一度送り損ねる方が安い
- **fencing token**: 担当が替わるたびに単調に上がる番号を `Lease` に持たせる。backend は token を保存し、鍵に付いた最新より古い token での書き込み (`put_fenced` による控えの保存、`renew`) を拒む。期限切れに気づかず動き続けた元担当が store を書き戻す事故を、期限の判定でなく token の比較で止める
- **fencing token が守るのは store への書き込みだけ**で、外部 (upstream) への送信そのものは fence できない。upstream は token を知らないので、古い担当が送ってしまえば止める手段が無い。送信の二重は今の keepalive と同じく、担当の取得 (claim) と、控えを読み直して `fires_at_ms` が自分の予定より先へ進んでいれば送らない検出 (他の担当が既に送った印) で抑える。この 2 つは書き分け、token で送信まで守れるとは扱わない
- 期限の判定に使う時刻は backend の時刻

#### (3) 合算可能なカウンタ — `CounterStore`

writer ごとに自分の分だけを書き、読む時に全 writer 分を merge する。writer 間で同じ値を書き換えない。

```rust
trait CounterStore<C: Mergeable> {
    fn write_own(&self, writer, bucket, value: &C) -> Result<()>;
    fn read_merged(&self, bucket) -> Result<Merged<C>>;  // Merged { value, missing }
}
```

- merge 則: **可換・結合的で、単位元を持つ** (加算、最大値など)。同じ writer の分は上書き (自分の最新の累計を書く) で、writer 間は merge。これで writer の数や読む順序に依らず同じ合計になる
- `read_merged` は合算値と**欠けた writer の一覧**を返す (例: `Merged { value: C, missing: Vec<Writer> }`)。`Result<C>` だけでは欠けを運べないため
- 用途ごとの倒し方 (writer 別に書いて読む時に合算する形は DR-0030 §3 のとおり保つ):

| 用途 | `write_own` | `read_merged` |
|---|---|---|
| stats (閲覧) | **best-effort**。書けなければ次の flush で書き直し、転送は止めない | **best-effort**。`missing` は無視してよい |
| 日次のレート制限バケット | **fail-closed**。書けなければその request を通さない (書き損ねた分だけ枠を超えるため) | **fail-closed**。`missing` が空でなければ判定できないとして通さない |

#### (4) LWW スナップショット — `SnapshotStore`

鍵ごとに 1 つの値を持ち、新しい方が勝つ。

```rust
trait SnapshotStore<T> {
    fn put(&self, writer, key, value: &T, observed_at) -> Result<()>;  // 古ければ捨てる
    fn get(&self, key) -> Result<Option<(T, observed_at)>>;
}
```

これは**目標の意味論**で、今の usage スナップショットはまだ LWW ではない (Context の表)。LWW は trait の契約として固定するが、file backend の実装は分割の段 1 では今のプロセス別スナップショット (自分の分だけ読み戻す) のままで、挙動は変えない。writer を横断して `observed_at` で比べる読み出しは分割の範囲外の後続 (2 つ目の backend を入れる時か、その前の独立した変更) で入れる (`gateway-core-split.md` §4)。

- **writer の識別子を引数に持つ**。どの writer の観測かを backend が知らないと、同じ鍵への複数 writer の書き込みを区別できない
- **時刻源は値に付いた `observed_at`** (gateway が上流の応答を観測した時刻) で、書き込んだ時刻ではない。遅れて届いた古い観測が新しい観測を上書きしないため
- `put` / `get`: **best-effort**。usage は次の応答で上書きされ、締め出しは失えば一度上流に聞き直すだけで済む

### 3. file backend は今の実装を各 trait に収める

挙動は変えない。

| trait | file backend | fencing token / 時刻源 |
|---|---|---|
| (1) `Persistence` | `FileStore` そのまま (`.lock` の flock、版は更新時刻のナノ秒) | — |
| (2) `LeaseStore` | keepalive の `.lock` を非ブロッキング flock で掴む。排他が効くのは `Claim` guard が生きている間だけで、プロセスの寿命とは一致しない (guard を落とせば同じプロセスの中でも外れる) | file backend でも token を保存し、控えの保存時に比較する。guard を落とした後の元担当が控えを書き戻す窓は単一ホストでも生じる |
| (3) `CounterStore` | `stats.rs` の writer 別ファイル + 閲覧時の `merge` | — |
| (4) `SnapshotStore` | `persist.rs` の原子的 rename + writer 別ファイル。LWW にするには他の writer の分を読んで `observed_at` で比べる処理を足す | 時刻源は `observed_at` |

締め出しと upstream status は今メモリのみなので、(4) に載せるかは別の判断 (再起動を跨ぐ必要が出た時)。本 DR は「載せるなら (4)」とだけ決める。

### 4. backend は設定で選ぶ

設定に `store = "file"` (既定) を置き、core が backend を選ぶ口を enum で持つ。第一版は `File` の 1 variant だけ。`op://` 解決や cache-warden は variant を足して入れる。trait ごとに別の backend を選べるようにするかは、2 つ目の backend を入れる時に決める。

### 5. 導入の順序

`gateway-core-split.md` の段 1 / 段 3 で (1) を先に形にする。(3) は段 2 の `Stats<C: Mergeable>` が器になるが、backend 差し替えの trait としては (1) の流儀が固まってから切る。(2) と (4) は分割の範囲外で、2 つ目の backend を入れる時に切る。

## Alternatives Considered

- 案 A: データの形 (Graph / Blob / Secret) で切る
  - 不採用理由: 同じ形でも求める一貫性が違い (credential の refresh と usage はどちらも小さな JSON だが、前者は単一 writer、後者は LWW)、逆に違う形でも同じ意味論のもの (credential と JWKS) が別 trait に割れる。backend を替えた時に保証すべきものが trait に現れない
- 案 B: ORM 的な汎用 KV (`get` / `put` / `cas` だけ)
  - 不採用理由: 排他・リース・merge を各器が KV の上で組み立てることになり、今 flock が暗黙に与えている保証を器ごとに再実装する。backend 側の最適化 (Raft で (3)(4) を log に載せない) も表現できない
- 案 C: 別プロセスの store (gateway の外に状態サーバを立てる)
  - 不採用理由: 状態の所有者が gateway の外に割れる。credential の更新は gateway の中の refresh と一体で、別プロセスに store を持たせると「更新は 1 箇所」(DR-0010 の flock が守る前提) が gateway と store の 2 者に分かれる。DR-0030 §1 がプロセスを分けない理由と同じ論拠。外部の store は Store 層の backend として足す (cache-warden はその形で入る) のであって、gateway の状態の所有者を外へ移すのではない

## Consequences

- **使わない抽象を先に切るコスト (YAGNI)**: 今 backend は file しか無い。ただし切る位置を意味論に置くと、file 実装の側でも「どの器がどの保証に頼っているか」が型に現れ、見通しが良くなる。HA に進まなくても損にならないと判断する。その上で、実際に trait を切るのは (1) から順に、使う時点まで遅らせる (§5)
- **Raft backend を入れる時は DR-0027 を supersede する**: DR-0027 の「分散 backend は作らない」とは、Store 層を切ること自体は衝突しない。Raft backend は (1)(2) だけを log に載せ、(3)(4) は複製で済む見立て (keepalive の本文のような 2 MB 級の控えは log に載せない)
- **fencing token は file backend でも保存して比較する**: flock の排他は `Claim` guard の存命中だけなので、guard を落とした元担当が控えを書き戻す窓は単一ホストでも生じる。今の keepalive の `save` は `Claim` も token も要求していないので、`save` に Claim / token を要求する変更は、リースを trait として切る後続の段で行う (`gateway-core-split.md` §4 でリースは分割の範囲外)。それまでの間、控えの `save` は今のとおり Claim を要求せず、二重送出の抑止は claim と `fires_at_ms` の検出に依る
- **送信の二重は token では防げない**: upstream への送信は今の claim と `fires_at_ms` の検出のまま。複数ホストの backend でもこの検出は要る
- **fail-closed の操作が増える backend では可用性が下がる**: Raft 等で quorum を失うと (1)(2) が止まる。これは契約通り (refresh と発火は止まるのが正しい) だが、転送自体は (1) の読み出しが通る限り続くことを backend 側で保つ必要がある
- 静的 secret と JWKS は (1) に載るので、cache-warden への移行は backend の variant 追加で済み、読む側は変えない (DR-0030 §5)

## 未確定

- Raft backend の具体 (openraft 等の選定、(1)(2) の log 設計、(3)(4) の複製方式)
- cache-warden の IF (どの trait をどの API で満たすか。cache-warden 側の仕様が固まってから)
- fencing token の永続化形式 (file backend で値をどこに持つか — `.lock` ファイルの中身か、控えの JSON のフィールドか)
- trait ごとに backend を分けて選べるようにするか (§4)
- 締め出しと upstream status を再起動を跨いで持つか (§3)
- `SnapshotStore` で同じ `observed_at` が衝突した時の決め方 (候補: writer 名の順で決め、全ノードで同じ方が勝つようにする)

## 関連

- issue `2026-09-15-store-layer-for-replaceable-persistence` — 本 DR の元
- `docs/design/gateway-core-split.md` §5 — 段 1 で `Persistence` を (1) として core へ移す
- `docs/research/2026-09-23-harnessrouter-gateway.md` §2.1 — fail-closed / best-effort の契約と fencing token の出所
- [DR-0010](DR-0010-credential-cross-process-lock.md) — (1) の file 実装の版と排他
- [DR-0011](DR-0011-daily-usage-stats.md) / [DR-0029](DR-0029-stats-origin-axis.md) — (3) の日次集計
- [DR-0021](DR-0021-upstream-service-status.md) — upstream status (メモリのみ)
- [DR-0022](DR-0022-credential-update-triggers-discovery.md) — 版を持たない置き場は `None` を返してよい
- [DR-0027](DR-0027-keepalive-by-replay.md) — (2) のリースの現状、Raft 導入時に supersede
- [DR-0030](DR-0030-general-purpose-auth-gateway.md) §3 / §5 — 日次バケットは (3)、静的 secret と JWKS は (1)
