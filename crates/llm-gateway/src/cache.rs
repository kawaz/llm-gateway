//! prompt cache の扱いを、送る本文へ効かせる (DR-0024)。
//!
//! 触るのは `cache_control` だけ。ブレークポイントを増やす・動かすことはしない
//! — 境界が変わると、その時点のプレフィックス全量が新しい書き込みになる。
//! thinking / system / tools / messages の中身にも触らない (DR-0024 の禁則。
//! 過去の thinking 署名が無効化される)。
//!
//! 判断は純粋関数で、状態を持たない。1 本の転送で経路を何度も試すので、
//! どの試行でも同じ本文になる。

pub mod keepalive;

use serde_json::{Map, Value};

use crate::config::{CacheRule, CacheStrategy};
use crate::provider::RequestOrigin;

/// この呼び出し元に効く戦略。
///
/// 見分けが付かなかった呼び出し元は main として扱う (DR-0024)。サブエージェント
/// 向けの戦略を当てると、メインの会話に効かせるつもりのない扱いが本流へ及ぶ。
pub fn strategy_of(rule: &CacheRule, origin: RequestOrigin) -> CacheStrategy {
    match origin {
        RequestOrigin::Sub => rule.sub,
        RequestOrigin::Main | RequestOrigin::Unknown => rule.main,
    }
}

/// 戦略どおりに `cache_control` を整える。
///
/// `keepalive` の本文は `1h` と同じに整える。違うのは、会話が止まったときに
/// 合図を出して cache を継ぎ足すかどうかだけ (DR-0024 §2)。
pub fn apply(body: &mut Value, strategy: CacheStrategy) {
    match strategy {
        CacheStrategy::Passthrough => {}
        CacheStrategy::None => visit(body, &mut |holder| {
            holder.remove("cache_control");
        }),
        CacheStrategy::FiveMinutes => visit(body, &mut |holder| {
            if let Some(control) = control_of(holder) {
                control.remove("ttl");
            }
        }),
        CacheStrategy::OneHour | CacheStrategy::Keepalive => apply_one_hour(body),
    }
}

/// 既にあるブレークポイント全部に 1 時間を指定する。
fn apply_one_hour(body: &mut Value) {
    visit(body, &mut |holder| {
        if let Some(control) = control_of(holder) {
            control.insert("ttl".to_owned(), Value::String("1h".to_owned()));
        }
    });
}

/// この本文が残すプレフィックスの寿命 (秒)。cache に載らない 1 本では `None`。
///
/// 戦略で決め打ちにできるものはそれで答え、本文に触らない `passthrough`
/// (と規則の無い namespace) だけ、送る本文のブレークポイントを読む —
/// `ttl: "1h"` がひとつでもあれば 1 時間、`ttl` の無いブレークポイントだけなら
/// 既定の 5 分、ブレークポイントが無ければ載らない。
///
/// 見る側が「あと何分もつか」を描くための値 (DR-0012)。`prefix` と同じで、
/// **キャッシュに当たる保証ではない** — プレフィックスが変われば実際には
/// 効かない。
pub fn ttl_secs(strategy: Option<CacheStrategy>, body: &Value) -> Option<u64> {
    const FIVE_MINUTES: u64 = 5 * 60;
    const ONE_HOUR: u64 = 60 * 60;

    match strategy {
        Some(CacheStrategy::None) => None,
        Some(CacheStrategy::FiveMinutes) => Some(FIVE_MINUTES),
        Some(CacheStrategy::OneHour | CacheStrategy::Keepalive) => Some(ONE_HOUR),
        Some(CacheStrategy::Passthrough) | None => {
            let mut longest = None;
            visit(&mut body.clone(), &mut |holder| {
                let Some(control) = holder.get("cache_control").and_then(Value::as_object) else {
                    return;
                };
                let ttl = match control.get("ttl").and_then(Value::as_str) {
                    Some("1h") => ONE_HOUR,
                    _ => FIVE_MINUTES,
                };
                longest = Some(longest.map_or(ttl, |seen: u64| seen.max(ttl)));
            });
            longest
        }
    }
}

/// `cache_control` を持てる場所を全部なめる。
///
/// 置ける場所は本文の形で決まっている — トップレベル、`system` の各ブロック、
/// 各メッセージの `content` の各ブロック、`tools` の各要素。持っていない場所も
/// 渡すので、付け外しの判断は呼び出し側が 1 箇所で書ける。
pub(crate) fn visit(body: &mut Value, act: &mut dyn FnMut(&mut Map<String, Value>)) {
    let Some(root) = body.as_object_mut() else {
        return;
    };
    act(root);
    for field in ["system", "tools"] {
        if let Some(items) = root.get_mut(field).and_then(Value::as_array_mut) {
            for item in items {
                if let Some(item) = item.as_object_mut() {
                    act(item);
                }
            }
        }
    }
    let Some(messages) = root.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(message) = message.as_object_mut() else {
            continue;
        };
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                if let Some(block) = block.as_object_mut() {
                    act(block);
                }
            }
        }
    }
}

/// この場所に付いているブレークポイント。付いていなければ `None`。
fn control_of(holder: &mut Map<String, Value>) -> Option<&mut Map<String, Value>> {
    holder.get_mut("cache_control")?.as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// ブレークポイントを 4 種類の置き場所すべてに持つ本文。
    fn body() -> Value {
        json!({
            "model": "m",
            "cache_control": {"type": "ephemeral", "ttl": "5m"},
            "system": [
                {"type": "text", "text": "head"},
                {"type": "text", "text": "tail", "cache_control": {"type": "ephemeral"}},
            ],
            "tools": [
                {"name": "Bash", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "now", "cache_control": {"type": "ephemeral"}},
                ]},
                {"role": "assistant", "content": "plain string content"},
            ],
        })
    }

    /// その本文に残っている `cache_control` を、置き場所ごとに数え上げる。
    fn controls(body: &Value) -> Vec<Value> {
        let mut found = Vec::new();
        let mut body = body.clone();
        visit(&mut body, &mut |holder| {
            if let Some(control) = holder.get("cache_control") {
                found.push(control.clone());
            }
        });
        found
    }

    /// 素通しは 1 文字も変えない。
    #[test]
    fn passthrough_changes_nothing() {
        let mut sending = body();
        apply(&mut sending, CacheStrategy::Passthrough);
        assert_eq!(sending, body());
    }

    /// `none` は置き場所を問わず全部剥がす。
    #[test]
    fn none_strips_every_breakpoint() {
        let mut sending = body();
        apply(&mut sending, CacheStrategy::None);

        assert!(controls(&sending).is_empty());
        assert_eq!(
            sending["system"][1]["text"], "tail",
            "only the breakpoint is removed"
        );
        assert_eq!(sending["messages"].as_array().unwrap().len(), 2);
        assert_eq!(sending["tools"][0]["name"], "Bash");
    }

    /// `5m` は ttl の指定を落とす (= 既定の 5 分に戻す)。型は残す。
    #[test]
    fn five_minutes_drops_the_ttl_of_every_breakpoint() {
        let mut sending = body();
        apply(&mut sending, CacheStrategy::FiveMinutes);

        assert_eq!(
            controls(&sending),
            vec![json!({"type": "ephemeral"}); 4],
            "same four breakpoints, none of them carrying a ttl"
        );
    }

    /// `1h` は全ブレークポイントに 1 時間を書く。無い場所には付けない。
    ///
    /// `keepalive` の本文も同じ — 違うのは合図を出すかどうかだけ (DR-0024 §2)。
    #[test]
    fn one_hour_marks_every_existing_breakpoint() {
        for strategy in [CacheStrategy::OneHour, CacheStrategy::Keepalive] {
            let mut sending = body();
            apply(&mut sending, strategy);

            assert_eq!(
                controls(&sending),
                vec![json!({"type": "ephemeral", "ttl": "1h"}); 4],
                "{}",
                strategy.as_str()
            );
            assert!(
                sending["system"][0].get("cache_control").is_none(),
                "a place without a breakpoint does not gain one"
            );
        }
    }

    /// ブレークポイントの数と位置は、どの戦略でも動かさない (DR-0024 の禁則)。
    #[test]
    fn no_strategy_moves_a_breakpoint() {
        let places = |body: &Value| {
            let mut found = Vec::new();
            let mut body = body.clone();
            let mut at = 0;
            visit(&mut body, &mut |holder| {
                if holder.contains_key("cache_control") {
                    found.push(at);
                }
                at += 1;
            });
            found
        };
        for strategy in [
            CacheStrategy::Passthrough,
            CacheStrategy::FiveMinutes,
            CacheStrategy::OneHour,
            CacheStrategy::Keepalive,
        ] {
            let mut sending = body();
            apply(&mut sending, strategy);
            assert_eq!(places(&sending), places(&body()), "{}", strategy.as_str());
        }
    }

    /// 相手の本文が想定の形でなくても落ちない。
    #[test]
    fn an_unexpected_body_is_left_alone() {
        for shape in [
            json!(null),
            json!("string"),
            json!({"system": "old style"}),
            json!({"messages": [null, 42, {"content": "plain"}]}),
            json!({"tools": [null], "cache_control": "not an object"}),
        ] {
            for strategy in [
                CacheStrategy::None,
                CacheStrategy::FiveMinutes,
                CacheStrategy::OneHour,
            ] {
                let mut sending = shape.clone();
                apply(&mut sending, strategy);
            }
        }
    }

    /// サブエージェントだけ別の戦略に振れる。見分けが付かなければ main の扱い。
    #[test]
    fn the_caller_decides_which_strategy_applies() {
        let rule = CacheRule {
            models: vec!["*".to_owned()],
            main: CacheStrategy::OneHour,
            sub: CacheStrategy::None,
            keepalive_horizon: None,
        };

        assert_eq!(
            strategy_of(&rule, RequestOrigin::Main),
            CacheStrategy::OneHour
        );
        assert_eq!(strategy_of(&rule, RequestOrigin::Sub), CacheStrategy::None);
        assert_eq!(
            strategy_of(&rule, RequestOrigin::Unknown),
            CacheStrategy::OneHour,
            "an unrecognized caller is treated as the main conversation"
        );
    }

    /// 戦略を書いた 1 本の寿命は、戦略だけで決まる。
    #[test]
    fn the_strategy_decides_how_long_the_prefix_lives() {
        for (strategy, want) in [
            (CacheStrategy::FiveMinutes, Some(300)),
            (CacheStrategy::OneHour, Some(3600)),
            (CacheStrategy::Keepalive, Some(3600)),
            (CacheStrategy::None, None),
        ] {
            let mut sending = body();
            apply(&mut sending, strategy);
            assert_eq!(
                ttl_secs(Some(strategy), &sending),
                want,
                "{}",
                strategy.as_str()
            );
        }
    }

    /// 本文に触らない 1 本の寿命は、クライアントが書いたブレークポイントで決まる。
    #[test]
    fn an_untouched_body_answers_for_itself() {
        let hour = json!({"system": [
            {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "b", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
        ]});
        let five = json!({"system": [
            {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
        ]});
        let none = json!({"system": [{"type": "text", "text": "a"}]});

        for strategy in [Some(CacheStrategy::Passthrough), None] {
            assert_eq!(
                ttl_secs(strategy, &hour),
                Some(3600),
                "one hour anywhere is the life of the prefix"
            );
            assert_eq!(ttl_secs(strategy, &five), Some(300));
            assert_eq!(
                ttl_secs(strategy, &none),
                None,
                "a body with no breakpoint leaves nothing behind"
            );
        }
    }
}
