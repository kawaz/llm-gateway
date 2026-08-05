//! Anthropic の応答を metering の正規形へ写す。
//!
//! 枠ヘッダの読み方、断られ方の意味、本文 usage のトークン区分、単価の課金軸は
//! どれもこの方言の知識なので preset 側が持つ (DR-0014 §4)。core が規定するのは
//! 写した先の形 ([`crate::metering`] / [`crate::quota`] / [`crate::denial`]) だけ。

use serde_json::Value;

use crate::denial::{DEFAULT_BACKOFF, Denial, RESET_SLACK, Reason, Scope};
use crate::egress::Headers;
use crate::metering::{Pricing, TokenKind, TokenUsage, UsageObserver};
use crate::provider::Metering;
use crate::quota::{Overage, Snapshot, Window};

/// `retry-after` をどこまで信じるか (秒)。
///
/// 窓の開く時刻と違い、`retry-after` は根拠を確かめようがない。桁の壊れた値
/// (時刻の足し算が溢れる、実質永久に閉じる) をそのまま採ると、経路を失う。
const MAX_BACKOFF: i64 = 7 * 24 * 60 * 60;

/// SSE の 1 イベントをどこまで抱えるか。
///
/// 行の途中でチャンクが切れるので行が揃うまで持ち、さらに 1 イベントが複数の
/// `data:` 行に割れうるのでイベントが閉じるまで持つ。壊れた相手 (改行も空行も
/// 返さない upstream) にメモリを食い潰されないための上限で、実際の
/// `message_start` は 1KB 前後なので桁が違う。
const MAX_SSE_EVENT: usize = 256 * 1024;

/// ストリームでない応答を、集計のためにどこまで抱えるか。
const MAX_JSON_BODY: usize = 4 * 1024 * 1024;

/// Messages API の応答から読み取る。
pub struct AnthropicMetering;

impl Metering for AnthropicMetering {
    fn quota_snapshot(&self, headers: &Headers, observed_at: i64) -> Option<Snapshot> {
        read_unified(headers, observed_at)
    }

    /// この応答は経路を締め出すか。するならいつまで、どの範囲で。
    ///
    /// - **429 + 塞がっている窓** → その窓が開く時刻 (+ [`RESET_SLACK`]) まで、
    ///   経路全体を ([`Reason::Limited`] / [`Scope::Everything`])。窓が
    ///   複数塞がっているなら**最も遅く開く時刻**まで — 5 時間の窓が開いても、
    ///   7 日の窓が塞がったままなら通らない
    /// - **429 で窓が読めない / 529** → `retry-after`、無ければ
    ///   [`DEFAULT_BACKOFF`] だけ、頼んだモデルにだけ
    ///   ([`Reason::Busy`] / [`Scope::Model`])
    /// - それ以外の状態 → 締め出さない。401 / 403 は待っても直らないので、
    ///   時間で空ける印を付ける意味がない
    ///
    /// 範囲を応答から決めるのは、**モデル別の制限がヘッダに出てこない**ため
    /// (実測 2026-07-31、DR-0009)。窓が塞がったという証拠があるときだけ
    /// 経路全体に広げ、証拠が無いものは頼んだモデルの事情として扱う。
    fn rejection(
        &self,
        status: u16,
        headers: &Headers,
        model: &str,
        observed_at: i64,
    ) -> Option<Denial> {
        if !matches!(status, 429 | 529) {
            return None;
        }
        if status == 429
            && let Some(reset) = last_reset(headers, observed_at)
        {
            return Some(Denial {
                // 開くと言われた時刻ちょうどではなく、少し待ってから戻す。
                until: reset + RESET_SLACK,
                reason: Reason::Limited,
                scope: Scope::Everything,
            });
        }
        let after = retry_after(headers).unwrap_or(DEFAULT_BACKOFF);
        Some(Denial {
            until: observed_at + after.clamp(0, MAX_BACKOFF),
            reason: Reason::Busy,
            scope: Scope::Model(model.to_owned()),
        })
    }

    fn usage_observer(&self, content_type: Option<&str>) -> Option<Box<dyn UsageObserver>> {
        Mode::of(content_type)
            .map(|mode| Box::new(MessagesUsage::new(mode)) as Box<dyn UsageObserver>)
    }

    /// 重複しない課金軸を選ぶ。
    ///
    /// Anthropic の `input_tokens` はキャッシュ分を含まないので、単価表の
    /// 4 区分をそのまま並べても二重計上にならない。表に無い区分 (provider 固有の
    /// 内訳) は課金へ入らないので、観測値として残したまま合計は動かない。
    fn pricing(&self, model: &str) -> Option<Pricing> {
        crate::preset::pricing::for_model(model)
    }
}

/// `anthropic-ratelimit-unified-*` を読む (実測値は DR-0007)。
///
/// undocumented なヘッダなので、読めた分だけ使って残りは `None` にする。
/// 1 つも読めなければスナップショットを作らない — 空の器を置くと「観測した」
/// と「まだ観測していない」が区別できなくなる。
fn read_unified(headers: &Headers, now: i64) -> Option<Snapshot> {
    let window = |prefix: &str| {
        let w = Window {
            utilization: headers
                .get(&format!("anthropic-ratelimit-unified-{prefix}-utilization"))
                .and_then(|v| v.trim().parse().ok()),
            status: headers
                .get(&format!("anthropic-ratelimit-unified-{prefix}-status"))
                .map(str::to_owned),
            ..Window::default()
        }
        .with_reset(
            headers
                .get(&format!("anthropic-ratelimit-unified-{prefix}-reset"))
                .and_then(|v| v.trim().parse().ok()),
        );
        (!w.is_empty()).then_some(w)
    };

    let overage = Overage {
        status: headers
            .get("anthropic-ratelimit-unified-overage-status")
            .map(str::to_owned),
        disabled_reason: headers
            .get("anthropic-ratelimit-unified-overage-disabled-reason")
            .map(str::to_owned),
    };

    Snapshot::new(
        now,
        window("5h"),
        window("7d"),
        (!overage.is_empty()).then_some(overage),
    )
}

/// 塞がっている窓が全部開くのはいつか。
///
/// **最も遅い方**を採る。5 時間の窓が開いても、7 日の窓が塞がったままなら
/// このリクエストは通らない。早い方を採ると、開いていない相手に当てに行って
/// 429 を貰い直すことになる。
fn last_reset(headers: &Headers, now: i64) -> Option<i64> {
    let snapshot = read_unified(headers, now)?;
    [snapshot.five_hour, snapshot.seven_day]
        .into_iter()
        .flatten()
        .filter(is_rejected)
        .filter_map(|w| w.reset)
        // 過ぎている時刻は手掛かりにならない。次の手掛かりへ落とす。
        .filter(|reset| *reset > now)
        .max()
}

/// この窓は塞がっているか。
///
/// 通っている側の語 (`allowed`, `allowed_warning`) だけを数え、それ以外を
/// 塞がっている扱いにする。語彙は公式に記載が無く増えうる (DR-0007) ので、
/// 知らない語で締め出しを見送ると、印が付かないまま毎回 429 を貰い続ける
/// 元の状態に戻る。
fn is_rejected(window: &Window) -> bool {
    window
        .status
        .as_deref()
        .is_some_and(|s| !s.trim().to_ascii_lowercase().starts_with("allowed"))
}

/// `retry-after` の秒数。HTTP-date 形式なら読まない (既定へ落とす)。
fn retry_after(headers: &Headers) -> Option<i64> {
    headers
        .get("retry-after")
        .and_then(|v| v.trim().parse().ok())
}

/// 本文のどの読み方をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// SSE。`data:` の行に usage が現れる。
    Sse,
    /// ひとまとまりの JSON。終端で `/usage` を読む。
    Json,
}

impl Mode {
    /// この content-type から usage を読めるか。
    ///
    /// 分からない形は読まない。中身を推測して読みに行くと、画像やバイナリを
    /// JSON として抱え込むことになる。
    fn of(content_type: Option<&str>) -> Option<Self> {
        // `application/json; charset=utf-8` のような付属物を落とす。
        let base = content_type?.split(';').next()?.trim().to_ascii_lowercase();
        match base.as_str() {
            "text/event-stream" => Some(Self::Sse),
            "application/json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// 通り過ぎたバイト列から usage を読む。本文は変えない。
struct MessagesUsage {
    mode: Mode,
    /// 行の途中 (SSE) / 本文の全部 (JSON) を溜める控え。
    held: Vec<u8>,
    /// 今のイベントで溜めた `data:` の中身 (SSE)。複数行なら改行で繋いである。
    event: Vec<u8>,
    /// 上限を超えたので、この応答の集計をやめた。
    given_up: bool,
    usage: TokenUsage,
}

impl MessagesUsage {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            held: Vec::new(),
            event: Vec::new(),
            given_up: false,
            usage: TokenUsage::default(),
        }
    }

    /// SSE を行に切り、イベントが閉じるところで中身を読む。
    ///
    /// チャンクの境目は行の途中に落ちる。揃った行だけを処理し、残りは次の
    /// チャンクまで持つ。
    fn observe_sse(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == b'\n' {
                let line = std::mem::take(&mut self.held);
                self.read_sse_line(&line);
                continue;
            }
            // 書きかけの行と、このイベントで溜めた分の合計で見る。
            if self.held.len() + self.event.len() >= MAX_SSE_EVENT {
                self.give_up("SSE の 1 イベントが長すぎます");
                return;
            }
            self.held.push(b);
        }
    }

    /// SSE の 1 行を処理する。
    ///
    /// 空行はイベントの終わり。`data:` の行は中身を溜めるだけで、読むのは
    /// イベントが閉じたとき — **1 つのイベントの data は複数行に割れてよく、
    /// その場合は改行で繋いだものが 1 つの中身**になる (SSE の仕様)。行ごとに
    /// 解こうとすると、そうやって割られた usage を黙って取りこぼす。
    fn read_sse_line(&mut self, line: &[u8]) {
        // 行末の `\r` は終端の一部 (CRLF で区切る upstream がある)。
        let line = line.strip_suffix(b"\r").unwrap_or(line);

        if line.is_empty() {
            self.finish_event();
            return;
        }
        let Some(payload) = line.strip_prefix(b"data:") else {
            // `event:` / `id:` / 注釈行は読まない。usage を載せるのは data だけ。
            return;
        };
        // コロンの直後の空白 1 つは区切りの一部で、中身には入らない。
        let payload = payload.strip_prefix(b" ").unwrap_or(payload);

        if !self.event.is_empty() {
            self.event.push(b'\n');
        }
        self.event.extend_from_slice(payload);
    }

    /// イベントが閉じた。溜めた中身から usage を読む。
    ///
    /// JSON として解くのは usage が載っているものだけ。イベントは 1 応答で
    /// 何十個も流れるので、全部解くと中継の脇で無駄に働くことになる。
    fn finish_event(&mut self) {
        let event = std::mem::take(&mut self.event);
        if !contains(&event, b"\"usage\"") {
            return;
        }
        let Ok(parsed) = serde_json::from_slice::<Value>(&event) else {
            return;
        };
        // `message_start` は `/message/usage`、`message_delta` は `/usage` に
        // 載せる。イベント名で決め打ちせず、在る方を読む。
        for pointer in ["/message/usage", "/usage"] {
            if let Some(usage) = parsed.pointer(pointer) {
                self.absorb(usage);
            }
        }
    }

    /// usage オブジェクトに載っている分を正規形へ写す。
    ///
    /// 値は累積で届くので**上書き**する (足さない)。載っていない区分は前に
    /// 拾った値を保つ — `message_delta` が一部しか載せない場合に備える。
    fn absorb(&mut self, usage: &Value) {
        const AXES: &[(&str, &str)] = &[
            ("input_tokens", TokenKind::INPUT_NAME),
            ("output_tokens", TokenKind::OUTPUT_NAME),
            (
                "cache_creation_input_tokens",
                TokenKind::INPUT_CACHE_CREATION_NAME,
            ),
            ("cache_read_input_tokens", TokenKind::INPUT_CACHE_READ_NAME),
        ];
        for (field, kind) in AXES {
            if let Some(count) = usage.get(field).and_then(Value::as_u64) {
                self.usage.set(*kind, count);
            }
        }
    }

    /// 上限を超えた。この応答の集計は捨てる。
    fn give_up(&mut self, reason: &str) {
        self.given_up = true;
        self.held = Vec::new();
        self.event = Vec::new();
        self.usage = TokenUsage::default();
        tracing::warn!(reason, "使用量の集計をやめます");
    }
}

impl UsageObserver for MessagesUsage {
    fn observe(&mut self, chunk: &[u8]) {
        if self.given_up {
            return;
        }
        match self.mode {
            Mode::Sse => self.observe_sse(chunk),
            Mode::Json => {
                if self.held.len() + chunk.len() > MAX_JSON_BODY {
                    self.give_up("応答が大きすぎます");
                    return;
                }
                self.held.extend_from_slice(chunk);
            }
        }
    }

    /// 途中まで流れた応答は、そこまでに読めた分を返す。
    ///
    /// `message_start` まで届いていれば input は分かる。中断した分を丸ごと
    /// 捨てると、実際に消費した入力が記録から消える。
    fn finish(mut self: Box<Self>) -> Option<TokenUsage> {
        if self.given_up {
            return None;
        }
        match self.mode {
            // ストリームでない応答は、ここで初めて全体が揃う。
            Mode::Json => {
                if !self.held.is_empty()
                    && let Ok(body) = serde_json::from_slice::<Value>(&self.held)
                    && let Some(usage) = body.pointer("/usage")
                {
                    self.absorb(usage);
                }
            }
            // 終端が空行で閉じられていなければ、最後のイベントが溜まったまま
            // 残る。書きかけの行も最後の 1 行として扱う。
            Mode::Sse => {
                let last = std::mem::take(&mut self.held);
                if !last.is_empty() {
                    self.read_sse_line(&last);
                }
                self.finish_event();
            }
        }
        (!self.usage.is_empty()).then_some(self.usage)
    }
}

/// `needle` を含むか。
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    /// 試験で使うモデル名。実際の運用と同じく、断られ方がモデルで違う。
    const FABLE: &str = "claude-fable-5";

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        Headers::new(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    /// 1 つの窓を表すヘッダ。
    fn window(prefix: &str, status: &str, reset: i64) -> Vec<(String, String)> {
        vec![
            (
                format!("anthropic-ratelimit-unified-{prefix}-status"),
                status.to_owned(),
            ),
            (
                format!("anthropic-ratelimit-unified-{prefix}-reset"),
                reset.to_string(),
            ),
        ]
    }

    fn windows(pairs: Vec<Vec<(String, String)>>) -> Headers {
        Headers::new(pairs.into_iter().flatten().collect())
    }

    fn rejection(status: u16, headers: &Headers, model: &str) -> Option<Denial> {
        AnthropicMetering.rejection(status, headers, model, NOW)
    }

    fn limited(until: i64) -> Denial {
        Denial {
            until,
            reason: Reason::Limited,
            scope: Scope::Everything,
        }
    }

    fn busy(until: i64, model: &str) -> Denial {
        Denial {
            until,
            reason: Reason::Busy,
            scope: Scope::Model(model.to_owned()),
        }
    }

    fn read(content_type: &str, chunks: &[&[u8]]) -> Option<TokenUsage> {
        let mut observer = AnthropicMetering
            .usage_observer(Some(content_type))
            .expect("読める形");
        for chunk in chunks {
            observer.observe(chunk);
        }
        observer.finish()
    }

    // ---------- 枠ヘッダ ----------

    /// 実測された unified ヘッダ一式 (DR-0007 の表)。
    fn unified() -> Headers {
        headers(&[
            ("content-type", "application/json"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.71"),
            ("anthropic-ratelimit-unified-5h-reset", "1785344400"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.3"),
            ("anthropic-ratelimit-unified-7d-reset", "1785661200"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-overage-status", "disabled"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "out_of_credits",
            ),
        ])
    }

    #[test]
    fn reads_every_unified_field() {
        let s = AnthropicMetering
            .quota_snapshot(&unified(), NOW)
            .expect("読める");

        assert_eq!(s.observed_at, NOW, "取得時刻を必ず付ける");

        let five = s.five_hour.unwrap();
        assert_eq!(five.utilization, Some(0.71));
        assert_eq!(five.status.as_deref(), Some("allowed"));
        assert_eq!(five.reset, Some(1_785_344_400));
        assert_eq!(
            five.reset_iso.as_deref(),
            Some("2026-07-29T17:00:00Z"),
            "Unix 秒と ISO の両方を出す"
        );

        let seven = s.seven_day.unwrap();
        assert_eq!(seven.utilization, Some(0.3));
        assert_eq!(seven.reset, Some(1_785_661_200));

        let overage = s.overage.unwrap();
        assert_eq!(overage.status.as_deref(), Some("disabled"));
        assert_eq!(overage.disabled_reason.as_deref(), Some("out_of_credits"));
    }

    /// ヘッダ名の大小は問わない (upstream や手前のプロキシで変わりうる)。
    #[test]
    fn header_lookup_ignores_case() {
        let s = AnthropicMetering
            .quota_snapshot(
                &headers(&[("Anthropic-RateLimit-Unified-5h-Utilization", "0.5")]),
                NOW,
            )
            .unwrap();
        assert_eq!(s.five_hour.unwrap().utilization, Some(0.5));
    }

    /// undocumented なヘッダなので、欠けても壊れない。
    /// 読めた分だけ使い、残りは None のままにする。
    #[test]
    fn missing_fields_do_not_break_the_rest() {
        let s = AnthropicMetering
            .quota_snapshot(
                &headers(&[
                    ("anthropic-ratelimit-unified-5h-utilization", "0.9"),
                    (
                        "anthropic-ratelimit-unified-7d-utilization",
                        "beyond-repair",
                    ),
                ]),
                NOW,
            )
            .unwrap();

        let five = s.five_hour.unwrap();
        assert_eq!(five.utilization, Some(0.9));
        assert_eq!(five.reset, None, "無いものは None");
        assert_eq!(five.reset_iso, None);
        assert!(s.overage.is_none());
        assert!(
            s.seven_day.is_none(),
            "1 つも読めなかった窓は、空の器を置かずに落とす"
        );
    }

    /// 枠が 1 つも無い応答からは作らない。
    ///
    /// 空のスナップショットを置くと「観測した」と「まだ観測していない」が
    /// 区別できなくなる。
    #[test]
    fn unrelated_response_yields_nothing() {
        assert!(
            AnthropicMetering
                .quota_snapshot(
                    &headers(&[
                        ("content-type", "text/event-stream"),
                        ("anthropic-ratelimit-requests-remaining", "42"),
                    ]),
                    NOW,
                )
                .is_none()
        );
        assert!(
            AnthropicMetering
                .quota_snapshot(&Headers::default(), NOW)
                .is_none()
        );
    }

    // ---------- 断られ方 ----------

    /// 塞がっている窓があれば、そこが開く時刻まで経路全体を外す。
    #[test]
    fn a_rejected_window_closes_the_whole_route() {
        let h = windows(vec![
            window("5h", "allowed", NOW + 100),
            window("7d", "rejected", NOW + 5000),
        ]);
        assert_eq!(
            rejection(429, &h, FABLE),
            Some(limited(NOW + 5000 + RESET_SLACK)),
            "窓はアカウントに掛かるので、頼んだモデルには関係しない"
        );
    }

    /// 開くと言われた時刻ちょうどには戻さない。
    ///
    /// リセット時刻ちょうどに使い始めても 1 分ほど遅れて実際に使えるように
    /// なる (kawaz 実測)。ちょうどに戻すと、その 1 本が 429 を貰って
    /// 締め出しが伸びる。
    #[test]
    fn the_route_comes_back_a_little_after_the_reset() {
        let h = windows(vec![window("7d", "rejected", NOW + 5000)]);
        let denial = rejection(429, &h, FABLE).unwrap();
        assert_eq!(denial.until, NOW + 5000 + RESET_SLACK);
        assert!(
            denial.until > NOW + 5000,
            "開くと言われた時刻より後に戻す (猶予 {} 秒)",
            denial.until - (NOW + 5000)
        );
    }

    /// 塞がっている窓が複数あるなら、全部開くまで。
    ///
    /// 5 時間の窓が開いても、7 日の窓が塞がったままなら通らない。早い方を
    /// 採ると、開いていない相手に当てに行って 429 を貰い直すことになる。
    #[test]
    fn every_rejected_window_must_open() {
        let h = windows(vec![
            window("5h", "rejected", NOW + 100),
            window("7d", "rejected", NOW + 5000),
        ]);
        assert_eq!(
            rejection(429, &h, FABLE),
            Some(limited(NOW + 5000 + RESET_SLACK))
        );
    }

    /// 開いている窓の時刻は数えない。塞がっている窓だけを見る。
    #[test]
    fn an_open_window_does_not_extend_the_deadline() {
        let h = windows(vec![
            window("5h", "rejected", NOW + 100),
            window("7d", "allowed", NOW + 5000),
        ]);
        assert_eq!(
            rejection(429, &h, FABLE),
            Some(limited(NOW + 100 + RESET_SLACK))
        );
    }

    /// 遠い開く時刻でも縮めない。開く時刻を知っているならそこまで待つ。
    #[test]
    fn a_distant_reset_is_not_shortened() {
        let h = windows(vec![window("7d", "rejected", NOW + 400 * 24 * 3600)]);
        assert_eq!(
            rejection(429, &h, FABLE),
            Some(limited(NOW + 400 * 24 * 3600 + RESET_SLACK))
        );
    }

    /// 警告つきでも通ってはいる。塞がっている扱いにしない。
    #[test]
    fn a_warning_window_is_still_open() {
        let mut pairs = window("5h", "allowed_warning", NOW + 100);
        pairs.push(("retry-after".to_owned(), "30".to_owned()));
        assert_eq!(
            rejection(429, &Headers::new(pairs), FABLE),
            Some(busy(NOW + 30, FABLE)),
            "窓は塞がっていないので、頼んだモデルの事情として短く退避する"
        );
    }

    /// 窓が読めない 429 は、頼んだモデルだけを短く外す。
    ///
    /// 実測 (2026-07-31): 同じ credential で haiku は 200 (全窓 allowed)、
    /// fable / opus / sonnet は上限のヘッダを 1 つも載せない 429 を返す。
    /// モデル別の制限はヘッダに出てこないので、頼んだモデル以外に広げる
    /// 根拠がない。
    #[test]
    fn a_bare_rate_limit_only_closes_the_model_asked_for() {
        assert_eq!(
            rejection(429, &Headers::default(), FABLE),
            Some(busy(NOW + DEFAULT_BACKOFF, FABLE))
        );
        assert_eq!(
            rejection(429, &headers(&[("retry-after", "30")]), FABLE),
            Some(busy(NOW + 30, FABLE))
        );
    }

    /// 529 も宛先とモデルの都合。窓のヘッダが載っていても短い退避のまま。
    #[test]
    fn an_overloaded_upstream_only_steps_aside() {
        let h = windows(vec![window("7d", "rejected", NOW + 5000)]);
        assert_eq!(
            rejection(529, &h, FABLE),
            Some(busy(NOW + DEFAULT_BACKOFF, FABLE))
        );
    }

    /// 過ぎた開く時刻は使わない。
    #[test]
    fn a_past_reset_is_ignored() {
        let mut pairs = window("7d", "rejected", NOW - 10);
        pairs.push(("retry-after".to_owned(), "30".to_owned()));
        assert_eq!(
            rejection(429, &Headers::new(pairs), FABLE),
            Some(busy(NOW + 30, FABLE))
        );
    }

    /// 日付形式の `retry-after` は読まない。
    #[test]
    fn an_http_date_retry_after_falls_back_to_the_default() {
        let h = headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(
            rejection(529, &h, FABLE),
            Some(busy(NOW + DEFAULT_BACKOFF, FABLE))
        );
    }

    /// 待っても直らない断りは締め出さない。
    #[test]
    fn an_auth_failure_is_not_a_cooldown() {
        for status in [200, 400, 401, 403, 500] {
            assert_eq!(
                rejection(status, &Headers::default(), FABLE),
                None,
                "{status}"
            );
        }
    }

    /// 桁の壊れた `retry-after` で時刻の足し算を溢れさせない。
    #[test]
    fn an_absurd_retry_after_is_clamped() {
        for value in [i64::MAX.to_string(), "999999999999".to_owned()] {
            let h = headers(&[("retry-after", &value)]);
            assert_eq!(
                rejection(429, &h, FABLE),
                Some(busy(NOW + MAX_BACKOFF, FABLE)),
                "{value}"
            );
        }
    }

    /// 負の `retry-after` は 0 扱い。過去の時刻を印にしない。
    #[test]
    fn a_negative_retry_after_does_not_look_expired() {
        let h = headers(&[("retry-after", "-10")]);
        assert_eq!(rejection(429, &h, FABLE), Some(busy(NOW, FABLE)));
    }

    // ---------- 本文 usage ----------

    /// 累積で届く usage は、最後に見た値が残る。
    #[test]
    fn reads_usage_from_a_streamed_response() {
        let usage = read(
            "text/event-stream",
            &[
                b"event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":3}}}\n\n",
                b"event: message_delta\ndata: {\"usage\":{\"output_tokens\":7}}\n\n",
            ],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::input()), Some(10));
        assert_eq!(usage.get(&TokenKind::input_cache_read()), Some(3));
        assert_eq!(usage.get(&TokenKind::output()), Some(7));
    }

    /// チャンクの境目が行の途中に落ちても取りこぼさない。
    #[test]
    fn survives_chunk_boundaries_inside_a_line() {
        let usage = read(
            "text/event-stream",
            &[b"data: {\"usage\":{\"input_", b"tokens\":42}}", b"\n\n"],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::input()), Some(42));
    }

    /// 1 イベントの data が複数行に割れていても 1 つの中身として読む。
    #[test]
    fn joins_multi_line_event_data() {
        let usage = read(
            "text/event-stream",
            &[b"data: {\"usage\":\ndata: {\"output_tokens\":5}}\n\n"],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::output()), Some(5));
    }

    /// 空行で閉じられないまま終わっても、溜めた分を読む。
    #[test]
    fn reads_the_last_event_without_a_closing_blank_line() {
        let usage = read(
            "text/event-stream",
            &[b"data: {\"usage\":{\"output_tokens\":9}}"],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::output()), Some(9));
    }

    /// ストリームでない応答は終端で全体を読む。
    #[test]
    fn reads_usage_from_a_whole_json_body() {
        let usage = read(
            "application/json; charset=utf-8",
            &[br#"{"usage":{"input_tokens":1,"cache_creation_input_tokens":2}}"#],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::input()), Some(1));
        assert_eq!(usage.get(&TokenKind::input_cache_creation()), Some(2));
    }

    /// usage を載せない content-type には observer を作らない。
    #[test]
    fn refuses_to_read_unknown_content_types() {
        assert!(AnthropicMetering.usage_observer(None).is_none());
        assert!(
            AnthropicMetering
                .usage_observer(Some("image/png"))
                .is_none()
        );
    }

    /// 累積で届く usage は足さずに置き換える (実測 2026-07-30)。
    ///
    /// `message_start` の `output_tokens:1` と `message_delta` の `16` を足すと
    /// 17 になり、実際より多く数える。
    #[test]
    fn a_cumulative_usage_replaces_the_earlier_value() {
        let usage = read(
            "text/event-stream",
            &[
                b"event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":18,\"output_tokens\":1}}}\n\n",
                b"event: message_delta\ndata: {\"usage\":{\"input_tokens\":18,\"output_tokens\":16}}\n\n",
            ],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::output()), Some(16), "1 + 16 にしない");
        assert_eq!(
            usage.get(&TokenKind::input()),
            Some(18),
            "同じ値を二重に足さない"
        );
    }

    /// 後の usage に載っていない区分は、前に拾った値を保つ。
    ///
    /// `message_delta` が一部しか載せない場合に、拾えていた分が消えないように。
    #[test]
    fn kinds_absent_from_a_later_usage_are_kept() {
        let usage = read(
            "text/event-stream",
            &[
                b"data: {\"usage\":{\"input_tokens\":20,\"cache_read_input_tokens\":900}}\n\n",
                b"data: {\"usage\":{\"output_tokens\":7}}\n\n",
            ],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::input()), Some(20));
        assert_eq!(usage.get(&TokenKind::input_cache_read()), Some(900));
        assert_eq!(usage.get(&TokenKind::output()), Some(7));
    }

    /// 数の付いていない値は拾わない。読めた区分だけを残す。
    #[test]
    fn non_numeric_usage_values_are_skipped() {
        let usage = read(
            "application/json",
            &[br#"{"usage":{"input_tokens":"many","output_tokens":null,
                "cache_read_input_tokens":5}}"#],
        )
        .expect("読めた区分がある");

        assert_eq!(usage.get(&TokenKind::input()), None);
        assert_eq!(usage.get(&TokenKind::output()), None);
        assert_eq!(usage.get(&TokenKind::input_cache_read()), Some(5));
    }

    /// `data:` 以外の行は読まない。
    ///
    /// `event:` の行や注釈に usage という語が混ざっても数えない。
    #[test]
    fn only_data_lines_are_parsed() {
        assert_eq!(
            read(
                "text/event-stream",
                &[
                    b": a comment mentioning usage\nevent: usage\nid: {\"usage\":{\"input_tokens\":999}}\n\n"
                ],
            ),
            None
        );
    }

    /// `data:` の後の空白は在っても無くてもよい。
    #[test]
    fn a_data_line_without_a_space_is_read() {
        let usage = read(
            "text/event-stream",
            &[b"data:{\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n"],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::output()), Some(5));
    }

    /// 別のイベントの data 同士は繋がない。
    ///
    /// 空行で区切られていれば別の中身。繋ぐと壊れた JSON になって、どちらの
    /// usage も読めなくなる。
    #[test]
    fn data_from_different_events_is_not_joined() {
        let usage = read(
            "text/event-stream",
            &[
                b"event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n",
                b"event: message_delta\ndata: {\"usage\":{\"output_tokens\":9}}\n\n",
            ],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::input()), Some(7), "前のイベントから");
        assert_eq!(usage.get(&TokenKind::output()), Some(9), "後のイベントから");
    }

    /// CRLF で区切る upstream でも読める。
    #[test]
    fn crlf_line_endings_are_handled() {
        let usage = read(
            "text/event-stream",
            &[b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\r\n\r\n"],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::output()), Some(9));
    }

    /// 1 つも読めなければ「観測なし」を返す (0 と区別する)。
    #[test]
    fn reports_nothing_when_no_usage_appeared() {
        assert_eq!(
            read("text/event-stream", &[b"data: {\"type\":\"ping\"}\n\n"]),
            None
        );
    }

    /// 壊れた本文からは読み取らない (読めないだけで、中継は別の話)。
    #[test]
    fn a_broken_body_yields_nothing() {
        assert_eq!(read("application/json", &[b"{ not json"]), None);
        assert_eq!(
            read("text/event-stream", &[b"data: {\"usage\": broken}\n\n"]),
            None
        );
    }

    /// 上限を超えた応答は、途中まで読めていても捨てる。
    #[test]
    fn gives_up_on_an_endless_event() {
        let flood = vec![b'x'; MAX_SSE_EVENT + 1];
        assert_eq!(read("text/event-stream", &[&flood]), None);
    }

    /// 諦めた後に usage が流れてきても拾い直さない。
    #[test]
    fn giving_up_is_not_undone_by_a_later_event() {
        let flood = vec![b'x'; MAX_SSE_EVENT + 1];
        assert_eq!(
            read(
                "text/event-stream",
                &[&flood, b"\ndata: {\"usage\":{\"output_tokens\":9}}\n\n"],
            ),
            None
        );
    }

    /// 大きすぎる JSON は抱え込まない。
    #[test]
    fn gives_up_on_an_oversized_json_body() {
        let chunk = vec![b'x'; 1024 * 1024];
        let chunks: Vec<&[u8]> = (0..5).map(|_| chunk.as_slice()).collect();
        assert_eq!(read("application/json", &chunks), None);
    }

    /// 単価表にあるモデルは 4 区分に値が付き、無いモデルは値付けしない。
    #[test]
    fn prices_only_known_models() {
        let pricing = AnthropicMetering
            .pricing("claude-opus-5")
            .expect("表にある");
        for kind in [
            TokenKind::input(),
            TokenKind::output(),
            TokenKind::input_cache_creation(),
            TokenKind::input_cache_read(),
        ] {
            assert!(pricing.rates.contains_key(&kind), "{kind} の単価がない");
        }

        assert!(AnthropicMetering.pricing("no-such-model").is_none());
    }
}
