//! 同じ prefix のはずの 2 本の request の本文を比べ、食い違う位置を全部並べる
//! (`dump diff`)。
//!
//! prompt cache は 1 か所でも違えばそこから後ろを読めないので、最初の 1 か所で
//! 止めずに全部の違いを並べる。比べる順は cache の prefix と同じ
//! `tools` → `system` → `messages`。
//!
//! **本文の中身は出さない**。出すのは位置 (区分・番号)・種類 (block の型、どの欄が
//! 違うか)・大きさ・hash だけ。控えの本文を見せる口を増やさない (DR-0042 決定 1
//! の線)。欄の名前 (tool_use の `input` の鍵など) は位置の一部として出る。
//!
//! 台には聞かない。読むのは手元のファイル (ダンプの `*.request.body`、DR-0038) と、
//! `--kept` の時は keepalive の控え (DR-0027) の置き場だけ。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use llm_gateway::cache::keepalive::Series;
use llm_gateway::cache::keepalive::store::Store;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::destination;
use crate::failure::Failure;
use crate::help;
use crate::options::{split, take_value};

/// 1 つの違いの中で並べる欄の数の上限。形の違う値 (配列と文字列など) の下を
/// 全部たどると、同じ 1 か所の違いが何十行にもなる。
const MAX_FIELDS: usize = 16;

/// hash の桁数 (16 進)。同じかどうかを目で見比べるためのもので、衝突の耐性は
/// 求めない。
const HASH_DIGITS: usize = 12;

pub fn run(args: &[String]) -> Result<ExitCode, Failure> {
    let Some(inputs) = parse(args)? else {
        print!("{}", help::DUMP);
        return Ok(ExitCode::SUCCESS);
    };
    let a = load(&inputs.a)?;
    let b = load(&inputs.b)?;
    for line in report(&a, &b) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

/// 比べる 1 本の出どころ。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// request の本文の JSON のファイル (ダンプの `*.request.body` など)。
    File(PathBuf),
    /// keepalive の控え。会話の id と、系列が複数ある時の prefix。
    Kept {
        session_id: String,
        prefix: Option<String>,
        unit: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Inputs {
    a: Source,
    b: Source,
}

/// 引数を読む。何も渡されなければ `None` (help を出す)。
fn parse(args: &[String]) -> Result<Option<Inputs>, Failure> {
    let mut paths = Vec::new();
    let mut kept = None;
    let mut prefix = None;
    let mut unit = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            paths.extend(it.by_ref().map(PathBuf::from));
            break;
        }
        match split(arg) {
            Some(("kept", inline)) => kept = Some(take_value("kept", inline, &mut it)?),
            Some(("prefix", inline)) => prefix = Some(take_value("prefix", inline, &mut it)?),
            Some(("unit", inline)) => unit = Some(take_value("unit", inline, &mut it)?),
            Some(_) => return Err(Failure::from(format!("could not understand `{arg}`"))),
            None => paths.push(PathBuf::from(arg)),
        }
    }
    if args.is_empty() {
        return Ok(None);
    }
    match kept {
        None => {
            if prefix.is_some() || unit.is_some() {
                return Err(Failure::from(
                    "--prefix and --unit choose the kept copy, so they go with --kept".to_owned(),
                ));
            }
            match <[PathBuf; 2]>::try_from(paths) {
                Ok([a, b]) => Ok(Some(Inputs {
                    a: Source::File(a),
                    b: Source::File(b),
                })),
                Err(paths) => Err(Failure::from(format!(
                    "diff compares two request bodies (or one with --kept), got {}",
                    paths.len()
                ))),
            }
        }
        Some(session_id) => match <[PathBuf; 1]>::try_from(paths) {
            Ok([b]) => Ok(Some(Inputs {
                a: Source::Kept {
                    session_id,
                    prefix,
                    unit,
                },
                b: Source::File(b),
            })),
            Err(paths) => Err(Failure::from(format!(
                "with --kept, diff compares the kept copy with one request body, got {}",
                paths.len()
            ))),
        },
    }
}

/// 読み込んだ 1 本。
#[derive(Debug, Clone, PartialEq)]
struct Side {
    body: Value,
    /// 出どころの説明 (入力の行にそのまま出す)。本文は入れない。
    about: Map<String, Value>,
    /// 受けた形 (`received`、ダンプ) か、送った形 (`sent`、控え) か。
    form: Form,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Received,
    Sent,
}

impl Form {
    fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Sent => "sent",
        }
    }
}

fn load(source: &Source) -> Result<Side, Failure> {
    match source {
        Source::File(path) => load_file(path),
        Source::Kept {
            session_id,
            prefix,
            unit,
        } => {
            let registry = llm_gateway::daemon::registry::open();
            let (_, config) = destination::configured(&registry, unit.as_deref())?;
            let store = Store::new(config.stats.resolve_dir());
            load_kept(&store, session_id, prefix.as_deref())
        }
    }
}

/// ダンプの本文 (または任意の request の本文の JSON) を読む。
///
/// `<ts>-<seq>.request.body` なら、同じ 1 本の試行の `*.response.json` から、
/// gateway が送る前に加えた書き換え (`request_rewrites`、DR-0038 決定 7) を拾う。
/// 拾うのは番号の一番大きい試行 (最後に送った形) の分。
fn load_file(path: &Path) -> Result<Side, Failure> {
    let raw = std::fs::read(path).map_err(|e| {
        Failure::new(
            "body_unreadable",
            format!("could not read {}: {e}", path.display()),
        )
    })?;
    let body: Value = serde_json::from_slice(&raw).map_err(|e| {
        Failure::new(
            "body_unreadable",
            format!("{} is not a JSON request body: {e}", path.display()),
        )
    })?;
    if !body.is_object() {
        return Err(Failure::new(
            "body_unreadable",
            format!("{} is not a JSON object", path.display()),
        ));
    }
    let mut about = Map::new();
    about.insert("source".to_owned(), json!("file"));
    about.insert("path".to_owned(), json!(path.display().to_string()));
    about.insert("bytes".to_owned(), json!(raw.len()));
    if let Some((attempt, rewrites)) = rewrites_of(path) {
        about.insert("attempt".to_owned(), json!(attempt));
        about.insert("request_rewrites".to_owned(), rewrites);
    }
    Ok(Side {
        body,
        about,
        form: Form::Received,
    })
}

/// `<stem>.request.body` の隣の `<stem>.<attempt>.response.json` のうち、番号の
/// 一番大きいものの `request_rewrites`。ダンプでなければ `None`。
fn rewrites_of(path: &Path) -> Option<(u32, Value)> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".request.body")?;
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    let attempt = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.strip_prefix(stem)?
                .strip_prefix('.')?
                .strip_suffix(".response.json")?
                .parse::<u32>()
                .ok()
        })
        .max()?;
    let meta = std::fs::read(dir.join(format!("{stem}.{attempt}.response.json"))).ok()?;
    let meta: Value = serde_json::from_slice(&meta).ok()?;
    Some((attempt, meta.get("request_rewrites")?.clone()))
}

/// 控えを読む。系列が複数あって `prefix` で絞れなければ断る。
fn load_kept(store: &Store, session_id: &str, prefix: Option<&str>) -> Result<Side, Failure> {
    let metas: Vec<_> = store
        .load_meta_of(session_id)
        .into_iter()
        .filter(|meta| prefix.is_none_or(|prefix| meta.prefix == prefix))
        .collect();
    let series = |meta: &llm_gateway::cache::keepalive::KeptMeta| json!({"prefix": meta.prefix, "ns": meta.ns, "model": meta.model});
    let meta = match metas.as_slice() {
        [] => {
            let mut failure = Failure::new(
                "no_kept_copy",
                format!(
                    "there is no kept copy of `{session_id}`{}. only conversations kept alive \
(main conversations of a namespace whose cache is `keepalive`) have one, and it is removed \
when the series ends",
                    prefix
                        .map(|p| format!(" with prefix {p}"))
                        .unwrap_or_default()
                ),
            );
            if prefix.is_some() {
                let all = store.load_meta_of(session_id);
                failure = failure.with("series", all.iter().map(series).collect::<Vec<_>>());
            }
            return Err(failure);
        }
        [meta] => meta,
        _ => {
            return Err(Failure::new(
                "prefix_required",
                format!(
                    "`{session_id}` has more than one kept series. say which one with --prefix"
                ),
            )
            .with("series", metas.iter().map(series).collect::<Vec<_>>()));
        }
    };
    let kept = store
        .load(&Series {
            session_id: meta.session_id.clone(),
            prefix: meta.prefix.clone(),
        })
        .ok_or_else(|| {
            Failure::new(
                "no_kept_copy",
                format!("the kept copy of `{session_id}` went away while it was being read"),
            )
        })?;
    let mut about = Map::new();
    about.insert("source".to_owned(), json!("kept"));
    about.insert("session_id".to_owned(), json!(kept.session_id));
    about.insert("prefix".to_owned(), json!(kept.prefix));
    about.insert("ns".to_owned(), json!(kept.ns));
    about.insert("model".to_owned(), json!(kept.model));
    about.insert("bytes".to_owned(), json!(serialized(&kept.body).len()));
    Ok(Side {
        body: kept.body,
        about,
        form: Form::Sent,
    })
}

/// 出す行を全部組む: 入力の 2 行、違いの行、最後に要約の 1 行。
fn report(a: &Side, b: &Side) -> Vec<Value> {
    let mut lines = Vec::new();
    for (name, side) in [("a", a), ("b", b)] {
        let mut line = Map::new();
        line.insert("record".to_owned(), json!("input"));
        line.insert("side".to_owned(), json!(name));
        line.insert("form".to_owned(), json!(side.form.as_str()));
        line.extend(side.about.clone());
        lines.push(Value::Object(line));
    }
    let compared = compare(&a.body, &b.body);
    let mut summary = compared.summary();
    if a.form != b.form {
        summary.insert(
            "note".to_owned(),
            json!(
                "one side is a kept copy (as sent upstream) and the other a body as received: \
what the gateway rewrote before sending (model, thinking display and thinking turned into \
text, cache lifetimes; see request_rewrites of the received side) shows up as differences"
            ),
        );
    }
    lines.extend(compared.differences);
    lines.push(Value::Object(summary));
    lines
}

/// 比べた結果。
#[derive(Debug, Default)]
struct Compared {
    differences: Vec<Value>,
    counts: Map<String, Value>,
    /// cache の順で最初に違った位置。
    first: Option<Value>,
    tools_identical: bool,
    system_identical: bool,
    /// 先頭から続けて一致した messages の数と、その大きさ (a の側で測る)。
    /// `tools` / `system` の一致とは別に数える (どちらかが違っても、messages の
    /// どこまでが同じかは揃える範囲を決める材料になる)。
    leading_messages: usize,
    leading_message_bytes: usize,
    /// 同じ番号どうしで一致した messages の数 (先頭から続いていなくても数える)。
    identical_messages: usize,
}

impl Compared {
    fn push(&mut self, difference: Value) {
        if self.first.is_none() {
            let mut at = Map::new();
            for key in ["section", "tool", "block", "message"] {
                if let Some(value) = difference.get(key) {
                    at.insert(key.to_owned(), value.clone());
                }
            }
            // params の違いは prefix の位置を持たない。
            if difference["section"] != "params" {
                self.first = Some(Value::Object(at));
            }
        }
        self.differences.push(difference);
    }

    fn summary(&self) -> Map<String, Value> {
        let mut summary = Map::new();
        summary.insert("record".to_owned(), json!("summary"));
        summary.insert("identical".to_owned(), json!(self.differences.is_empty()));
        summary.insert("differences".to_owned(), json!(self.differences.len()));
        summary.extend(self.counts.clone());
        summary.insert(
            "first_difference".to_owned(),
            self.first.clone().unwrap_or(Value::Null),
        );
        summary.insert(
            "matched_prefix".to_owned(),
            json!({
                "tools": self.tools_identical,
                "system": self.system_identical,
                "messages": self.leading_messages,
                "message_bytes": self.leading_message_bytes,
            }),
        );
        summary.insert(
            "identical_messages".to_owned(),
            json!(self.identical_messages),
        );
        summary
    }
}

/// 2 つの本文を比べる。
///
/// 比べる前に除くもの: `cache_control` (gateway と client が置き場所や寿命を
/// 書き換える)、top-level の `model` (gateway が解決後の名前に書き換える)。
/// 文字列の content と text block 1 つは同じものとして扱う。
fn compare(a: &Value, b: &Value) -> Compared {
    let mut compared = Compared::default();

    let tools_a = list(a.get("tools"))
        .into_iter()
        .map(strip)
        .collect::<Vec<_>>();
    let tools_b = list(b.get("tools"))
        .into_iter()
        .map(strip)
        .collect::<Vec<_>>();
    compared.tools_identical = sequence(&mut compared, "tools", "tool", &tools_a, &tools_b);

    let system_a = blocks(a.get("system"));
    let system_b = blocks(b.get("system"));
    compared.system_identical = sequence(&mut compared, "system", "block", &system_a, &system_b);

    let messages_a = list(a.get("messages"))
        .into_iter()
        .map(message)
        .collect::<Vec<_>>();
    let messages_b = list(b.get("messages"))
        .into_iter()
        .map(message)
        .collect::<Vec<_>>();
    count(
        &mut compared,
        "messages",
        messages_a.len(),
        messages_b.len(),
    );
    let mut leading = true;
    for (i, (ma, mb)) in messages_a.iter().zip(&messages_b).enumerate() {
        if serialized(ma) == serialized(mb) {
            compared.identical_messages += 1;
            if leading {
                compared.leading_messages += 1;
                compared.leading_message_bytes += serialized(ma).len();
            }
            continue;
        }
        leading = false;
        message_differences(&mut compared, i, ma, mb);
    }
    if messages_a.len() != messages_b.len() {
        compared.push(json!({
            "record": "difference",
            "section": "messages",
            "message": messages_a.len().min(messages_b.len()),
            "kind": "count",
            "a": messages_a.len(),
            "b": messages_b.len(),
        }));
    }

    params(&mut compared, a, b);
    compared
}

/// 数を要約に載せる。
fn count(compared: &mut Compared, section: &str, a: usize, b: usize) {
    compared
        .counts
        .insert(section.to_owned(), json!({"a": a, "b": b}));
}

/// `tools` / `system` のような、要素を番号で突き合わせる列を比べる。全部一致
/// すれば `true`。
fn sequence(
    compared: &mut Compared,
    section: &str,
    index_key: &str,
    a: &[Value],
    b: &[Value],
) -> bool {
    count(compared, section, a.len(), b.len());
    let mut intact = true;
    for (i, (ea, eb)) in a.iter().zip(b).enumerate() {
        if serialized(ea) == serialized(eb) {
            continue;
        }
        intact = false;
        let mut difference = Map::new();
        difference.insert("record".to_owned(), json!("difference"));
        difference.insert("section".to_owned(), json!(section));
        difference.insert(index_key.to_owned(), json!(i));
        element_difference(&mut difference, ea, eb);
        compared.push(Value::Object(difference));
    }
    if a.len() != b.len() {
        intact = false;
        let mut difference = Map::new();
        difference.insert("record".to_owned(), json!("difference"));
        difference.insert("section".to_owned(), json!(section));
        difference.insert(index_key.to_owned(), json!(a.len().min(b.len())));
        difference.insert("kind".to_owned(), json!("count"));
        difference.insert("a".to_owned(), json!(a.len()));
        difference.insert("b".to_owned(), json!(b.len()));
        compared.push(Value::Object(difference));
    }
    intact
}

/// 違う 2 つの要素 (block / tool) の違いの種類と、どの欄が違うか。
fn element_difference(difference: &mut Map<String, Value>, a: &Value, b: &Value) {
    let type_a = a.get("type");
    let type_b = b.get("type");
    if type_a != type_b {
        difference.insert("kind".to_owned(), json!("block_type"));
    } else {
        difference.insert("kind".to_owned(), json!("fields"));
        difference.insert("fields".to_owned(), Value::Array(fields(a, b)));
    }
    difference.insert("a".to_owned(), element(a));
    difference.insert("b".to_owned(), element(b));
}

/// 違う 2 つの message の違いを並べる (同じ番号どうし)。
fn message_differences(compared: &mut Compared, i: usize, a: &Value, b: &Value) {
    let role_a = a.get("role").cloned().unwrap_or(Value::Null);
    let role_b = b.get("role").cloned().unwrap_or(Value::Null);
    if role_a != role_b {
        compared.push(json!({
            "record": "difference",
            "section": "messages",
            "message": i,
            "kind": "role",
            "a": {"role": role_a, "bytes": serialized(a).len(), "hash": hash(a)},
            "b": {"role": role_b, "bytes": serialized(b).len(), "hash": hash(b)},
        }));
        return;
    }
    let base = |kind: &str| {
        let mut difference = Map::new();
        difference.insert("record".to_owned(), json!("difference"));
        difference.insert("section".to_owned(), json!("messages"));
        difference.insert("message".to_owned(), json!(i));
        difference.insert("role".to_owned(), role_a.clone());
        difference.insert("kind".to_owned(), json!(kind));
        difference
    };

    // message の欄のうち role と content 以外 (role:system の message の印など)。
    let outer = |m: &Value| {
        let mut m = m.as_object().cloned().unwrap_or_default();
        m.shift_remove("content");
        Value::Object(m)
    };
    let (outer_a, outer_b) = (outer(a), outer(b));
    if serialized(&outer_a) != serialized(&outer_b) {
        let mut difference = base("fields");
        difference.insert(
            "fields".to_owned(),
            Value::Array(fields(&outer_a, &outer_b)),
        );
        compared.push(Value::Object(difference));
    }

    let blocks_a = list(a.get("content"));
    let blocks_b = list(b.get("content"));
    if blocks_a.len() != blocks_b.len() {
        let mut difference = base("block_count");
        difference.insert("a".to_owned(), block_list(&blocks_a));
        difference.insert("b".to_owned(), block_list(&blocks_b));
        difference.insert(
            "text_joined".to_owned(),
            json!(joined(&blocks_a, &blocks_b)),
        );
        compared.push(Value::Object(difference));
    }
    for (j, (ba, bb)) in blocks_a.iter().zip(&blocks_b).enumerate() {
        if serialized(ba) == serialized(bb) {
            continue;
        }
        let mut difference = base("fields");
        difference.insert("block".to_owned(), json!(j));
        element_difference(&mut difference, ba, bb);
        compared.push(Value::Object(difference));
    }
}

/// block の数が違う時に、両側の block の並び (型と大きさ) を出す。
fn block_list(blocks: &[Value]) -> Value {
    json!({
        "blocks": blocks.len(),
        "types": blocks.iter().map(|b| b.get("type").cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
        "bytes": blocks.iter().map(|b| serialized(b).len()).collect::<Vec<_>>(),
    })
}

/// text block の text を順につなげた時に、両側が同じか。block の割り方だけの
/// 違いか、中身の違いかを見分けるため。
fn joined(a: &[Value], b: &[Value]) -> &'static str {
    let join = |blocks: &[Value]| {
        blocks
            .iter()
            .filter(|b| b.get("type") == Some(&json!("text")))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<String>()
    };
    let (ja, jb) = (join(a), join(b));
    if ja == jb {
        "same"
    } else if ja.split_whitespace().eq(jb.split_whitespace()) {
        "same_apart_from_whitespace"
    } else {
        "different"
    }
}

/// top-level の欄のうち `tools` / `system` / `messages` / `model` 以外の違い。
/// prefix の位置は持たないので、最初の違いの位置には数えない。
fn params(compared: &mut Compared, a: &Value, b: &Value) {
    const SKIP: [&str; 4] = ["tools", "system", "messages", "model"];
    let empty = Map::new();
    let oa = a.as_object().unwrap_or(&empty);
    let ob = b.as_object().unwrap_or(&empty);
    let keys = oa
        .keys()
        .chain(ob.keys().filter(|k| !oa.contains_key(*k)))
        .filter(|k| !SKIP.contains(&k.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        let (va, vb) = (oa.get(&key), ob.get(&key));
        if va.map(serialized) == vb.map(serialized) {
            continue;
        }
        let mut difference = json!({
            "record": "difference",
            "section": "params",
            "field": key,
            "kind": "value",
            "a": va.map(shape).unwrap_or(Value::Null),
            "b": vb.map(shape).unwrap_or(Value::Null),
        });
        if let (Some(va), Some(vb)) = (va, vb) {
            difference["fields"] = Value::Array(fields(va, vb));
        }
        compared.push(difference);
    }
}

/// 2 つの値のうち違う欄を、位置 (path) と両側の形で並べる。中身は出さない。
fn fields(a: &Value, b: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    walk(a, b, String::new(), &mut out);
    if out.len() > MAX_FIELDS {
        out.truncate(MAX_FIELDS);
        out.push(json!({"truncated": true}));
    }
    out
}

fn walk(a: &Value, b: &Value, path: String, out: &mut Vec<Value>) {
    if out.len() > MAX_FIELDS || serialized(a) == serialized(b) {
        return;
    }
    match (a, b) {
        (Value::Object(oa), Value::Object(ob)) => {
            // 同じ鍵の組が違う順に並んでいる。cache は bytes で比べるので、値が
            // 同じでも違いになる。
            let same_keys = oa.len() == ob.len() && oa.keys().all(|k| ob.contains_key(k));
            if same_keys && !oa.keys().eq(ob.keys()) {
                out.push(json!({"path": path, "key_order": true}));
            }
            for (key, va) in oa {
                let child = join_path(&path, key);
                match ob.get(key) {
                    Some(vb) => walk(va, vb, child, out),
                    None => out.push(json!({"path": child, "a": shape(va), "b": null})),
                }
            }
            for (key, vb) in ob.iter().filter(|(k, _)| !oa.contains_key(*k)) {
                out.push(json!({"path": join_path(&path, key), "a": null, "b": shape(vb)}));
            }
        }
        (Value::Array(xa), Value::Array(xb)) if xa.len() == xb.len() => {
            for (i, (va, vb)) in xa.iter().zip(xb).enumerate() {
                walk(va, vb, format!("{path}[{i}]"), out);
            }
        }
        (Value::String(sa), Value::String(sb)) => {
            // どこが違うかの見当 (末尾に足されたのか、頭か、途中か)。
            let same = |x: (u8, u8)| x.0 == x.1;
            let prefix = sa.bytes().zip(sb.bytes()).take_while(|&x| same(x)).count();
            let suffix = sa
                .bytes()
                .rev()
                .zip(sb.bytes().rev())
                .take_while(|&x| same(x))
                .count();
            out.push(json!({
                "path": path,
                "a": shape(a),
                "b": shape(b),
                "common_prefix_bytes": prefix,
                "common_suffix_bytes": suffix,
            }));
        }
        _ => out.push(json!({"path": path, "a": shape(a), "b": shape(b)})),
    }
}

fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// 値の形 (JSON の型と大きさ)。中身は出さない。
fn shape(v: &Value) -> Value {
    let kind = match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    let mut shape = json!({"type": kind, "bytes": serialized(v).len()});
    match v {
        Value::Array(items) => shape["len"] = json!(items.len()),
        Value::Object(map) => shape["len"] = json!(map.len()),
        // `bytes` は JSON に書いた大きさ (引用符とエスケープ込み)。文字列そのものの
        // 大きさは `text_bytes` で、`common_prefix_bytes` と同じ単位。
        Value::String(text) => shape["text_bytes"] = json!(text.len()),
        _ => {}
    }
    shape
}

/// 1 つの block / tool の要約 (型・大きさ・hash)。
fn element(v: &Value) -> Value {
    json!({
        "type": v.get("type").cloned().unwrap_or(Value::Null),
        "bytes": serialized(v).len(),
        "hash": hash(v),
    })
}

fn serialized(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn hash(v: &Value) -> String {
    Sha256::digest(serialized(v).as_bytes())
        .iter()
        .take(HASH_DIGITS / 2)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn list(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// `cache_control` を除く (block / tool の直下)。
fn strip(mut v: Value) -> Value {
    if let Some(map) = v.as_object_mut() {
        map.shift_remove("cache_control");
    }
    v
}

/// content (または system) を block の列にする。文字列は text block 1 つとみなし、
/// 各 block から `cache_control` を除く。tool_result の中の content も同じ形にする。
fn blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        Some(Value::Array(items)) => items.iter().cloned().map(block).collect(),
        Some(other) => vec![other.clone()],
    }
}

fn block(v: Value) -> Value {
    let mut v = strip(v);
    if v.get("type") == Some(&json!("tool_result"))
        && let Some(content) = v.get("content").filter(|c| !c.is_null())
    {
        let inner = blocks(Some(content));
        v["content"] = Value::Array(inner);
    }
    v
}

/// message の content を block の列にした形。
fn message(mut m: Value) -> Value {
    if let Some(map) = m.as_object_mut() {
        let content = blocks(map.get("content"));
        if map.contains_key("content") {
            map.insert("content".to_owned(), Value::Array(content));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn differences(a: &Value, b: &Value) -> Vec<Value> {
        compare(a, b).differences
    }

    /// 親が送った形の会話 (今回の 5 か所の違いの、親の側)。
    fn parent() -> Value {
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 32000,
            "system": [
                {"type": "text", "text": "billing header"},
                {"type": "text", "text": "you are a helper", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ],
            "tools": [
                {"name": "permit", "description": "ask", "input_schema": {"type": "object"}},
                {"name": "Bash", "description": "run", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "system", "content": "session start"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "id": "toolu_1", "name": "permit", "input": {"commands": "[\"ls\"]", "why": "look"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"},
                    {"type": "text", "text": "first input"},
                    {"type": "text", "text": "second input"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_2", "name": "permit", "input": {"commands": "[\"pwd\"]"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": [
                        {"type": "tool_reference", "tool_name": "Bash"}
                    ]}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "next", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]}
            ]
        })
    }

    /// 同じ会話を transcript から組み立て直した fork の形。親と 5 か所違う:
    /// role:system の message の `output_config`、tool_use の `input` の形 2 か所、
    /// turn の途中の入力の text block の数 (と前の block の末尾)、ToolSearch の結果の
    /// block の数。
    fn fork() -> Value {
        json!({
            "model": "claude-opus-5-5[1m]",
            "max_tokens": 128000,
            "system": [
                {"type": "text", "text": "billing header", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "you are a helper", "cache_control": {"type": "ephemeral"}}
            ],
            "tools": [
                {"name": "permit", "description": "ask", "input_schema": {"type": "object"}},
                {"name": "Bash", "description": "run", "input_schema": {"type": "object"}}
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "system", "content": "session start", "output_config": {"effort": "medium"}},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "id": "toolu_1", "name": "permit", "input": {"commands": ["ls"], "why": "look"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "ok"}]},
                    {"type": "text", "text": "first input\n"},
                    {"type": "text", "text": "second input"},
                    {"type": "text", "text": "third input"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_2", "name": "permit", "input": {"commands": ["pwd"]}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": [
                        {"type": "tool_reference", "tool_name": "Bash"},
                        {"type": "text", "text": "Tool loaded."}
                    ]}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
                {"role": "user", "content": [{"type": "text", "text": "next"}]},
                {"role": "user", "content": [{"type": "text", "text": "side question"}]}
            ]
        })
    }

    /// 本文の中身 (text・thinking・tool の入力の値) は出力のどこにも出ない。
    fn assert_no_body(lines: &[Value]) {
        let printed = lines.iter().map(serialized).collect::<String>();
        for secret in [
            "hello",
            "session start",
            "hmm",
            "sig\"",
            "first input",
            "second input",
            "third input",
            "Tool loaded",
            "[\\\"ls\\\"]",
            "pwd",
            "look",
            "done",
            "side question",
            "you are a helper",
            "billing header",
        ] {
            assert!(!printed.contains(secret), "`{secret}` leaked: {printed}");
        }
    }

    /// 今回の 5 か所の違いが、全部並ぶ。cache_control・model・文字列の content
    /// と text block 1 つの違いは出ない。
    #[test]
    fn every_divergence_of_the_fork_is_listed() {
        let found = differences(&parent(), &fork());
        let at = |message: usize| {
            found
                .iter()
                .filter(|d| d["section"] == "messages" && d["message"] == message)
                .collect::<Vec<_>>()
        };

        // 1: role:system の message の印。
        let system_message = at(1);
        assert_eq!(system_message.len(), 1, "{found:#?}");
        assert_eq!(system_message[0]["kind"], "fields");
        assert_eq!(system_message[0]["fields"][0]["path"], "output_config");
        assert_eq!(system_message[0]["fields"][0]["a"], Value::Null);

        // 2 と 4: tool_use の input の形 (文字列と配列)。
        for message in [2, 4] {
            let tool_use = at(message);
            assert_eq!(tool_use.len(), 1, "{message}: {found:#?}");
            assert_eq!(tool_use[0]["kind"], "fields");
            assert_eq!(tool_use[0]["a"]["type"], "tool_use");
            let field = &tool_use[0]["fields"][0];
            assert_eq!(field["path"], "input.commands");
            assert_eq!(field["a"]["type"], "string");
            assert_eq!(field["b"]["type"], "array");
        }

        // 3: turn の途中の入力。block の数と、前の block の末尾。
        let mid_turn = at(3);
        let count = mid_turn
            .iter()
            .find(|d| d["kind"] == "block_count")
            .unwrap();
        assert_eq!(count["a"]["blocks"], 3);
        assert_eq!(count["b"]["blocks"], 4);
        assert_eq!(
            count["b"]["types"],
            json!(["tool_result", "text", "text", "text"])
        );
        assert_eq!(count["text_joined"], "different");
        let tail = mid_turn.iter().find(|d| d["kind"] == "fields").unwrap();
        assert_eq!(tail["block"], 1);
        assert_eq!(tail["fields"][0]["path"], "text");
        // 「first input」(11 bytes) の末尾に 1 byte 足されている。
        assert_eq!(tail["fields"][0]["common_prefix_bytes"], 11);
        assert_eq!(tail["fields"][0]["common_suffix_bytes"], 0);
        assert_eq!(tail["fields"][0]["a"]["text_bytes"], 11);
        assert_eq!(tail["fields"][0]["b"]["text_bytes"], 12);
        assert_eq!(tail["fields"][0]["b"]["bytes"], 15);

        // 5: ToolSearch の結果に足された block。
        let tool_result = at(5);
        assert_eq!(tool_result.len(), 1, "{found:#?}");
        let field = &tool_result[0]["fields"][0];
        assert_eq!(field["path"], "content");
        assert_eq!(field["a"]["len"], 1);
        assert_eq!(field["b"]["len"], 2);

        // fork が末尾に足した 1 件は、数の違いとして 1 行。
        let tail_count = at(8);
        assert_eq!(tail_count[0]["kind"], "count");
        assert_eq!(tail_count[0]["a"], 8);
        assert_eq!(tail_count[0]["b"], 9);

        // tools / system には違いが出ない (cache_control は除く)。0 / 6 / 7 も同じ。
        assert!(
            found
                .iter()
                .all(|d| d["section"] != "tools" && d["section"] != "system"),
            "{found:#?}"
        );
        for same in [0, 6, 7] {
            assert!(at(same).is_empty(), "{same}: {found:#?}");
        }

        // top-level の model は比べない。max_tokens は params の違いとして出る。
        let params = found
            .iter()
            .filter(|d| d["section"] == "params")
            .map(|d| d["field"].clone())
            .collect::<Vec<_>>();
        assert_eq!(params, vec![json!("max_tokens")]);
    }

    /// 要約は、cache の順で最初に違った位置と、そこまで一致した messages の数を言う。
    #[test]
    fn the_summary_says_how_far_the_prefix_matched() {
        let compared = compare(&parent(), &fork());
        let summary = compared.summary();
        assert_eq!(summary["identical"], false);
        assert_eq!(
            summary["first_difference"],
            json!({"section": "messages", "message": 1})
        );
        assert_eq!(
            summary["matched_prefix"],
            json!({"tools": true, "system": true, "messages": 1, "message_bytes": 58})
        );
        // 1 / 2 / 3 / 4 / 5 が違い、0 / 6 / 7 が同じ。
        assert_eq!(summary["identical_messages"], 3);
        assert_eq!(summary["messages"], json!({"a": 8, "b": 9}));
        assert_eq!(summary["tools"], json!({"a": 2, "b": 2}));
        assert_eq!(summary["differences"], json!(compared.differences.len()));
    }

    /// 違いの無い 2 本は「全部一致」。cache_control と model だけ違っても同じ。
    #[test]
    fn two_equal_bodies_are_identical() {
        let summary = compare(&parent(), &parent()).summary();
        assert_eq!(summary["identical"], true);
        assert_eq!(summary["differences"], 0);
        assert_eq!(summary["first_difference"], Value::Null);
        assert_eq!(summary["matched_prefix"]["messages"], 8);
        assert_eq!(summary["identical_messages"], 8);

        let mut moved = parent();
        moved["model"] = json!("opus");
        moved["system"][1]
            .as_object_mut()
            .unwrap()
            .shift_remove("cache_control");
        moved["messages"][7]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        moved["messages"][2]["content"][1]["cache_control"] = json!({"type": "ephemeral"});
        let summary = compare(&parent(), &moved).summary();
        assert_eq!(summary["identical"], true, "{summary:#?}");
    }

    /// 鍵の順だけが違う値は「形の違い」として出る (cache は bytes で比べる)。
    #[test]
    fn a_different_key_order_is_a_difference() {
        let mut reordered = parent();
        reordered["messages"][2]["content"][1]["input"] =
            json!({"why": "look", "commands": "[\"ls\"]"});
        let found = differences(&parent(), &reordered);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(
            found[0]["fields"][0],
            json!({"path": "input", "key_order": true})
        );
    }

    /// tools / system の違い・数の違いは、位置 (番号) つきで、cache の順に先に出る。
    #[test]
    fn tools_and_system_differences_come_first() {
        let mut other = parent();
        other["tools"][1]["description"] = json!("run a command");
        other["system"] = json!("one string");
        let compared = compare(&parent(), &other);
        let found = &compared.differences;
        assert_eq!(found[0]["section"], "tools");
        assert_eq!(found[0]["tool"], 1);
        assert_eq!(found[0]["fields"][0]["path"], "description");
        assert_eq!(found[1]["section"], "system");
        assert_eq!(found[1]["block"], 0);
        assert_eq!(found[2]["kind"], "count");
        assert_eq!(found[2]["a"], 2);
        assert_eq!(found[2]["b"], 1);
        let summary = compared.summary();
        assert_eq!(
            summary["first_difference"],
            json!({"section": "tools", "tool": 1})
        );
        // tools と system が違っても、messages がどこまで同じかは数える。
        assert_eq!(summary["matched_prefix"]["tools"], false);
        assert_eq!(summary["matched_prefix"]["system"], false);
        assert_eq!(summary["matched_prefix"]["messages"], 8);
    }

    /// role が違えば block には降りない。block の型が違えば block_type。
    #[test]
    fn a_role_or_a_block_type_is_named() {
        let mut other = parent();
        other["messages"][0]["role"] = json!("assistant");
        other["messages"][6]["content"][0] = json!({"type": "thinking", "thinking": "x"});
        let found = differences(&parent(), &other);
        assert_eq!(found[0]["kind"], "role");
        assert_eq!(found[0]["a"]["role"], "user");
        assert_eq!(found[0]["b"]["role"], "assistant");
        assert_eq!(found[1]["kind"], "block_type");
        assert_eq!(found[1]["a"]["type"], "text");
        assert_eq!(found[1]["b"]["type"], "thinking");
        assert_eq!(found.len(), 2, "{found:#?}");
    }

    /// block の割り方だけが違う時は text_joined が same になる。
    #[test]
    fn a_split_alone_is_told_apart_from_a_change() {
        let one = vec![json!({"type": "text", "text": "ab cd"})];
        let two = vec![
            json!({"type": "text", "text": "ab"}),
            json!({"type": "text", "text": " cd"}),
        ];
        let spaced = vec![
            json!({"type": "text", "text": "ab\n"}),
            json!({"type": "text", "text": "cd"}),
        ];
        assert_eq!(joined(&one, &two), "same");
        assert_eq!(joined(&one, &spaced), "same_apart_from_whitespace");
        assert_eq!(
            joined(&one, &[json!({"type": "text", "text": "ab"})]),
            "different"
        );
    }

    /// 1 つの違いの中の欄は上限で切り、切ったことを言う。
    #[test]
    fn a_long_list_of_fields_is_cut() {
        let a =
            json!({"input": (0..40).map(|i| (format!("k{i}"), json!(i))).collect::<Map<_, _>>()});
        let b = json!({"input": (0..40).map(|i| (format!("k{i}"), json!(i + 1))).collect::<Map<_, _>>()});
        let found = fields(&a, &b);
        assert_eq!(found.len(), MAX_FIELDS + 1);
        assert_eq!(found[MAX_FIELDS], json!({"truncated": true}));
    }

    /// 出力の全行 (入力・違い・要約) に本文の中身が出ない。受けた形と送った形を
    /// 比べた時は、書き換えが違いとして出ることを要約に書く。
    #[test]
    fn the_report_never_prints_the_body() {
        let side = |body: Value, form: Form| Side {
            body,
            about: Map::new(),
            form,
        };
        let lines = report(&side(parent(), Form::Sent), &side(fork(), Form::Received));
        assert_no_body(&lines);
        assert_eq!(lines[0]["record"], "input");
        assert_eq!(lines[0]["form"], "sent");
        assert_eq!(lines[1]["form"], "received");
        let summary = lines.last().unwrap();
        assert_eq!(summary["record"], "summary");
        assert!(
            summary["note"]
                .as_str()
                .unwrap()
                .contains("request_rewrites")
        );

        let same_form = report(
            &side(parent(), Form::Received),
            &side(fork(), Form::Received),
        );
        assert!(same_form.last().unwrap().get("note").is_none());
    }

    /// 引数: パス 2 つ、または `--kept` とパス 1 つ。何も無ければ help。
    #[test]
    fn it_takes_two_bodies_or_one_and_a_kept_copy() {
        assert_eq!(parse(&args(&[])).unwrap(), None);
        assert_eq!(
            parse(&args(&["a.body", "b.body"])).unwrap(),
            Some(Inputs {
                a: Source::File("a.body".into()),
                b: Source::File("b.body".into()),
            })
        );
        assert_eq!(
            parse(&args(&[
                "b.body",
                "--kept=s1",
                "--prefix",
                "ab12",
                "--unit",
                "u"
            ]))
            .unwrap(),
            Some(Inputs {
                a: Source::Kept {
                    session_id: "s1".to_owned(),
                    prefix: Some("ab12".to_owned()),
                    unit: Some("u".to_owned()),
                },
                b: Source::File("b.body".into()),
            })
        );
        // `--` の後ろはパスとして読む。
        assert_eq!(
            parse(&args(&["--", "--a", "b"])).unwrap(),
            Some(Inputs {
                a: Source::File("--a".into()),
                b: Source::File("b".into()),
            })
        );
        assert!(parse(&args(&["a"])).is_err());
        assert!(parse(&args(&["a", "b", "c"])).is_err());
        assert!(parse(&args(&["--kept", "s"])).is_err());
        assert!(parse(&args(&["--kept", "s", "a", "b"])).is_err());
        assert!(parse(&args(&["a", "b", "--prefix", "x"])).is_err());
        assert!(parse(&args(&["a", "b", "--unit", "x"])).is_err());
        assert!(parse(&args(&["a", "b", "--nope"])).is_err());
        assert!(parse(&args(&["a", "--kept"])).is_err());
    }

    /// ダンプの本文は隣の試行のメタから書き換えを拾う (番号の一番大きい試行)。
    #[test]
    fn a_dumped_body_brings_its_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let body = dir.path().join("1792000000000-7.request.body");
        std::fs::write(&body, serialized(&parent())).unwrap();
        std::fs::write(
            dir.path().join("1792000000000-7.1.response.json"),
            r#"{"request_rewrites":{"model":{"received":"opus","sent":"claude-opus-5-5"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("1792000000000-7.2.response.json"),
            r#"{"request_rewrites":{"cache_ttl_secs":3600}}"#,
        )
        .unwrap();
        // 別の 1 本のメタは拾わない。
        std::fs::write(
            dir.path().join("1792000000000-70.9.response.json"),
            r#"{"request_rewrites":{"other":true}}"#,
        )
        .unwrap();
        let side = load_file(&body).unwrap();
        assert_eq!(side.form, Form::Received);
        assert_eq!(side.about["attempt"], 2);
        assert_eq!(
            side.about["request_rewrites"],
            json!({"cache_ttl_secs": 3600})
        );
        assert_eq!(side.body, parent());

        // ダンプでない名前の本文は、書き換えを探さない。
        let plain = dir.path().join("plain.json");
        std::fs::write(&plain, "{}").unwrap();
        assert!(!load_file(&plain).unwrap().about.contains_key("attempt"));

        // JSON の object でなければ断る。
        let broken = dir.path().join("broken.request.body");
        std::fs::write(&broken, "not json").unwrap();
        assert!(load_file(&broken).is_err());
        std::fs::write(&broken, "[1]").unwrap();
        assert!(load_file(&broken).is_err());
        assert!(load_file(&dir.path().join("missing")).is_err());
    }

    fn keep(store: &Store, session_id: &str, prefix: &str, body: Value) {
        let kept = llm_gateway::cache::keepalive::Kept {
            session_id: session_id.to_owned(),
            prefix: prefix.to_owned(),
            ns: "lab".to_owned(),
            model: "claude-opus-5-5".to_owned(),
            route: "r".to_owned(),
            body,
            headers: vec![],
            path: "/v1/messages".to_owned(),
            query: None,
            shape: llm_gateway::egress::RequestShape::Messages,
            fires_at_ms: 0,
            expires_at_ms: 0,
            horizon_end_ms: 0,
            since_ms: 0,
            count: 0,
            cache_notice: None,
        };
        assert!(store.save(&kept));
    }

    /// 控えは会話の id で引き、系列が複数あれば --prefix で選ばせる。
    #[test]
    fn a_kept_copy_is_found_by_its_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        keep(&store, "conv-1", "aa11", parent());

        let side = load_kept(&store, "conv-1", None).unwrap();
        assert_eq!(side.form, Form::Sent);
        assert_eq!(side.body, parent());
        assert_eq!(side.about["prefix"], "aa11");
        assert_eq!(side.about["bytes"], json!(serialized(&parent()).len()));
        assert_no_body(&[Value::Object(side.about.clone())]);

        let e = load_kept(&store, "conv-2", None).unwrap_err();
        assert_eq!(e.kind(), "no_kept_copy");

        keep(&store, "conv-1", "bb22", fork());
        let e = load_kept(&store, "conv-1", None).unwrap_err();
        assert_eq!(e.kind(), "prefix_required");
        assert!(e.to_json().contains("aa11") && e.to_json().contains("bb22"));
        assert_eq!(
            load_kept(&store, "conv-1", Some("bb22")).unwrap().body,
            fork()
        );
        let e = load_kept(&store, "conv-1", Some("cc33")).unwrap_err();
        assert_eq!(e.kind(), "no_kept_copy");
        assert!(e.to_json().contains("aa11"), "{}", e.to_json());
    }

    /// help の文面と受け付ける引数が揃っている (cli-design-preferences)。
    #[test]
    fn the_help_documents_what_it_takes() {
        for option in [
            "diff <a> <b>",
            "diff --kept <session_id> <b>",
            "--kept <session_id>",
            "--prefix <prefix>",
        ] {
            assert!(help::DUMP.contains(option), "the help misses {option}");
        }
        assert_eq!(run(&[]).unwrap(), ExitCode::SUCCESS);
    }
}
