//! gateway が入れ替える自動の鍵 (DR-0037)。
//!
//! 自動の鍵は ns ごとに 1 つのファイル (1 行 1 JWK) にあり、行に印
//! (`"auto": true`、`created`、`max_token_ttl`) を持つ。印の無い行には触らない。
//!
//! - 入れ替え ([`rotate`]) だけが書く。脇の `.lock` を flock で掴み、読み直して
//!   「最新の自動の鍵がまだ若ければ何もしない」を確かめてから書く。同じファイルを
//!   共有する unit が同時に走っても、増える鍵は 1 本
//! - 新しい鍵は追記、消すのは一時ファイル + rename。切り詰めて書き直すと、空の
//!   瞬間を読んだ発行と検証が失敗する
//! - 読めないファイルは `<名前>.broken.<UTC の日時>` に退避し、新しい鍵 1 本の束で
//!   作り直す。退避したファイルは消さない (人が原因を見る)
//! - 発行 ([`newest`]) は読むだけ
//!
//! 時刻は全部、呼び出し側が渡す壁時計 (Unix 秒)。

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{SigningKey, private_jwk, read_private_jwk};

/// 消す時刻に足す余裕で、消す処理をまとめる粒度 (秒、DR-0037 §13)。
///
/// 検証の時計の揺れ ([`super::SKEW_SECS`]) と、後継ができた瞬間に古い鍵で署名した
/// 発行の読み遅れを覆う。入れ替えの仕事が 1 度に寝る長さの上限でもある。
pub const COOLTIME_SECS: i64 = 3600;

/// kid の重なりを避けて引き直す回数の上限。
const KID_ATTEMPTS: usize = 64;

/// 自動の鍵の印。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// 作った時刻 (Unix 秒)。
    pub created: i64,
    /// この鍵で発行してよい token の寿命の上限 (秒)。作った時点で刻む (DR-0037 §11)。
    pub max_token_ttl: i64,
}

/// 発行に使う自動の鍵。
#[derive(Debug, Clone)]
pub struct AutoKey {
    pub kid: String,
    pub key: SigningKey,
    pub mark: Mark,
}

/// ファイルの 1 行。書き直す時は `text` をそのまま使う (読み直した値で組み直さない)。
struct Line {
    text: String,
    kid: String,
    key: SigningKey,
    mark: Option<Mark>,
    /// `auto` を名乗るが印の欄が揃っていない (DR-0037 §5)。印の無い行と同じく扱う。
    incomplete: bool,
}

/// 印は 3 欄 (`"auto": true`、整数の `created`、正の整数の `max_token_ttl`) が揃って
/// 初めて印 (DR-0037 §5)。1 つでも欠ければ印の無い行と同じく扱う: 消さない
/// (この鍵の token の寿命が分からない) し、署名にも使わない。
fn mark_of(jwk: &Value) -> Option<Mark> {
    if jwk.get("auto").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let created = jwk.get("created")?.as_i64()?;
    let max_token_ttl = jwk.get("max_token_ttl")?.as_i64()?;
    (max_token_ttl > 0).then_some(Mark {
        created,
        max_token_ttl,
    })
}

/// 行に分けて読む。空のファイルは鍵 0 本。不正な行・kid の重なりはエラー
/// (エラーに秘密鍵の中身は含めない)。
fn parse(text: &str) -> Result<Vec<Line>, String> {
    let mut lines = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, raw) in text.lines().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let line_no = index + 1;
        let (kid, key) = read_private_jwk(raw).map_err(|e| format!("line {line_no}: {e}"))?;
        let kid = kid.ok_or_else(|| format!("line {line_no}: JWK needs `kid`"))?;
        if !seen.insert(kid.clone()) {
            return Err(format!(
                "line {line_no}: duplicate kid `{kid}`; remove one line"
            ));
        }
        // read_private_jwk が JSON として読めたものだけがここに来る。
        let jwk: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        lines.push(Line {
            text: raw.to_owned(),
            kid,
            key,
            mark: mark_of(&jwk),
            incomplete: jwk.get("auto").is_some() && mark_of(&jwk).is_none(),
        });
    }
    Ok(lines)
}

/// ファイルを読む。無ければ `None`。
fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "{}: {e}; check the file and its permissions",
            path.display()
        )),
    }
}

/// 印のある行を古い順 (作成時刻、同時なら kid) に並べる。最後が最新。
fn marked(lines: &[Line]) -> Vec<(&str, Mark)> {
    let mut marked: Vec<(&str, Mark)> = lines
        .iter()
        .filter_map(|line| line.mark.map(|mark| (line.kid.as_str(), mark)))
        .collect();
    marked.sort_by(|(a_kid, a), (b_kid, b)| (a.created, a_kid).cmp(&(b.created, b_kid)));
    marked
}

/// 最新の自動の鍵。ファイルが無い・自動の鍵が無ければ `None`、読めなければエラー。
/// 読むだけで、何も書かない。
pub fn newest(path: &Path) -> Result<Option<AutoKey>, String> {
    let Some(text) = read(path)? else {
        return Ok(None);
    };
    let lines = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some((kid, mark)) = marked(&lines).last().copied() else {
        return Ok(None);
    };
    let line = lines
        .iter()
        .find(|line| line.kid == kid)
        .expect("a marked kid comes from the lines");
    Ok(Some(AutoKey {
        kid: line.kid.clone(),
        key: line.key.clone(),
        mark,
    }))
}

/// 鍵のファイルにある kid (確かめる側のため)。ファイルが無ければ `None`、読めなければ
/// エラー (秘密鍵の中身は含めない)。読むだけで、何も書かない。
pub fn kids(path: &Path) -> Result<Option<Vec<String>>, String> {
    let Some(text) = read(path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse(&text)?.into_iter().map(|line| line.kid).collect(),
    ))
}

/// 1 回の入れ替えでやること。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 新しい鍵を作るか。
    pub create: bool,
    /// 消す古い自動の鍵。
    pub delete: Vec<String>,
    /// 次にやることがある時刻 (Unix 秒)。作るなら作った後の姿で計算する。
    pub next_at: i64,
}

/// 印のある鍵 (古い順) と今の時刻から、やることを決める。
///
/// - 作る: 自動の鍵が無い、または最新が `every` 以上古い
/// - 消す: 最新でない鍵 K のうち、`後継の created + K の max_token_ttl + cooltime`
///   を過ぎたもの。後継ができた後は K で発行されないので、K の token はそこから
///   `max_token_ttl` で全部切れる
pub fn plan(marked: &[(&str, Mark)], now: i64, every: i64) -> Plan {
    let create = marked
        .last()
        .is_none_or(|(_, newest)| now >= newest.created.saturating_add(every));
    // 作るなら、今の最新の後継は今作る鍵。
    let mut successors: Vec<i64> = marked.iter().skip(1).map(|(_, m)| m.created).collect();
    if create && !marked.is_empty() {
        successors.push(now);
    }
    let mut delete = Vec::new();
    let mut next_at = if create {
        now.saturating_add(every)
    } else {
        marked
            .last()
            .map_or(i64::MAX, |(_, newest)| newest.created.saturating_add(every))
    };
    for ((kid, mark), successor) in marked.iter().zip(successors) {
        let due = successor
            .saturating_add(mark.max_token_ttl)
            .saturating_add(COOLTIME_SECS);
        if now >= due {
            delete.push((*kid).to_owned());
        } else {
            next_at = next_at.min(due);
        }
    }
    Plan {
        create,
        delete,
        next_at,
    }
}

/// 入れ替えの指示。
#[derive(Debug, Clone)]
pub struct Rotation<'a> {
    /// kid の頭に付ける ns の名前。
    pub ns: &'a str,
    /// 自動の鍵のファイル。
    pub path: &'a Path,
    /// 入れ替えの周期 (秒)。
    pub every: i64,
    /// 新しく作る鍵に刻む token の寿命の上限 (秒)。
    pub max_token_ttl: i64,
    /// 作る kid が重なってはいけない kid (手の鍵束の kid)。
    pub reserved: &'a BTreeSet<String>,
}

/// 入れ替えの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub created: Option<String>,
    pub deleted: Vec<String>,
    pub next_at: i64,
    /// `auto` を名乗るが印の欄が揃っていない行の kid。残してあるので知らせる。
    pub incomplete: Vec<String>,
    /// 読めずに退避したファイル。退避した上で新しい鍵 1 本の束を書いたので知らせる。
    pub broken: Option<Broken>,
}

/// 読めずに退避した鍵のファイル。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken {
    /// 退避先 (`<名前>.broken.<UTC の日時>`)。
    pub moved_to: PathBuf,
    /// 読めなかった理由 (秘密鍵の中身は含めない)。
    pub error: String,
}

/// 脇の `.lock` の置き場。
pub fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// kid (`<ns>-<地方時の日付>-<16 進 4 桁>`、DR-0037 §9)。
pub fn kid_for(ns: &str, now: i64, tag: [u8; 2]) -> String {
    format!(
        "{ns}-{}-{:02x}{:02x}",
        crate::credential::time::local_date(now),
        tag[0],
        tag[1]
    )
}

/// 1 回入れ替える。`fill` は乱数で埋める口 (OS の生成器を呼び出し側が渡す)。
///
/// 掴んでいる区間は読み直しから書き終わりまで。読めないファイル (無いのは読めない
/// うちに入らない、最初の 1 本を作る) は退避して、新しい鍵 1 本の束で作り直す。
/// 上に書き足すと他の鍵を消しうるし、残しておくと入れ替えも削除も止まり、最後に
/// 読めていた自動の鍵が検証側で効き続ける (DR-0037 §13)。
pub fn rotate(
    rotation: &Rotation<'_>,
    now: i64,
    fill: &mut dyn FnMut(&mut [u8]),
) -> Result<Outcome, String> {
    let path = rotation.path;
    let shown = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: has no directory", path.display()))?;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(shown)?;
    }
    // 秘密鍵を置くので、作るときは自分だけが入れる形にする。
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(shown(e)),
    }
    let _held = hold(&lock_path(path)).map_err(shown)?;
    sweep_temporaries(path);

    let readable = read(path).and_then(|text| {
        let lines =
            parse(text.as_deref().unwrap_or("")).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok((text, lines))
    });
    let (text, lines, broken) = match readable {
        Ok((text, lines)) => (text, lines, None),
        Err(error) => {
            let moved_to = set_aside(path, now).map_err(|e| {
                format!("{error}; cannot move it aside ({e}), so not rotating until it is fixed")
            })?;
            (None, Vec::new(), Some(Broken { moved_to, error }))
        }
    };
    let plan = plan(&marked(&lines), now, rotation.every);

    let created = if plan.create {
        let mut taken: BTreeSet<&str> = rotation.reserved.iter().map(String::as_str).collect();
        taken.extend(lines.iter().map(|line| line.kid.as_str()));
        Some(new_line(rotation, now, &taken, fill)?)
    } else {
        None
    };

    if !plan.delete.is_empty() || text.as_deref().is_none_or(|t| t.trim().is_empty()) {
        // 消す、または最初の 1 本: 丸ごと一時ファイルに書いて rename する。
        if !plan.delete.is_empty() || created.is_some() {
            let mut body = String::new();
            for line in lines.iter().filter(|line| !plan.delete.contains(&line.kid)) {
                body.push_str(&line.text);
                body.push('\n');
            }
            if let Some((_, line)) = &created {
                body.push_str(line);
                body.push('\n');
            }
            write_atomically(path, body.as_bytes()).map_err(shown)?;
        }
    } else if let Some((_, line)) = &created {
        // 新しい鍵は追記 (inode を変えない、空の瞬間を作らない)。
        let separator = if text.as_deref().is_some_and(|t| !t.ends_with('\n')) {
            "\n"
        } else {
            ""
        };
        append(path, format!("{separator}{line}\n").as_bytes()).map_err(shown)?;
    }

    Ok(Outcome {
        created: created.map(|(kid, _)| kid),
        deleted: plan.delete,
        next_at: plan.next_at,
        incomplete: lines
            .iter()
            .filter(|line| line.incomplete)
            .map(|line| line.kid.clone())
            .collect(),
        broken,
    })
}

/// 読めないファイルを `<名前>.broken.<UTC の日時 YYYYMMDDTHHMMSSZ>` へ rename で
/// 退避する。その名前が既にあれば `.1`、`.2` … を足す。退避先を作るのは flock の
/// 内側の書き手だけなので、確かめてから rename するまでの間に別の書き手は来ない。
fn set_aside(path: &Path, now: i64) -> std::io::Result<PathBuf> {
    let stamp = crate::credential::time::format_rfc3339(now).replace(['-', ':'], "");
    let base = format!(
        "{}.broken.{stamp}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut moved_to = path.with_file_name(&base);
    let mut seq = 0u32;
    while std::fs::symlink_metadata(&moved_to).is_ok() {
        seq += 1;
        moved_to = path.with_file_name(format!("{base}.{seq}"));
    }
    std::fs::rename(path, &moved_to)?;
    Ok(moved_to)
}

/// 新しい鍵の行を作る。kid は `taken` と重ならないよう引き直す。
fn new_line(
    rotation: &Rotation<'_>,
    now: i64,
    taken: &BTreeSet<&str>,
    fill: &mut dyn FnMut(&mut [u8]),
) -> Result<(String, String), String> {
    for _ in 0..KID_ATTEMPTS {
        let mut tag = [0u8; 2];
        fill(&mut tag);
        let kid = kid_for(rotation.ns, now, tag);
        if taken.contains(kid.as_str()) {
            continue;
        }
        let mut seed = [0u8; 32];
        fill(&mut seed);
        let mut jwk = private_jwk(&kid, &super::signing_key(&seed));
        jwk["auto"] = Value::Bool(true);
        jwk["created"] = now.into();
        jwk["max_token_ttl"] = rotation.max_token_ttl.into();
        return Ok((kid, jwk.to_string()));
    }
    Err(format!(
        "could not pick a kid for `{}` that is not taken",
        rotation.ns
    ))
}

/// 脇の `.lock` を掴む。掴めるまで待つ。`.lock` は消さない (消して作り直すと、
/// 掴んでいる側と後から来た側が別のファイルを見て締め出しが破れる)。
fn hold(lock: &Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock)?;
    loop {
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
            Ok(()) => return Ok(file),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

/// 一時ファイルの名前の頭 (`<名前>.tmp.`)。
fn temporary_prefix(path: &Path) -> String {
    format!(
        "{}.tmp.",
        path.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// `<名前>.tmp.<pid>.<連番>` の形か。前方一致だけだと、名前が入れ子になった別の ns
/// (`lab.jwks.jsonl.tmp.1` という ns の `lab.jwks.jsonl.tmp.1.jwks.jsonl`) を消しうる。
fn is_temporary(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    matches!(rest.split_once('.'), Some((pid, seq)) if digits(pid) && digits(seq))
}

/// 落ちた書き手が残した一時ファイルを消す。一時ファイルは flock の内側でしか
/// 作らないので、掴んでいる間に見えるものは全部残骸。
fn sweep_temporaries(path: &Path) {
    let (Some(dir), prefix) = (path.parent(), temporary_prefix(path)) else {
        return;
    };
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if is_temporary(&name.to_string_lossy(), &prefix) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {
                    tracing::info!(path = %entry.path().display(), "cleaned up a stray key ring write")
                }
                Err(e) => {
                    tracing::warn!(path = %entry.path().display(), %e, "cannot remove a stray key ring write")
                }
            }
        }
    }
}

/// 一時ファイル (600) に書いて fsync し、rename で差し替える。
fn write_atomically(path: &Path, body: &[u8]) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!(
        "{}{}.{seq}",
        temporary_prefix(path),
        std::process::id()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// 1 回の write で追記して fsync する。
fn append(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    const NOW: i64 = 1_791_270_000;
    const DAY: i64 = 86_400;
    const WEEK: i64 = 7 * DAY;

    fn mark(created: i64, max_token_ttl: i64) -> Mark {
        Mark {
            created,
            max_token_ttl,
        }
    }

    /// 決まった列を返す乱数の口。kid の 4 桁は呼ぶたびに進む。
    fn counter() -> impl FnMut(&mut [u8]) {
        let mut n: u8 = 0;
        move |buf: &mut [u8]| {
            n = n.wrapping_add(1);
            buf.fill(n);
        }
    }

    fn rotation<'a>(path: &'a Path, reserved: &'a BTreeSet<String>) -> Rotation<'a> {
        Rotation {
            ns: "lab",
            path,
            every: WEEK,
            max_token_ttl: WEEK,
            reserved,
        }
    }

    fn kids(path: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(path).unwrap();
        parse(&text)
            .unwrap()
            .into_iter()
            .map(|line| line.kid)
            .collect()
    }

    fn manual_line(kid: &str, seed: u8) -> String {
        private_jwk(kid, &super::super::signing_key(&[seed; 32])).to_string()
    }

    #[test]
    fn nothing_yet_means_create_and_come_back_after_a_period() {
        assert_eq!(
            plan(&[], NOW, WEEK),
            Plan {
                create: true,
                delete: vec![],
                next_at: NOW + WEEK
            }
        );
    }

    #[test]
    fn a_young_newest_key_is_left_alone_until_it_ages() {
        let keys = [("a", mark(NOW - DAY, WEEK))];
        assert_eq!(
            plan(&keys, NOW, WEEK),
            Plan {
                create: false,
                delete: vec![],
                next_at: NOW - DAY + WEEK
            }
        );
        // ちょうど周期で作る。
        let p = plan(&keys, NOW - DAY + WEEK, WEEK);
        assert!(p.create);
        // 作ったら、前の鍵の消す時刻 (今 + その鍵の上限 + cooltime) より周期が先なら、
        // 次は周期の方。
        assert_eq!(p.next_at, NOW - DAY + WEEK + WEEK);
    }

    /// 消す時刻は「後継の created + 消す鍵の max_token_ttl + cooltime」。その 1 秒前は
    /// 消さずにその時刻を次の時刻にする。
    #[test]
    fn an_old_key_goes_exactly_at_its_successor_plus_its_cap_plus_cooltime() {
        let successor = NOW;
        let keys = [
            ("old", mark(NOW - WEEK, DAY)),
            ("new", mark(successor, WEEK)),
        ];
        let due = successor + DAY + COOLTIME_SECS;
        let before = plan(&keys, due - 1, WEEK);
        assert_eq!(before.delete, Vec::<String>::new());
        assert!(!before.create);
        assert_eq!(before.next_at, due);
        let at = plan(&keys, due, WEEK);
        assert_eq!(at.delete, vec!["old".to_owned()]);
        assert_eq!(at.next_at, successor + WEEK);
    }

    /// 上限は消す鍵の行に刻まれた値で決まる (今の設定の周期ではない)。
    #[test]
    fn the_cap_written_on_the_old_key_decides_when_it_goes() {
        let keys = [
            ("old", mark(NOW - 30 * DAY, 30 * DAY)),
            ("new", mark(NOW, DAY)),
        ];
        // 周期を 1 日に縮めても、old の token は 30 日生きうるので消さない。
        let p = plan(&keys, NOW + 2 * DAY, DAY);
        assert!(p.delete.is_empty());
        assert!(p.create);
        let p = plan(&keys, NOW + 30 * DAY + COOLTIME_SECS, DAY);
        assert!(p.delete.contains(&"old".to_owned()));
    }

    /// 長く止まっていた後は、作るのと消すのが同じ回に重なる。最新は消さない。
    #[test]
    fn after_a_long_stop_it_creates_and_deletes_in_one_pass() {
        let keys = [
            ("a", mark(NOW - 40 * DAY, WEEK)),
            ("b", mark(NOW - 30 * DAY, WEEK)),
        ];
        let p = plan(&keys, NOW, WEEK);
        assert!(p.create);
        assert_eq!(
            p.delete,
            vec!["a".to_owned()],
            "b is the newest's predecessor now"
        );
        assert_eq!(
            p.next_at,
            NOW + WEEK,
            "b goes at now + its cap + cooltime, after the next rotation"
        );
    }

    /// 時計が戻って最新の鍵の `created` が未来でも、若いとみなして作らない。次の時刻は
    /// `created + every` (時計が追いつくまで待つ)。
    #[test]
    fn a_newest_key_from_the_future_counts_as_young() {
        let keys = [
            ("old", mark(NOW - WEEK, WEEK)),
            ("ahead", mark(NOW + DAY, WEEK)),
        ];
        let p = plan(&keys, NOW, WEEK);
        assert!(!p.create);
        assert!(p.delete.is_empty(), "old's successor is in the future too");
        assert_eq!(p.next_at, NOW + DAY + WEEK);
    }

    #[test]
    fn rotation_creates_a_marked_private_key_readable_only_by_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jwt-keys/lab.jwks.jsonl");
        let reserved = BTreeSet::new();
        let outcome = rotate(&rotation(&path, &reserved), NOW, &mut counter()).unwrap();
        let kid = outcome.created.clone().unwrap();
        assert!(
            kid.starts_with("lab-") && kid.len() == "lab-2026-10-06-0101".len(),
            "{kid}"
        );
        assert_eq!(outcome.deleted, Vec::<String>::new());
        assert_eq!(outcome.next_at, NOW + WEEK);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&lock_path(&path)), 0o600);
        let newest = newest(&path).unwrap().unwrap();
        assert_eq!(newest.kid, kid);
        assert_eq!(newest.mark, mark(NOW, WEEK));
        // 検証側の鍵束としてもそのまま読める。
        assert!(super::super::read_key_ring(&std::fs::read_to_string(&path).unwrap()).is_ok());
    }

    /// 2 度目は「若い」を見て何もしない。
    #[test]
    fn rotation_is_idempotent_while_the_newest_is_young() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let reserved = BTreeSet::new();
        rotate(&rotation(&path, &reserved), NOW, &mut counter()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let again = rotate(&rotation(&path, &reserved), NOW + DAY, &mut counter()).unwrap();
        assert_eq!(again.created, None);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// 2 つの書き手が同じ時刻に同時に入れ替えても、増える鍵は 1 本。
    #[test]
    fn two_writers_at_once_add_exactly_one_key() {
        for round in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("jwt-keys/lab.jwks.jsonl");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = (0..2u8)
                .map(|writer| {
                    let (path, barrier) = (path.clone(), std::sync::Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        let reserved = BTreeSet::new();
                        let mut fill = move |buf: &mut [u8]| buf.fill(writer.wrapping_add(round));
                        barrier.wait();
                        rotate(&rotation(&path, &reserved), NOW, &mut fill).unwrap()
                    })
                })
                .collect();
            let outcomes: Vec<Outcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert_eq!(
                outcomes.iter().filter(|o| o.created.is_some()).count(),
                1,
                "round {round}: {outcomes:?}"
            );
            assert_eq!(kids(&path).len(), 1, "round {round}");
        }
    }

    /// 印の無い行 (人が書いた鍵) は、消す回にも残す。
    #[test]
    fn unmarked_lines_survive_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let reserved = BTreeSet::new();
        std::fs::write(&path, format!("{}\n", manual_line("by-hand", 9))).unwrap();
        let first = rotate(&rotation(&path, &reserved), NOW, &mut counter())
            .unwrap()
            .created
            .unwrap();
        let second = rotate(&rotation(&path, &reserved), NOW + WEEK, &mut counter())
            .unwrap()
            .created
            .unwrap();
        let outcome = rotate(
            &rotation(&path, &reserved),
            NOW + WEEK + WEEK + COOLTIME_SECS,
            &mut counter(),
        )
        .unwrap();
        assert_eq!(outcome.deleted, vec![first]);
        let left = kids(&path);
        assert!(left.contains(&"by-hand".to_owned()), "{left:?}");
        assert!(left.contains(&second), "{left:?}");
    }

    /// 消すのは一時ファイル + rename (inode が替わる)、作るのは追記 (inode が同じ)。
    /// どちらも一時ファイルを残さない。
    #[test]
    fn deletions_replace_the_file_and_additions_append_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let reserved = BTreeSet::new();
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        rotate(&rotation(&path, &reserved), NOW, &mut counter()).unwrap();
        let first = ino(&path);
        // 2 本目は追記。
        let mut fill = counter();
        fill(&mut [0u8; 2]);
        rotate(&rotation(&path, &reserved), NOW + WEEK, &mut fill).unwrap();
        assert_eq!(ino(&path), first, "an addition appends in place");
        assert_eq!(kids(&path).len(), 2);
        // 1 本目を消す回は差し替え。
        let outcome = rotate(
            &rotation(&path, &reserved),
            NOW + 2 * WEEK + COOLTIME_SECS,
            &mut counter(),
        )
        .unwrap();
        assert_eq!(outcome.deleted.len(), 1);
        assert_ne!(ino(&path), first, "a deletion renames a new file in");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.contains(".tmp.")), "{names:?}");
    }

    /// 消している最中も、読み手は空や途中のファイルを見ない。
    #[test]
    fn a_reader_never_sees_an_empty_ring_while_keys_are_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let reserved = BTreeSet::new();
        // 鍵 2 本 (古い方は消せる時刻にある) を作ってから、消す回を繰り返す。
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let (path, stop) = (path.clone(), std::sync::Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut reads = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        assert!(
                            super::super::read_key_ring(&text).is_ok(),
                            "read a ring that is not whole"
                        );
                        reads += 1;
                    }
                }
                reads
            })
        };
        for round in 0..50 {
            let base = NOW + round * 3 * WEEK;
            let mut fill = counter();
            for _ in 0..round {
                fill(&mut [0u8; 2]);
            }
            rotate(&rotation(&path, &reserved), base, &mut fill).unwrap();
            rotate(&rotation(&path, &reserved), base + WEEK, &mut fill).unwrap();
            rotate(
                &rotation(&path, &reserved),
                base + 2 * WEEK + COOLTIME_SECS,
                &mut fill,
            )
            .unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
    }

    /// 退避先の名前 (`<名前>.broken.<UTC の日時>`、NOW は 2026-10-06T07:00:00Z)。
    const BROKEN: &str = "lab.jwks.jsonl.broken.20261006T070000Z";

    /// ディレクトリにある `.broken.` の名前。
    fn broken_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".broken."))
            .collect();
        names.sort();
        names
    }

    /// 読めないファイルは中身のまま退避し、新しい自動の鍵 1 本の束で作り直す。
    #[test]
    fn a_broken_file_is_moved_aside_and_replaced_by_one_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let reserved = BTreeSet::new();
        // 2 行目は秘密鍵 (`d`) を持つが、`x` が別の鍵の JWK。エラーに `d` の値を出さない。
        let mut mixed: Value = serde_json::from_str(&manual_line("mixed", 8)).unwrap();
        let secret = mixed["d"].as_str().unwrap().to_owned();
        mixed["x"] = Value::String(super::super::public_key_text(
            &super::super::signing_key(&[9; 32]).verifying_key(),
        ));
        let broken = format!("{}\n{mixed}\n", manual_line("by-hand", 9));
        std::fs::write(&path, &broken).unwrap();
        let error = newest(&path).unwrap_err();
        assert!(
            !error.contains(&secret),
            "the private key leaked into: {error}"
        );

        let outcome = rotate(&rotation(&path, &reserved), NOW, &mut counter()).unwrap();
        let moved = outcome.broken.clone().unwrap();
        assert_eq!(moved.moved_to, dir.path().join(BROKEN));
        assert!(moved.error.contains("line 2"), "{}", moved.error);
        assert!(
            !moved.error.contains(&secret),
            "the private key leaked into: {}",
            moved.error
        );
        assert_eq!(std::fs::read_to_string(&moved.moved_to).unwrap(), broken);
        let created = outcome.created.unwrap();
        assert_eq!(kids(&path), vec![created.clone()], "only the new key");
        assert_eq!(newest(&path).unwrap().unwrap().kid, created);
        assert_eq!(outcome.next_at, NOW + WEEK);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // 作り直した束は読めるので、次の回は退避しない。
        let again = rotate(&rotation(&path, &reserved), NOW + DAY, &mut counter()).unwrap();
        assert_eq!(again.broken, None);
        assert_eq!(broken_names(dir.path()), vec![BROKEN.to_owned()]);
    }

    /// JSON でない・UTF-8 でない中身も読めないうちに入る。退避先の名前が既にあれば
    /// 番号を足し、前に退避したファイルには触らない。
    #[test]
    fn a_taken_broken_name_gets_a_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let reserved = BTreeSet::new();
        std::fs::write(dir.path().join(BROKEN), "earlier").unwrap();
        std::fs::write(&path, "garbage\n").unwrap();
        let first = rotate(&rotation(&path, &reserved), NOW, &mut counter()).unwrap();
        assert_eq!(
            first.broken.unwrap().moved_to,
            dir.path().join(format!("{BROKEN}.1"))
        );
        std::fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
        let second = rotate(&rotation(&path, &reserved), NOW, &mut counter()).unwrap();
        assert_eq!(
            second.broken.unwrap().moved_to,
            dir.path().join(format!("{BROKEN}.2"))
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(BROKEN)).unwrap(),
            "earlier"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(format!("{BROKEN}.1"))).unwrap(),
            "garbage\n"
        );
        assert_eq!(
            std::fs::read(dir.path().join(format!("{BROKEN}.2"))).unwrap(),
            [0xff, 0xfe, b'\n']
        );
        assert_eq!(kids(&path).len(), 1);
    }

    /// 無いファイル・空のファイルは壊れていない (最初の 1 本を作るだけで、退避しない)。
    #[test]
    fn a_missing_or_empty_file_is_not_broken() {
        let dir = tempfile::tempdir().unwrap();
        let reserved = BTreeSet::new();
        let missing = dir.path().join("lab.jwks.jsonl");
        let outcome = rotate(&rotation(&missing, &reserved), NOW, &mut counter()).unwrap();
        assert_eq!(outcome.broken, None);
        assert!(outcome.created.is_some());
        let empty = dir.path().join("empty/lab.jwks.jsonl");
        std::fs::create_dir_all(empty.parent().unwrap()).unwrap();
        std::fs::write(&empty, "\n").unwrap();
        let outcome = rotate(&rotation(&empty, &reserved), NOW, &mut counter()).unwrap();
        assert_eq!(outcome.broken, None);
        assert_eq!(kids(&empty).len(), 1);
        assert!(broken_names(dir.path()).is_empty());
        assert!(broken_names(empty.parent().unwrap()).is_empty());
    }

    /// 退避できない (ディレクトリに書けない) 時は、何も書かずにエラーにする。
    #[test]
    fn a_broken_file_that_cannot_be_moved_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let reserved = BTreeSet::new();
        std::fs::write(&path, "garbage\n").unwrap();
        // .lock は先に作っておく (書けないディレクトリには作れない)。
        std::fs::write(lock_path(&path), "").unwrap();
        let mode = |m: u32| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(dir.path(), mode(0o500)).unwrap();
        // root は書けないディレクトリにも書けるので、確かめられない。
        let enforced = std::fs::File::create(dir.path().join("probe")).is_err();
        let result = rotate(&rotation(&path, &reserved), NOW, &mut counter());
        std::fs::set_permissions(dir.path(), mode(0o700)).unwrap();
        if !enforced {
            return;
        }
        let error = result.unwrap_err();
        assert!(error.contains("cannot move it aside"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "garbage\n");
        assert!(broken_names(dir.path()).is_empty());
    }

    /// 作る kid は手の鍵束の kid と重ならない。
    #[test]
    fn a_new_kid_avoids_the_manual_kids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let taken = kid_for("lab", NOW, [1, 1]);
        let reserved = BTreeSet::from([taken.clone()]);
        let created = rotate(&rotation(&path, &reserved), NOW, &mut counter())
            .unwrap()
            .created
            .unwrap();
        assert_ne!(created, taken);
    }

    /// 前の書き手が落ちて残した一時ファイルは、掴んだ時に片付ける。
    #[test]
    fn leftovers_of_a_crashed_writer_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let leftover = dir.path().join("lab.jwks.jsonl.tmp.99999.0");
        let other = dir.path().join("other.jwks.jsonl.tmp.99999.0");
        std::fs::write(&leftover, "x").unwrap();
        std::fs::write(&other, "x").unwrap();
        rotate(&rotation(&path, &BTreeSet::new()), NOW, &mut counter()).unwrap();
        assert!(!leftover.exists());
        assert!(other.exists(), "another ns's file is not ours to sweep");
    }

    /// 一時ファイルの形 (`<名前>.tmp.<pid>.<連番>`) まで合うものだけを消す。名前が
    /// 入れ子になった別の ns のファイルは消さない。
    #[test]
    fn sweeping_spares_a_nested_namespace_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let nested = dir.path().join("lab.jwks.jsonl.tmp.1.jwks.jsonl");
        let nested_lock = dir.path().join("lab.jwks.jsonl.tmp.1.jwks.jsonl.lock");
        let leftover = dir.path().join("lab.jwks.jsonl.tmp.123.4");
        for p in [&nested, &nested_lock, &leftover] {
            std::fs::write(p, "x").unwrap();
        }
        rotate(&rotation(&path, &BTreeSet::new()), NOW, &mut counter()).unwrap();
        assert!(nested.exists() && nested_lock.exists());
        assert!(!leftover.exists());
    }

    /// 印は 3 欄 (`auto: true`・`created`・`max_token_ttl`) が揃って初めて印。欠けた行は
    /// 印の無い行と同じく消さず署名にも使わず、入れ替えの結果に挙げて知らせる。
    #[test]
    fn a_partial_mark_is_kept_unused_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        let mut partial: Value = serde_json::from_str(&manual_line("half", 5)).unwrap();
        partial["auto"] = Value::Bool(true);
        partial["created"] = (NOW - 100 * DAY).into();
        std::fs::write(&path, format!("{partial}\n")).unwrap();
        assert!(newest(&path).unwrap().is_none(), "not used for signing");
        let reserved = BTreeSet::new();
        let first = rotate(&rotation(&path, &reserved), NOW, &mut counter()).unwrap();
        assert_eq!(first.incomplete, vec!["half".to_owned()]);
        let later = rotate(&rotation(&path, &reserved), NOW + 3 * WEEK, &mut counter()).unwrap();
        assert_eq!(later.incomplete, vec!["half".to_owned()]);
        assert!(kids(&path).contains(&"half".to_owned()), "never deleted");
    }

    #[test]
    fn newest_reads_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lab.jwks.jsonl");
        assert!(newest(&path).unwrap().is_none(), "no file yet");
        std::fs::write(&path, format!("{}\n", manual_line("by-hand", 9))).unwrap();
        assert!(newest(&path).unwrap().is_none(), "no marked key");
        std::fs::write(&path, "garbage\n").unwrap();
        assert!(newest(&path).is_err());
        assert!(!lock_path(&path).exists(), "reading takes no lock");
    }
}
