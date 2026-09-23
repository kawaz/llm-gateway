//! 認証情報 × モデル × 日のトークン使用量 (DR-0011)。
//!
//! 集計を持っているのは走っている側なので、ここは聞いて整形するだけ
//! (usage と同じ形)。過去日の分もディスクに残っているので、聞かれた側は
//! 再集計せずに返す。

use std::collections::BTreeSet;
use std::process::ExitCode;

use llm_gateway::daemon::registry::Registry;
use llm_gateway::metering::TokenKind;
use llm_gateway::stats::{Counters, InputBasis, Report, UNKNOWN_ORIGIN};

use crate::destination;
use crate::failure::Failure;
use crate::help;
use crate::options::{split, take_value};
use crate::text::{thousands, width};

/// `stats` に渡された内容。
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    days: usize,
    unit: Option<String>,
    by: By,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            days: 7,
            unit: None,
            by: By::Route,
        }
    }
}

/// 内訳をどの軸で割るか (DR-0029)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// 認証情報 × モデル。既定。
    Route,
    /// 出した側 (`main` / `sub` / `keepalive` …) も割る。
    Origin,
}

pub fn run(args: &[String]) -> Result<ExitCode, Failure> {
    if help::wanted(args) {
        print!("{}", help::TOP);
        return Ok(ExitCode::SUCCESS);
    }
    let parsed = parse(args)?;
    let target = destination::resolve(&Registry::open(), parsed.unit.as_deref())?;
    let report: Report = destination::ask(&target, "stats", &format!("?days={}", parsed.days))?;
    print!("{}", render(&report, parsed.by));
    Ok(ExitCode::SUCCESS)
}

fn parse(args: &[String]) -> Result<Args, Failure> {
    let mut parsed = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("days", inline)) => {
                parsed.days = take_days(&take_value("days", inline, &mut it)?)?;
            }
            Some(("unit", inline)) => parsed.unit = Some(take_value("unit", inline, &mut it)?),
            Some(("by", inline)) => parsed.by = take_by(&take_value("by", inline, &mut it)?)?,
            _ => return Err(Failure::from(format!("could not understand `{arg}`"))),
        }
    }
    Ok(parsed)
}

/// `--days` の値を読む。
///
/// 上限で抑えるのは、日数を秒に直す掛け算が桁あふれするため (`llm_gateway::stats`
/// の `MAX_DAYS`)。全期間を見たいなら `--days 0`。
fn take_days(raw: &str) -> Result<usize, Failure> {
    raw.parse::<usize>()
        .map(|days| days.min(llm_gateway::stats::MAX_DAYS))
        .map_err(|_| Failure::from(format!("--days takes a whole number, got `{raw}`")))
}

/// `--by` の値を読む。
///
/// 受ける語を並べて断るのは、黙って既定に落とすと「割ったつもり」の相手が
/// 別の表を読むため (`--days` と同じ構え)。
fn take_by(raw: &str) -> Result<By, Failure> {
    match raw {
        "origin" => Ok(By::Origin),
        other => Err(Failure::from(format!("--by takes `origin`, got `{other}`"))),
    }
}

/// 日次集計を人が読む形に整える。
///
/// 1 つの桁組に全部を並べる。日ごとに合計を先に出し、その下に認証情報 ×
/// モデルの内訳を字下げして置く。合計を先にするのは「その日いくら使ったか」が
/// 一番よく見る数字で、内訳は当たりを付けた後に見るため。
///
/// 桁は全日で揃える。日ごとに幅が変わると、縦に並べて比べられない。
fn render(report: &Report, by: By) -> String {
    if report.days.is_empty() {
        return "No usage recorded yet.\n\
                Nothing has been forwarded through the gateway, \
                or the stats directory is empty.\n"
            .to_owned();
    }

    // 先に全行を組み立てる。幅は全体を見てからでないと決まらない。
    let mut lines: Vec<Line> = Vec::new();
    // 新しい日を上に出す。見たいのは直近。
    let mut all_days = Counters::default();
    let mut all_days_basis = InputBasis::Fresh;
    for (date, day) in report.days.iter().rev() {
        let mut total = Counters::default();
        // 揃えていない行が 1 つでも混ざれば、合計も揃っていない。
        let mut basis = InputBasis::Fresh;
        for models in day.credentials.values() {
            for entry in models.values() {
                total.merge(&entry.counters);
                if entry.input_basis == InputBasis::AsRecorded {
                    basis = InputBasis::AsRecorded;
                }
            }
        }
        all_days.merge(&total);
        if basis == InputBasis::AsRecorded {
            all_days_basis = InputBasis::AsRecorded;
        }
        lines.push((format!("{date} {TOTAL_LABEL}"), total, day.total_usd, basis));

        // 内訳の行。素性で割るときは素性を先頭に置き、ラベル順で並べ直す
        // (`keepalive` の行が縦に固まって読める)。
        let mut breakdown: Vec<Line> = Vec::new();
        for (cred, models) in &day.credentials {
            for (model, entry) in models {
                match by {
                    By::Route => breakdown.push((
                        format!("  {cred} {model}"),
                        entry.counters.clone(),
                        entry.usd,
                        entry.input_basis,
                    )),
                    // 素性を知らない gateway が返した報告では内訳が空。
                    // 行を落とすと合計と内訳が食い違うので、言えることを
                    // そのまま名乗らせる。
                    By::Origin if entry.origins.is_empty() => breakdown.push((
                        format!("  {UNKNOWN_ORIGIN} {cred} {model}"),
                        entry.counters.clone(),
                        entry.usd,
                        entry.input_basis,
                    )),
                    By::Origin => breakdown.extend(entry.origins.iter().map(|(origin, slice)| {
                        (
                            format!("  {origin} {cred} {model}"),
                            slice.counters.clone(),
                            slice.usd,
                            slice.input_basis,
                        )
                    })),
                }
            }
        }
        breakdown.sort_by(|(left, ..), (right, ..)| left.cmp(right));
        lines.extend(breakdown);
    }
    // 全期間の合計を最後に置く。日ごとの合計と同じ桁組に並ぶので、
    // 「今月いくら使ったか」を表の下端で読める。
    lines.push((
        TOTAL_LABEL.to_owned(),
        all_days,
        report.total_usd,
        all_days_basis,
    ));

    // 列は記録に現れた区分だけ並べる。区分は provider ごとに違うので
    // (DR-0014 §4)、固定の列を並べると知らない区分が表から消える。
    let kinds = kinds_of(&lines);
    let headers: Vec<&str> = std::iter::once(REQUESTS_HEADER)
        .chain(kinds.iter().map(header_of))
        .collect();

    let label_width = lines.iter().map(|(l, ..)| width(l)).max().unwrap_or(0);
    let cell_width = lines
        .iter()
        .flat_map(|(_, c, ..)| columns(c, &kinds))
        .map(|n| thousands(n).len())
        .max()
        .unwrap_or(0)
        .max(headers.iter().map(|h| h.len()).max().unwrap_or(0));
    let usd_width = lines
        .iter()
        .map(|(_, _, usd, _)| usd_cell(*usd).len())
        .chain(std::iter::once(USD_HEADER.len()))
        .max()
        .unwrap_or(0);

    // 揃えられなかった input に付ける印の桁。1 つも無ければ桁ごと取らない。
    let marked = lines
        .iter()
        .any(|(.., basis)| *basis == InputBasis::AsRecorded)
        && kinds.contains(&TokenKind::input());
    let input_column = kinds
        .iter()
        .position(|kind| *kind == TokenKind::input())
        .map(|i| i + 1);

    // 見出しは 1 度だけ。日ごとに挟むと、行数の少ない日ほど見出しで埋まる。
    let mut out = " ".repeat(label_width);
    for (i, head) in headers.iter().enumerate() {
        out.push_str(&format!(" {head:>cell_width$}"));
        if marked && Some(i) == input_column {
            out.push(' ');
        }
    }
    out.push_str(&format!(" {USD_HEADER:>usd_width$}\n"));

    let mut previous_day_ended = false;
    for (label, counters, usd, basis) in &lines {
        // 日の切り替わりで 1 行空ける。合計行は字下げが無いので見分けられる。
        if !label.starts_with(' ') && previous_day_ended {
            out.push('\n');
        }
        previous_day_ended = true;

        out.push_str(label);
        out.push_str(&" ".repeat(label_width.saturating_sub(width(label))));
        let mark = match (marked, basis) {
            (false, _) => None,
            (true, InputBasis::AsRecorded) => Some('*'),
            (true, InputBasis::Fresh) => Some(' '),
        };
        out.push_str(&row(counters, &kinds, cell_width, input_column.zip(mark)));
        out.push_str(&format!(" {:>usd_width$}\n", usd_cell(*usd)));
    }
    if marked {
        out.push_str(AS_RECORDED_NOTE);
    }
    out
}

/// `*` の脚注。どの数が揃っていないのか、なぜかを 1 行で言う。
const AS_RECORDED_NOTE: &str = "\n* input as reported by upstream: the model is not in the price table, \
     so cached tokens could not be taken out and may be included.\n";

/// 表示用の 1 行。`(ラベル, 集計, USD, input の数え方)`。
type Line = (String, Counters, Option<f64>, InputBasis);

/// USD の欄。単価表に無いモデルは `-`。
///
/// 小数 4 桁で止めるのは、1 リクエストが 0.1 セント未満になることがあり、
/// 2 桁だと内訳が全部 `0.00` に潰れるため。
fn usd_cell(usd: Option<f64>) -> String {
    usd.map_or_else(|| "-".to_owned(), |v| format!("{v:.4}"))
}

/// 合計行の印。内訳の行と見分けられればよい。
const TOTAL_LABEL: &str = "total";

/// 本数の見出し。トークンの区分より前に置く。
const REQUESTS_HEADER: &str = "reqs";

/// トークン数の右に置く金額の見出し。単位を書くのは、桁だけでは
/// トークン数と見分けが付かないため。
const USD_HEADER: &str = "usd";

/// 見慣れた並び。ここに挙げた区分を先にこの順で出し、残りは名前順で続ける。
///
/// 並べ替えの都合なので、ここに無い区分も落とさない (落とすと、その provider の
/// 消費が表から消える)。
const KIND_ORDER: [&str; 7] = [
    TokenKind::INPUT_NAME,
    TokenKind::OUTPUT_NAME,
    TokenKind::INPUT_CACHE_CREATION_NAME,
    TokenKind::INPUT_CACHE_CREATION_1H_NAME,
    TokenKind::INPUT_CACHE_CREATION_5M_NAME,
    TokenKind::INPUT_CACHE_READ_NAME,
    TokenKind::OUTPUT_REASONING_NAME,
];

/// 表に出す区分を、並べる順で返す。
fn kinds_of(lines: &[Line]) -> Vec<TokenKind> {
    let seen: BTreeSet<TokenKind> = lines
        .iter()
        .flat_map(|(_, c, ..)| c.tokens.tokens.keys().cloned())
        .collect();

    let mut ordered: Vec<TokenKind> = KIND_ORDER
        .iter()
        .map(|name| TokenKind::new(*name))
        .filter(|kind| seen.contains(kind))
        .collect();
    ordered.extend(
        seen.into_iter()
            .filter(|kind| !KIND_ORDER.contains(&kind.as_str())),
    );
    ordered
}

/// 区分の見出し。桁が広がらないよう、標準区分は短い名前で出す。
///
/// 知らない区分は名前をそのまま見出しにする。短縮した名前を勝手に付けると、
/// どの区分を見ているのか読み手に分からない。
fn header_of(kind: &TokenKind) -> &str {
    match kind.as_str() {
        TokenKind::INPUT_CACHE_CREATION_NAME => "cache_w",
        // 内訳は親のすぐ隣に立つので、TTL だけを見出しにする。
        TokenKind::INPUT_CACHE_CREATION_1H_NAME => "w_1h",
        TokenKind::INPUT_CACHE_CREATION_5M_NAME => "w_5m",
        TokenKind::INPUT_CACHE_READ_NAME => "cache_r",
        TokenKind::OUTPUT_REASONING_NAME => "reasoning",
        other => other,
    }
}

/// 1 行に並べる数。先頭は本数、続けて区分ごとのトークン数。
fn columns(c: &Counters, kinds: &[TokenKind]) -> Vec<u64> {
    std::iter::once(c.requests)
        .chain(kinds.iter().map(|kind| c.tokens.get(kind).unwrap_or(0)))
        .collect()
}

/// 数を桁揃えで 1 行に並べる。見出しと同じ幅で、先頭に区切りの 1 桁を置く。
///
/// `mark` は `(何列目か, 印)`。その列の数の直後に印の 1 桁を足す。
fn row(
    c: &Counters,
    kinds: &[TokenKind],
    cell_width: usize,
    mark: Option<(usize, char)>,
) -> String {
    let mut out = String::new();
    for (i, n) in columns(c, kinds).iter().enumerate() {
        out.push_str(&format!(" {:>cell_width$}", thousands(*n)));
        if let Some((column, mark)) = mark
            && column == i
        {
            out.push(mark);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn counters(reqs: u64, input: u64, output: u64) -> Counters {
        let mut tokens = llm_gateway::metering::TokenUsage::default();
        tokens.set(TokenKind::input(), input);
        tokens.set(TokenKind::output(), output);
        Counters {
            requests: reqs,
            tokens,
        }
    }

    /// 既定の見え方 (認証情報 × モデル) で整形する。
    fn render_routes(report: &Report) -> String {
        render(report, By::Route)
    }

    /// 試験で書く 1 行。`(認証情報, モデル, 集計)`。
    type Row<'a> = (&'a str, &'a str, Counters);

    fn stats_report(days: &[(&str, &[Row<'_>])]) -> Report {
        // 単価表を通した後の形を作る。ここで金額まで計算しておくと、
        // 表示側の試験が単価表の中身に引きずられない。
        let mut by_date = serde_json::Map::new();
        let mut grand: Option<f64> = None;
        for (date, rows) in days {
            let mut creds = serde_json::Map::new();
            let mut day_total: Option<f64> = None;
            for (cred, model, c) in *rows {
                let usd = llm_gateway::preset::pricing::for_model(model).map(|p| p.cost(&c.tokens));
                if let Some(usd) = usd {
                    day_total = Some(day_total.unwrap_or(0.0) + usd);
                }
                let mut entry = serde_json::to_value(c).unwrap();
                if let Some(usd) = usd {
                    entry["usd"] = serde_json::json!(usd);
                    entry["input_basis"] = serde_json::json!("fresh");
                }
                creds
                    .entry((*cred).to_owned())
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .unwrap()
                    .insert((*model).to_owned(), entry);
            }
            if let Some(total) = day_total {
                grand = Some(grand.unwrap_or(0.0) + total);
            }
            by_date.insert(
                (*date).to_owned(),
                serde_json::json!({"credentials": creds, "total_usd": day_total}),
            );
        }
        // JSON を経由するのは、走っている側が返す形をそのまま読めることも
        // 一緒に確かめられるため。
        serde_json::from_value(serde_json::json!({
            "generated_at": 1_785_326_400_000_i64,
            "days": by_date,
            "total_usd": grand,
        }))
        .unwrap()
    }

    /// 素性ごとの内訳を持つ 1 日分の報告 (DR-0029)。
    ///
    /// 走っている側と同じく、モデルの行は内訳の和で、額は同じ単価で出す。
    fn report_with_origins(rows: &[(&str, &str, &str, Counters)]) -> Report {
        let mut creds = serde_json::Map::new();
        let mut day_total: Option<f64> = None;
        for (cred, model, origin, c) in rows {
            let usd = llm_gateway::preset::pricing::for_model(model).map(|p| p.cost(&c.tokens));
            if let Some(usd) = usd {
                day_total = Some(day_total.unwrap_or(0.0) + usd);
            }
            let entry = creds
                .entry((*cred).to_owned())
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .unwrap()
                .entry((*model).to_owned())
                .or_insert_with(|| serde_json::json!({"requests": 0, "tokens": {}, "origins": {}}))
                .clone();
            let mut total: Counters = serde_json::from_value(entry).unwrap();
            total.merge(c);
            let mut slice = serde_json::to_value(c).unwrap();
            let mut row = serde_json::to_value(&total).unwrap();
            if let Some(usd) = usd {
                slice["usd"] = serde_json::json!(usd);
                slice["input_basis"] = serde_json::json!("fresh");
                row["input_basis"] = serde_json::json!("fresh");
                let before = llm_gateway::preset::pricing::for_model(model)
                    .map(|p| p.cost(&total.tokens))
                    .unwrap();
                row["usd"] = serde_json::json!(before);
            }
            let models = creds
                .get_mut(*cred)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .get_mut(*model)
                .unwrap();
            let origins = models
                .get("origins")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            row["origins"] = origins;
            row["origins"]
                .as_object_mut()
                .unwrap()
                .insert((*origin).to_owned(), slice);
            *models = row;
        }
        serde_json::from_value(serde_json::json!({
            "generated_at": 1_785_326_400_000_i64,
            "days": {"2026-07-29": {"credentials": creds, "total_usd": day_total}},
            "total_usd": day_total,
        }))
        .unwrap()
    }

    /// `--by origin` は内訳を素性ごとに割り、素性を先頭に置く。
    ///
    /// keepalive (gateway 自身の自送信) の費用を、人が出した分と分けて読む
    /// ための見え方 (DR-0029)。
    #[test]
    fn stats_splits_the_breakdown_by_origin() {
        let report = report_with_origins(&[
            (
                "claude-one",
                "claude-opus-5",
                "main",
                counters(2, 1_000, 100),
            ),
            (
                "claude-one",
                "claude-opus-5",
                "keepalive",
                counters(3, 30_000, 3),
            ),
        ]);

        let routes = render(&report, By::Route);
        assert!(
            !routes.contains("keepalive"),
            "the default view is unchanged: {routes}"
        );

        let out = render(&report, By::Origin);
        // 先頭の 1 行は見出し。内訳の行は字下げされている。
        let breakdown: Vec<&str> = out
            .lines()
            .skip(1)
            .filter(|l| l.starts_with("  "))
            .collect();
        assert_eq!(breakdown.len(), 2, "one row per origin: {out}");
        assert!(
            breakdown[0].starts_with("  keepalive claude-one claude-opus-5"),
            "the origin leads the label and the rows are grouped by it: {out}"
        );
        assert!(
            breakdown[1].starts_with("  main claude-one claude-opus-5"),
            "{out}"
        );
        // 割り方を変えただけなので、日の合計と全期間の合計は動かない。
        for line in out.lines().filter(|l| l.contains("total")) {
            assert!(line.contains("31,000"), "the totals still add up: {out}");
        }
    }

    /// 素性を知らない gateway が返した報告でも、行を落とさない。
    ///
    /// 落とすと内訳の和が日の合計と合わなくなる。言えるのは「見分けが
    /// 付かない」だけなので、その語をそのまま名乗らせる。
    #[test]
    fn a_report_without_origins_still_shows_its_rows() {
        let out = render(
            &stats_report(&[(
                "2026-07-29",
                &[("claude-one", "claude-opus-5", counters(1, 10, 5))],
            )]),
            By::Origin,
        );
        assert!(out.contains("  unknown claude-one claude-opus-5"), "{out}");
    }

    /// 割る軸は名指しで受ける。読めない語は断る。
    #[test]
    fn stats_rejects_an_unknown_axis() {
        assert_eq!(parse_stats(&["--by", "origin"]).unwrap().by, By::Origin);
        assert_eq!(parse_stats(&["--by=origin"]).unwrap().by, By::Origin);
        assert!(parse_stats(&["--by", "session"]).is_err());
        assert!(parse_stats(&["--by"]).is_err());
    }

    /// 日ごとの合計と、認証情報 × モデルの内訳が出る。
    #[test]
    fn stats_shows_a_total_and_a_breakdown() {
        let out = render_routes(&stats_report(&[(
            "2026-07-29",
            &[
                ("claude-one", "claude-haiku-4-5", counters(3, 1_200, 340)),
                ("claude-two", "claude-opus-5", counters(1, 50_000, 8_000)),
            ],
        )]));

        assert!(out.contains("2026-07-29"), "{out}");
        // 合計は 4 本 / input 51,200 / output 8,340。
        assert!(
            out.contains("51,200"),
            "the total with thousands separators: {out}"
        );
        assert!(out.contains("8,340"), "{out}");
        assert!(
            out.contains("claude-one"),
            "the credential appears in the breakdown: {out}"
        );
        assert!(
            out.contains("claude-opus-5"),
            "the model appears in the breakdown: {out}"
        );
        for head in [REQUESTS_HEADER, "input", "output", USD_HEADER] {
            assert!(out.contains(head), "missing header {head}: {out}");
        }
    }

    /// 記録に現れた区分だけが列になる。使っていない区分で桁を広げない。
    #[test]
    fn stats_columns_follow_the_kinds_that_were_recorded() {
        let out = render_routes(&stats_report(&[(
            "2026-07-29",
            &[("a", "claude-opus-5", counters(1, 10, 5))],
        )]));

        let head = out.lines().next().expect("a header line");
        assert!(head.contains("input") && head.contains("output"), "{out}");
        assert!(
            !head.contains("cache_w"),
            "an unused category is omitted: {out}"
        );
    }

    /// provider 固有の区分も列になる。潰すと、その消費が表から消える。
    #[test]
    fn a_provider_specific_kind_gets_its_own_column() {
        let mut c = counters(1, 10, 5);
        c.tokens.set(TokenKind::output_reasoning(), 7);
        c.tokens.set("provider.batch_prediction", 3);

        let out = render_routes(&stats_report(&[("2026-07-29", &[("a", "m", c)])]));

        let head = out.lines().next().expect("a header line");
        assert!(
            head.contains("reasoning"),
            "a standard category gets a short header: {out}"
        );
        assert!(
            head.contains("provider.batch_prediction"),
            "an unknown category is shown by its raw name: {out}"
        );
        let row = out
            .lines()
            .find(|l| l.starts_with("  a "))
            .expect("a breakdown line");
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>(),
            // 認証情報 / モデル / 本数 / input / output / reasoning / 固有区分 / USD。
            // `m` は単価表に無いので input は揃えられず、印が付く。
            ["a", "m", "1", "10*", "5", "7", "3", "-"],
            "{out}"
        );
    }

    /// 単価表に無いモデルの金額欄は `-`。0.0000 と出すと「使ったが安かった」に
    /// 見えるので、言えることが無いことを記号で示す。
    #[test]
    fn an_unpriced_model_shows_a_dash() {
        let out = render_routes(&stats_report(&[(
            "2026-07-29",
            &[
                ("a", "claude-opus-5", counters(1, 1_000_000, 0)),
                ("a", "who-knows", counters(1, 1_000_000, 0)),
            ],
        )]));

        let unpriced = out
            .lines()
            .find(|l| l.contains("who-knows"))
            .expect("a breakdown line");
        assert!(unpriced.trim_end().ends_with('-'), "{out}");
        // 合計は値付けできた分だけ ($5)。
        assert!(out.contains("5.0000"), "{out}");
    }

    /// 新しい日が上に来る。見たいのは直近。
    #[test]
    fn stats_puts_the_newest_day_first() {
        let out = render_routes(&stats_report(&[
            ("2026-07-28", &[("a", "m", counters(1, 1, 1))]),
            ("2026-07-30", &[("a", "m", counters(1, 2, 2))]),
        ]));

        let newest = out.find("2026-07-30").expect("the newer day");
        let oldest = out.find("2026-07-28").expect("the older day");
        assert!(newest < oldest, "the newer day comes first: {out}");
    }

    /// 桁は全日で揃える。日ごとに幅が変わると縦に読めない。
    #[test]
    fn stats_aligns_columns_across_days() {
        let out = render_routes(&stats_report(&[
            ("2026-07-29", &[("a", "m", counters(1, 1_000_000, 1))]),
            ("2026-07-30", &[("a", "m", counters(1, 5, 1))]),
        ]));

        // 内訳の行だけ集めて、数の始まる桁が揃っているか見る。
        let starts: Vec<usize> = out
            .lines()
            .filter(|l| l.starts_with("  a "))
            .map(|l| l.find(|c: char| c.is_ascii_digit() || c == ',').unwrap())
            .collect();
        assert_eq!(starts.len(), 2, "{out}");
        assert_eq!(starts[0], starts[1], "columns line up: {out}");
    }

    /// 記録が無ければ、何をすれば出るのかを言う。
    ///
    /// 文言は英語 (DR-0008: CLI が出す文言は英語)。
    #[test]
    fn empty_stats_say_why() {
        let out = render_routes(&stats_report(&[]));
        assert!(out.contains("No usage recorded yet"), "{out}");
        assert!(
            out.contains("gateway"),
            "points to what to check next: {out}"
        );
        assert!(
            out.contains("stats directory"),
            "shows the location too: {out}"
        );
    }

    /// 認証情報を持たない経路は予約名で出る。
    #[test]
    fn stats_show_the_credentialless_route() {
        let out = render_routes(&stats_report(&[(
            "2026-07-29",
            &[(llm_gateway::stats::NO_CREDENTIAL, "m", counters(1, 5, 6))],
        )]));
        assert!(out.contains(llm_gateway::stats::NO_CREDENTIAL), "{out}");
    }

    fn parse_stats(list: &[&str]) -> Result<Args, Failure> {
        parse(&args(list))
    }

    #[test]
    fn stats_defaults_to_a_week() {
        assert_eq!(parse_stats(&[]).unwrap().days, 7);
    }

    #[test]
    fn stats_takes_a_day_count() {
        assert_eq!(parse_stats(&["--days", "30"]).unwrap().days, 30);
        assert_eq!(parse_stats(&["--days=1"]).unwrap().days, 1);
        assert_eq!(
            parse_stats(&["--days", "0"]).unwrap().days,
            0,
            "0 means all time"
        );
    }

    /// 宛先は名前で指す (設定ファイルではなく、DR-0028 決定 6)。
    #[test]
    fn stats_takes_a_unit() {
        let parsed = parse_stats(&["--unit", "stable"]).unwrap();
        assert_eq!(parsed.unit.as_deref(), Some("stable"));
        assert_eq!(parsed.days, 7);
        assert_eq!(
            parse_stats(&["--unit=unstable", "--days=3"]).unwrap(),
            Args {
                days: 3,
                unit: Some("unstable".to_owned()),
                by: By::Route,
            }
        );
        assert!(parse_stats(&["--config", "/tmp/c.toml"]).is_err());
    }

    /// 読めない日数は断る。黙って既定に落とすと、絞ったつもりの相手が別の
    /// 範囲を見ることになる。
    #[test]
    fn stats_rejects_an_unreadable_day_count() {
        for bad in [vec!["--days", "lots"], vec!["--days=-1"], vec!["--days"]] {
            assert!(parse_stats(&bad).is_err(), "{bad:?}");
        }
    }

    /// 大きすぎる日数は上限で抑える (桁あふれで何も返らなくなるのを防ぐ)。
    #[test]
    fn stats_clamps_an_enormous_day_count() {
        let max = llm_gateway::stats::MAX_DAYS;
        assert_eq!(parse_stats(&["--days", "100000"]).unwrap().days, max);
        assert_eq!(
            parse_stats(&[&format!("--days={}", usize::MAX)])
                .unwrap()
                .days,
            max
        );
    }

    #[test]
    fn stats_rejects_unknown_options() {
        assert!(parse_stats(&["--refresh"]).is_err());
    }

    /// キャッシュ書き込みの内訳は、合計のすぐ隣に短い見出しで並ぶ。
    ///
    /// 生の区分名 (`input.cache_creation.ephemeral_1h`) を見出しにすると、
    /// 毎日出る列で桁が大きく広がる。
    #[test]
    fn the_cache_write_breakdown_sits_next_to_its_total() {
        let mut c = counters(1, 10, 5);
        c.tokens.set(TokenKind::input_cache_creation(), 300);
        c.tokens.set(TokenKind::input_cache_creation_1h(), 200);
        c.tokens.set(TokenKind::input_cache_creation_5m(), 100);

        let out = render_routes(&stats_report(&[("2026-07-29", &[("a", "m", c)])]));

        let head = out.lines().next().expect("a header line");
        let heads: Vec<&str> = head.split_whitespace().collect();
        let at = |name| heads.iter().position(|h| *h == name);
        assert_eq!(
            (at("w_1h"), at("w_5m")),
            (at("cache_w").map(|i| i + 1), at("cache_w").map(|i| i + 2)),
            "the breakdown follows the total: {head}"
        );
    }

    /// 表の形を丸ごと固定する。
    ///
    /// 桁揃えは目で見て決めたもので、崩れても個別の assert には出にくい。
    /// 1 度だけの見出し・日ごとの合計・字下げした内訳・日の間の空行を、
    /// まとめてここで見張る。1 行にしか出てこない区分 (ここでは cache) も
    /// 列として立ち、他の行はその欄が 0 になる。
    #[test]
    fn the_table_keeps_its_shape() {
        let mut cached = counters(12, 98_120, 7_431);
        cached.tokens.set(TokenKind::input_cache_creation(), 2_000);
        cached.tokens.set(TokenKind::input_cache_read(), 50_000);

        let out = render_routes(&stats_report(&[
            (
                "2026-07-29",
                &[
                    (
                        "claude-one",
                        "claude-haiku-4-5",
                        counters(312, 1_204_887, 34_002),
                    ),
                    ("claude-two", "claude-opus-5", cached),
                ],
            ),
            (
                "2026-07-30",
                &[("claude-one", "claude-haiku-4-5", counters(7, 8_120, 931))],
            ),
        ]));

        let expected = concat!(
            "                                   reqs     input    output   cache_w   cache_r    usd\n",
            "2026-07-30 total                      7     8,120       931         0         0 0.0128\n",
            "  claude-one claude-haiku-4-5         7     8,120       931         0         0 0.0128\n",
            "\n",
            "2026-07-29 total                    324 1,303,007    41,433     2,000    50,000 2.0888\n",
            "  claude-one claude-haiku-4-5       312 1,204,887    34,002         0         0 1.3749\n",
            "  claude-two claude-opus-5           12    98,120     7,431     2,000    50,000 0.7139\n",
            "\n",
            "total                               331 1,311,127    42,364     2,000    50,000 2.1015\n",
        );
        assert_eq!(
            out, expected,
            "\n--- actual ---\n{out}--- expected ---\n{expected}"
        );
    }

    /// 揃えられなかった input には `*` が付き、合計行にも伝わり、脚注が出る
    /// (DR-0029)。揃えた行だけの表には印の桁も脚注も出ない。
    #[test]
    fn an_input_left_as_recorded_is_marked() {
        let out = render_routes(&stats_report(&[(
            "2026-07-29",
            &[
                ("a", "claude-opus-5", counters(1, 100, 0)),
                ("a", "who-knows", counters(1, 1_000, 0)),
            ],
        )]));
        let cells = |prefix: &str| -> Vec<String> {
            out.lines()
                .find(|l| l.starts_with(prefix))
                .unwrap_or_else(|| panic!("no `{prefix}` line: {out}"))
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(cells("  a claude-opus-5")[3], "100", "{out}");
        assert_eq!(cells("  a who-knows")[3], "1,000*", "{out}");
        assert_eq!(cells("2026-07-29 total")[3], "1,100*", "{out}");
        assert_eq!(cells("total")[2], "1,100*", "{out}");
        assert!(out.contains("\n* input as reported by upstream"), "{out}");

        let clean = render_routes(&stats_report(&[(
            "2026-07-29",
            &[("a", "claude-opus-5", counters(1, 100, 0))],
        )]));
        assert!(!clean.contains('*'), "{clean}");
    }
}
