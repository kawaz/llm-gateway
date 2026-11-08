//! 系列ごとに「最後に転送した 1 本」を置く (DR-0027 決定 3)。
//!
//! **ここに載るのは会話の本文そのもの**である (DR-0027 決定 4)。同意フラグも
//! 暗号化もマスキングも持たない — 同じホストにはセッションの transcript が
//! 丸ごと置いてあり、一時ダンプ (DR-0038) も本文を書き出すので、ここだけに保護の
//! 儀式を足しても守られるものは増えない。置き場を配る・共有するときは、
//! 会話の中身がそのまま入っている前提で扱う。
//!
//! 置き場は待ち受けごとに分けない。11301 と 11302 は**同じファイルを共有**し、
//! 脇の `.lock` を掴んだ 1 台だけが撫でる (DR-0010 と同型)。送るのは gateway が
//! 自分で出す 1 本なので、ファイルの排他がそのまま重複防止になる。
//!
//! 書き方は他の置き場と同じ流儀 — 一時ファイルへ書いてから rename する
//! ([`write_atomically`], DR-0011)。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::egress::RequestShape;
use crate::persist::write_atomically;

/// 1 系列に持たせてよい本文の大きさ (バイト)。
///
/// 30K トークンのプレフィックスで 200KB 前後、道具と system を厚く積んだ
/// 会話でも 1MB を大きくは超えない。8MB はその 1 桁上に置いた蓋で、ここを
/// 超える系列は**保持しない** = 延命しない。置き場全体に上限を掛けないのは、
/// 系列の数は会話の数で頭打ちになり、1 本ずつの蓋があれば総量も抑えられる
/// ため (DR-0027「未確定」への回答)。
pub const BODY_LIMIT: usize = 8 * 1024 * 1024;

/// 系列 1 つ分の控え。時刻は全て Unix ミリ秒 (単調時計は保存できない)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub session_id: String,
    pub prefix: String,
    pub ns: String,
    /// 解決後の実モデル名 (クライアントが名乗った短い名前ではない)。
    pub model: String,
    /// 直前の実リクエストが通った経路の名前。
    pub route: String,
    /// 送出直前の本文。認証の差し替えとモデル名の書き換えを済ませた形。
    pub body: Value,
    /// 送出直前のヘッダ。**認証は入っていない** — 認証は経路が送るときに
    /// 付け直すので、控えに持つと古い token を持ち回ることになる。
    pub headers: Vec<(String, String)>,
    pub path: String,
    pub query: Option<String>,
    pub shape: RequestShape,
    /// 次に送り直す予定の時刻。
    pub fires_at_ms: i64,
    /// この系列の cache が消える時刻。過ぎていれば送っても繋ぐものが無い。
    pub expires_at_ms: i64,
    /// 送り直し続ける期間の終わり。延ばせるのは実リクエストだけ。
    pub horizon_end_ms: i64,
    /// この連鎖の起点 = 最後に来た実リクエストを送った時刻。
    pub since_ms: i64,
    /// ここまでに送り直した本数。
    #[serde(default)]
    pub count: u32,
    /// この系列について最後に約束した寿命の id (`cache_notice`、DR-0012)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_notice: Option<String>,
}

/// 控えのうち、本文 (`body` / `headers`) を除いた欄。会話の状態を問われた
/// ときに読む形 (DR-0042)。
///
/// 欄の意味は [`Kept`] と同じ。本文は読み飛ばす — `serde` は知らない欄を
/// 値の木に組まずに捨てるので、8MB まである本文を答えのたびに組み立てずに
/// 済む。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct KeptMeta {
    pub session_id: String,
    pub prefix: String,
    pub ns: String,
    pub model: String,
    pub route: String,
    pub fires_at_ms: i64,
    pub expires_at_ms: i64,
    pub horizon_end_ms: i64,
    pub since_ms: i64,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub cache_notice: Option<String>,
}

/// 系列を指す鍵。ファイル名にもなる。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Series {
    pub session_id: String,
    pub prefix: String,
}

impl Series {
    /// ファイル名 (拡張子なし)。形は `<潰した会話の id>-<会話の id のハッシュ>.<prefix>`。
    ///
    /// 会話の id は client が名乗るものなので、区切りやパスに使える文字が
    /// 混ざりうる。英数字以外を `-` に潰して名前に使える形にし、潰すと違う
    /// 会話が同じ形に落ちる (`a b` と `a/b`) ので、**潰す前の id のハッシュ**を
    /// 足して会話ごとに別の名前にする。prefix はこちらが本文から作る 16 進
    /// なのでそのまま使う。
    pub(super) fn stem(&self) -> String {
        format!("{}.{}", session_stem(&self.session_id), self.prefix)
    }
}

/// 会話の id から取るハッシュの桁数 (16 進)。
const SESSION_HASH_DIGITS: usize = 8;

/// ファイル名に残す、会話の id を潰した部分の長さの上限 (バイト)。
const SESSION_READABLE_MAX: usize = 64;

/// ファイル名のうち会話を表す部分 ([`Series::stem`] の `.` より前)。
///
/// 潰した id は人が置き場を眺めて会話を見分けるためのもので、会話ごとに
/// 別の名前になることはハッシュの側が受け持つ。
///
/// Design rationale: 潰さずに percent-encode する案 (`a/b` → `a%2Fb`) は
/// 名前が id と 1 対 1 になるが、名前の長さが client の名乗る id の長さで
/// 決まり (区切り 1 文字が 3 文字になる)、ファイル名の上限 (255 バイト) を
/// 超えた会話は控えを置けず黙って keepalive から外れる。ハッシュを足す形なら
/// 見分けをハッシュが持つので読める部分を切り詰められ、名前の長さが id に
/// 依らず抑えられる。ハッシュは prefix と同じ SHA-256 の先頭で、8 桁
/// (32 bit) あれば足りる — 衝突が効くのは潰した部分まで同じになった会話
/// どうしだけで、そういう組は数個しか並ばない。万一衝突しても、読み出し側は
/// 中の `session_id` の完全一致で確かめる ([`Store::load_meta_of`])。
fn session_stem(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // 潰した後は ASCII だけなので、バイト数で切っても文字の途中にならない。
    let readable = cleaned.trim_matches('-');
    let readable = readable[..readable.len().min(SESSION_READABLE_MAX)].trim_end_matches('-');
    let readable = if readable.is_empty() {
        "unknown"
    } else {
        readable
    };
    format!("{readable}-{}", session_hash(session_id))
}

/// 会話の id の SHA-256 の先頭 [`SESSION_HASH_DIGITS`] 桁。
fn session_hash(session_id: &str) -> String {
    use sha2::{Digest as _, Sha256};

    Sha256::digest(session_id.as_bytes())
        .iter()
        .take(SESSION_HASH_DIGITS.div_ceil(2))
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// この名前は自分が置いたものか ([`Series::stem`] の形をしているか)。
///
/// 置き場は他の書き手も使う。拾うのは `<会話>.<prefix>` の形をした名前だけで、
/// それ以外は読まずに飛ばす — 読めば「壊れている」と警告することになり、
/// 自分のものでないファイルについて毎回 1 行吐く。
pub(super) fn is_ours(stem: &str) -> bool {
    let Some((session, prefix)) = stem.rsplit_once('.') else {
        return false;
    };
    !session.is_empty()
        && !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_hexdigit())
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 控えのファイル 1 つを読む。読めない・壊れているものは警告して飛ばす。
fn read_kept<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    match std::fs::read(path).map(|raw| serde_json::from_slice::<T>(&raw)) {
        Ok(Ok(kept)) => Some(kept),
        Ok(Err(e)) => {
            tracing::warn!(path = %path.display(), %e, "the kept request is unreadable; dropping it");
            None
        }
        // 一覧を取ってから読むまでの間に畳まれた系列 (期限・pause・兄弟)。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "cannot read the kept request");
            None
        }
    }
}

/// 系列の控えを置くディレクトリ 1 つ。
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

/// 掴んでいる間だけ、この系列を撫でてよい ([`Store::claim`])。
///
/// 落とすと flock が外れ、待っている兄弟が掴めるようになる。
pub struct Claim {
    _file: std::fs::File,
}

impl Store {
    /// 日次集計と同じ置き場の下、`keepalive/` に系列ごとのファイルを持つ。
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().join("keepalive"),
        }
    }

    fn path_of(&self, series: &Series) -> PathBuf {
        self.dir.join(format!("{}.json", series.stem()))
    }

    fn lock_path_of(&self, series: &Series) -> PathBuf {
        self.dir.join(format!("{}.lock", series.stem()))
    }

    /// この系列を撫でる権利を掴む。**兄弟が掴んでいれば `None`**。
    ///
    /// 待たないのは、待って掴めた頃には相手が送り終えているため — その系列は
    /// 既に延びていて、こちらが続けて送る理由が無い。
    ///
    /// `.lock` は消さない。消して作り直すと、掴んでいる側と後から来た側が別の
    /// ファイルを見ることになり、締め出しが破れる (DR-0010 と同じ理由)。
    pub fn claim(&self, series: &Series) -> Option<Claim> {
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(path = %self.dir.display(), %e, "cannot create the cache keepalive directory");
            return None;
        }
        let path = self.lock_path_of(series);
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "cannot open the cache keepalive lock");
                return None;
            }
        };
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Some(Claim { _file: file }),
            Err(rustix::io::Errno::WOULDBLOCK) => None,
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "cannot take the cache keepalive lock");
                None
            }
        }
    }

    /// この系列の控え。無ければ `None`。
    pub fn load(&self, series: &Series) -> Option<Kept> {
        let path = self.path_of(series);
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "cannot read the kept request");
                return None;
            }
        };
        match serde_json::from_slice(&raw) {
            Ok(kept) => Some(kept),
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "the kept request is unreadable; dropping it");
                None
            }
        }
    }

    /// 置いてある控えを全部読む。起動時に予定を張り直すために使う。
    pub fn load_all(&self) -> Vec<Kept> {
        self.our_files()
            .into_iter()
            .filter_map(|(path, _)| read_kept(&path))
            .collect()
    }

    /// この会話の控えを、本文を除いて読む (DR-0042)。無ければ空。
    ///
    /// 名前で候補を絞ってから、中の `session_id` が完全に一致するものだけを
    /// 残す — 名前の会話の部分はハッシュの先頭しか持たないので、確かめるのは
    /// 中身の側。
    pub fn load_meta_of(&self, session_id: &str) -> Vec<KeptMeta> {
        let wanted = session_stem(session_id);
        self.our_files()
            .into_iter()
            .filter(|(_, stem)| {
                stem.rsplit_once('.')
                    .is_some_and(|(session, _)| session == wanted)
            })
            .filter_map(|(path, _)| read_kept::<KeptMeta>(&path))
            .filter(|meta| meta.session_id == session_id)
            .collect()
    }

    /// 置いてある控えを全部、本文を除いて読む (DR-0042)。`ns` を渡すと、
    /// その面を通った系列だけを残す。
    pub fn load_meta_all(&self, ns: Option<&str>) -> Vec<KeptMeta> {
        self.our_files()
            .into_iter()
            .filter_map(|(path, _)| read_kept::<KeptMeta>(&path))
            .filter(|meta| ns.is_none_or(|ns| meta.ns == ns))
            .collect()
    }

    /// 名前が中身の系列の名前 ([`Series::stem`]) と違う控えを、その名前に
    /// 付け替える。起動時に、読み戻す前に 1 回呼ぶ。
    ///
    /// 置き場には会話のハッシュを持たない形 (`<潰した会話の id>.<prefix>`) の
    /// 控えが居ることがあり、この関数がそれを読む。名前のままでは [`Self::load`]
    /// ・[`Self::claim`]・[`Self::remove`] がその系列に届かない。付け替え先が
    /// 既にあればそちらを残し、こちらを捨てる — 付け替え先は今の名前で書く
    /// 書き手が置いたもので、同じ系列の控えを 2 つ持つと読み戻しで二重に拾う。
    pub fn settle_names(&self) {
        for (path, stem) in self.our_files() {
            let Some(meta) = read_kept::<KeptMeta>(&path) else {
                continue;
            };
            let named = Series {
                session_id: meta.session_id,
                prefix: meta.prefix,
            }
            .stem();
            if stem == named {
                continue;
            }
            let target = self.dir.join(format!("{named}.json"));
            // 付け替え先を上書きしないよう、リンクを張ってから元を消す
            // (rename は付け替え先があれば黙って上書きする)。
            match std::fs::hard_link(&path, &target) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                // 兄弟が先に付け替えた。
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "cannot rename the kept request");
                    continue;
                }
            }
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), %e, "cannot drop the kept request after renaming it");
            }
        }
    }

    /// 自分が置いた形の名前を持つ控えのファイルと、その名前 (拡張子なし)。
    fn our_files(&self) -> Vec<(PathBuf, String)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "json") {
                    return None;
                }
                let stem = path.file_stem()?.to_str()?.to_owned();
                is_ours(&stem).then_some((path, stem))
            })
            .collect()
    }

    /// この系列の控えを書く。大きすぎる本文は**持たない** (= 延命しない)。
    ///
    /// 書けたかどうかを返す。書けなかったことは転送を止める理由にならない —
    /// 失うのは止まった会話を繋ぐ機会だけ。
    pub fn save(&self, kept: &Kept) -> bool {
        let series = Series {
            session_id: kept.session_id.clone(),
            prefix: kept.prefix.clone(),
        };
        let size = match serde_json::to_vec(&kept.body) {
            Ok(json) => json.len(),
            Err(e) => {
                tracing::warn!(%e, "cannot measure the request to keep");
                return false;
            }
        };
        if size > BODY_LIMIT {
            tracing::info!(
                session = %kept.session_id,
                prefix = %kept.prefix,
                bytes = size,
                limit = BODY_LIMIT,
                "this conversation is too large to keep; it will not be kept alive"
            );
            self.remove(&series);
            return false;
        }
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(path = %self.dir.display(), %e, "cannot create the cache keepalive directory");
            return false;
        }
        let path = self.path_of(&series);
        if let Err(e) = write_atomically(&path, kept) {
            tracing::warn!(path = %path.display(), %e, "cannot keep the request for the next keepalive");
            return false;
        }
        true
    }

    /// この系列の控えを捨てる。`.lock` は残す (掴んでいる相手が居る)。
    pub fn remove(&self, series: &Series) {
        let path = self.path_of(series);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), %e, "cannot drop the kept request");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(session: &str) -> Series {
        Series {
            session_id: session.to_owned(),
            prefix: "2cf24dba".to_owned(),
        }
    }

    fn kept(session: &str, body: Value) -> Kept {
        Kept {
            session_id: session.to_owned(),
            prefix: "2cf24dba".to_owned(),
            ns: "default".to_owned(),
            model: "claude-opus-5".to_owned(),
            route: "a".to_owned(),
            body,
            headers: vec![("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned())],
            path: "/v1/messages".to_owned(),
            query: None,
            shape: RequestShape::Messages,
            fires_at_ms: 1_800_003_300_000,
            expires_at_ms: 1_800_003_570_000,
            horizon_end_ms: 1_800_028_800_000,
            since_ms: 1_800_000_000_000,
            count: 2,
            cache_notice: Some("n-1".to_owned()),
        }
    }

    /// 書いたものがそのまま読み戻る。
    #[test]
    fn what_was_written_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        assert_eq!(store.load(&series("s-1")), None, "nothing is kept yet");

        let written = kept("s-1", serde_json::json!({"messages": [{"role": "user"}]}));
        assert!(store.save(&written));
        assert_eq!(store.load(&series("s-1")), Some(written));
    }

    /// 系列ごとに別のファイル。同じ会話でも prefix が違えば別の控え。
    #[test]
    fn each_series_keeps_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        store.save(&kept("s-1", serde_json::json!({"a": 1})));
        let mut other = kept("s-1", serde_json::json!({"a": 2}));
        other.prefix = "ffffffff".to_owned();
        store.save(&other);

        assert_eq!(store.load(&series("s-1")).unwrap().body["a"], 1);
        assert_eq!(
            store
                .load(&Series {
                    session_id: "s-1".to_owned(),
                    prefix: "ffffffff".to_owned(),
                })
                .unwrap()
                .body["a"],
            2
        );
    }

    /// 潰すと同じ形になる会話どうしも、別のファイルに置く。
    ///
    /// 会話の id の英数字以外は名前に使えないので潰すが、`a b` と `a/b` は
    /// どちらも `a-b` になる。同じ prefix で並べても、後から書いた方が先の
    /// 控えを上書きしない。
    #[test]
    fn conversations_folding_to_the_same_name_keep_their_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        assert!(store.save(&kept("a b", serde_json::json!({"a": 1}))));
        assert!(store.save(&kept("a/b", serde_json::json!({"a": 2}))));

        assert_eq!(store.load(&series("a b")).unwrap().body["a"], 1);
        assert_eq!(store.load(&series("a/b")).unwrap().body["a"], 2);
        let mut sessions: Vec<String> = store
            .load_all()
            .into_iter()
            .map(|kept| kept.session_id)
            .collect();
        sessions.sort();
        assert_eq!(sessions, ["a b", "a/b"], "both are kept");
    }

    /// 長い会話の id でも控えを置ける。末尾だけ違う id どうしも別の控え。
    ///
    /// ファイル名には上限 (255 バイト) があるので、名前の長さを id の長さに
    /// 任せると、長い id を名乗る会話は控えを置けずに keepalive から外れる。
    #[test]
    fn a_long_session_id_is_kept_under_a_name_of_bounded_length() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let one = format!("{}1", "x".repeat(1000));
        let other = format!("{}2", "x".repeat(1000));

        assert!(store.save(&kept(&one, serde_json::json!({"a": 1}))));
        assert!(store.save(&kept(&other, serde_json::json!({"a": 2}))));

        assert_eq!(store.load(&series(&one)).unwrap().body["a"], 1);
        assert_eq!(store.load(&series(&other)).unwrap().body["a"], 2);
    }

    /// 名前が中身の系列と合わない控え (会話のハッシュを持たない形) は、
    /// 名前を揃えると系列として読める。
    #[test]
    fn a_kept_request_without_the_hash_in_its_name_is_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let written = kept("s-1", serde_json::json!({"a": 1}));
        std::fs::create_dir_all(&store.dir).unwrap();
        let unhashed = store.dir.join("s-1.2cf24dba.json");
        std::fs::write(&unhashed, serde_json::to_vec(&written).unwrap()).unwrap();
        assert_eq!(
            store.load(&series("s-1")),
            None,
            "the series does not reach a file under that name"
        );

        store.settle_names();

        assert_eq!(store.load(&series("s-1")), Some(written));
        assert!(!unhashed.exists(), "it moved, not copied");
        assert_eq!(store.load_meta_of("s-1").len(), 1);
    }

    /// 揃えた先に控えが既にあれば、そちらを残す。
    ///
    /// 両方残すと、読み戻しで同じ系列を 2 つ拾う。
    #[test]
    fn settling_names_keeps_what_is_already_under_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert!(store.save(&kept("s-1", serde_json::json!({"a": 1}))));
        let unhashed = store.dir.join("s-1.2cf24dba.json");
        std::fs::write(
            &unhashed,
            serde_json::to_vec(&kept("s-1", serde_json::json!({"a": 2}))).unwrap(),
        )
        .unwrap();

        store.settle_names();

        assert_eq!(store.load(&series("s-1")).unwrap().body["a"], 1);
        assert!(!unhashed.exists());
        assert_eq!(store.load_all().len(), 1, "one file per series");
    }

    /// 上限を超えた本文は保持しない = その系列は延命しない。
    #[test]
    fn a_conversation_too_large_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        let huge = serde_json::json!({ "text": "x".repeat(BODY_LIMIT + 1) });
        assert!(!store.save(&kept("s-1", huge)));
        assert_eq!(store.load(&series("s-1")), None);
    }

    /// 大きくなった系列は、それまでの控えごと落ちる。
    ///
    /// 残しておくと、伸びた会話のプレフィックスからとうに外れた古い本文を
    /// 撫で続けることになる。
    #[test]
    fn growing_past_the_limit_drops_what_was_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        assert!(store.save(&kept("s-1", serde_json::json!({"a": 1}))));
        let huge = serde_json::json!({ "text": "x".repeat(BODY_LIMIT + 1) });
        assert!(!store.save(&kept("s-1", huge)));

        assert_eq!(store.load(&series("s-1")), None);
    }

    /// 撫でてよいのは 1 台だけ。掴んでいる間、後から来た側は掴めない。
    #[test]
    fn only_one_may_touch_a_series() {
        let dir = tempfile::tempdir().unwrap();
        let one = Store::new(dir.path());
        let other = Store::new(dir.path());

        let claim = one.claim(&series("s-1")).expect("the first claim wins");
        assert!(
            other.claim(&series("s-1")).is_none(),
            "the sibling must not touch the same series"
        );
        assert!(
            other.claim(&series("s-2")).is_some(),
            "another series is free"
        );

        drop(claim);
        assert!(
            other.claim(&series("s-1")).is_some(),
            "the lock is released with the claim"
        );
    }

    /// 置いてある控えを全部読み戻せる。壊れたファイルは飛ばす。
    #[test]
    fn everything_kept_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&kept("s-1", serde_json::json!({"a": 1})));
        store.save(&kept("s-2", serde_json::json!({"a": 2})));
        std::fs::write(store.dir.join("s-3.deadbeef.json"), b"{ not json").unwrap();

        let mut sessions: Vec<String> = store
            .load_all()
            .into_iter()
            .map(|kept| kept.session_id)
            .collect();
        sessions.sort();
        assert_eq!(sessions, ["s-1", "s-2"]);
    }

    /// 自分の名前で置いていないファイルは、読まずに飛ばす。
    ///
    /// 置き場は他の書き手も使う。読みに行けば「壊れている」と毎回警告する
    /// ことになるので、拾うのは `<会話>.<prefix>` の形だけにする。
    #[test]
    fn a_file_someone_else_left_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&kept("s-1", serde_json::json!({"a": 1})));
        std::fs::write(store.dir.join("127-0-0-1-11301.json"), b"{\"watched\":[]}").unwrap();

        let sessions: Vec<String> = store
            .load_all()
            .into_iter()
            .map(|kept| kept.session_id)
            .collect();
        assert_eq!(sessions, ["s-1"], "only what this gateway keeps comes back");
    }

    /// 置き場の下のディレクトリは、控えとして拾わない。
    ///
    /// 乗らなかった 1 本を取っておく `uncached/` がそこに居る。拾ってしまうと、
    /// 読めない控えとして毎回警告するか、畳んだはずの系列を撫で直すことになる。
    #[test]
    fn a_directory_beside_the_kept_requests_is_not_one_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&kept("s-1", serde_json::json!({"a": 1})));
        let aside = store.dir.join("uncached");
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("s-2.deadbeef.1800000000000.json"), b"{}").unwrap();

        let sessions: Vec<String> = store
            .load_all()
            .into_iter()
            .map(|kept| kept.session_id)
            .collect();
        assert_eq!(sessions, ["s-1"], "only the kept requests come back");
    }

    /// 捨てた控えは読めなくなる。
    #[test]
    fn what_was_dropped_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&kept("s-1", serde_json::json!({"a": 1})));

        store.remove(&series("s-1"));
        assert_eq!(store.load(&series("s-1")), None);
        store.remove(&series("s-1"));
    }

    /// 本文を除いた読み方でも、本文以外の欄は書いたとおりに読み戻る。
    #[test]
    fn the_fields_beside_the_body_read_back_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let written = kept("s-1", serde_json::json!({"messages": [{"role": "user"}]}));
        store.save(&written);

        assert_eq!(
            store.load_meta_of("s-1"),
            [KeptMeta {
                session_id: written.session_id,
                prefix: written.prefix,
                ns: written.ns,
                model: written.model,
                route: written.route,
                fires_at_ms: written.fires_at_ms,
                expires_at_ms: written.expires_at_ms,
                horizon_end_ms: written.horizon_end_ms,
                since_ms: written.since_ms,
                count: written.count,
                cache_notice: written.cache_notice,
            }]
        );
        assert_eq!(store.load_meta_of("s-2"), []);
    }

    /// 全件の読み方は面で絞れる。壊れたファイル・他人のファイルは飛ばす。
    #[test]
    fn every_kept_series_reads_back_by_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&kept("s-1", serde_json::json!({"a": 1})));
        let mut elsewhere = kept("s-2", serde_json::json!({"a": 2}));
        elsewhere.ns = "other".to_owned();
        store.save(&elsewhere);
        std::fs::write(store.dir.join("s-3.deadbeef.json"), b"{ not json").unwrap();
        std::fs::write(store.dir.join("127-0-0-1-11301.json"), b"{\"watched\":[]}").unwrap();

        let sessions = |ns: Option<&str>| {
            let mut sessions: Vec<String> = store
                .load_meta_all(ns)
                .into_iter()
                .map(|meta| meta.session_id)
                .collect();
            sessions.sort();
            sessions
        };
        assert_eq!(sessions(None), ["s-1", "s-2"]);
        assert_eq!(sessions(Some("default")), ["s-1"]);
        assert_eq!(sessions(Some("other")), ["s-2"]);
        assert_eq!(sessions(Some("nowhere")), Vec::<String>::new());
    }
}
