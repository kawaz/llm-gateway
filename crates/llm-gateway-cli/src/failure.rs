//! コマンドが終われなかったときに出すもの (DR-0028 決定 8)。
//!
//! エラーは JSON で stderr に出し、exit を非 0 にする。機械が読む側は
//! `kind` を見れば分岐でき、人は `message` を読めばよい。

use serde_json::{Map, Value, json};

/// 出せなかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    kind: String,
    message: String,
    details: Map<String, Value>,
}

impl Failure {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            details: Map::new(),
        }
    }

    /// `kind` と `message` だけでは足りないものを添える。
    ///
    /// 「どれを指せばよいのか」(登録されている unit 名など) は、人にも機械にも
    /// 次の一手そのものなので、文章に埋めずに欄として渡す。
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details.insert(key.to_owned(), value.into());
        self
    }

    /// 中身を覗くのは試験だけ。動く側は [`Self::to_json`] で丸ごと出す。
    #[cfg(test)]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[cfg(test)]
    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn to_json(&self) -> String {
        let mut error = Map::new();
        error.insert("kind".to_owned(), json!(self.kind));
        error.insert("message".to_owned(), json!(self.message));
        error.extend(self.details.clone());
        serde_json::to_string(&json!({ "error": Value::Object(error) })).unwrap_or_else(|e| {
            format!("{{\"error\":{{\"kind\":\"unprintable\",\"message\":{e}}}}}")
        })
    }
}

/// 種別を名乗れない失敗。
///
/// 既にある命令の多くは文章で断っており、その全部に種別を付け直すのは
/// DR-0028 の範囲ではない。JSON の形だけは揃えて出す。
pub const GENERIC: &str = "error";

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new(GENERIC, message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::new(GENERIC, message)
    }
}

impl From<llm_gateway::daemon::registry::Error> for Failure {
    fn from(e: llm_gateway::daemon::registry::Error) -> Self {
        use llm_gateway::daemon::registry::Error as E;
        let failure = Self::new(e.kind(), e.to_string());
        match e {
            E::UnknownUnit { units, .. } => failure.with("units", units),
            _ => failure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_one_json_object_with_its_kind() {
        let json = Failure::new("unit_required", "say which unit to ask").to_json();
        assert_eq!(
            serde_json::from_str::<Value>(&json).unwrap(),
            json!({"error": {"kind": "unit_required", "message": "say which unit to ask"}})
        );
    }

    /// 添えた欄は `error` の中に並ぶ (別の入れ物を作らない)。
    #[test]
    fn details_sit_next_to_the_kind() {
        let json = Failure::new("unit_required", "say which unit")
            .with("units", vec!["stable".to_owned(), "unstable".to_owned()])
            .to_json();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["error"]["units"], json!(["stable", "unstable"]));
    }

    /// 文章だけの失敗も同じ形で出る。読む側が 2 種類を覚えずに済む。
    #[test]
    fn a_plain_message_still_becomes_json() {
        let failure = Failure::from("could not understand `--nope`".to_owned());
        assert_eq!(failure.kind(), GENERIC);
        let value: Value = serde_json::from_str(&failure.to_json()).unwrap();
        assert_eq!(
            value["error"]["message"],
            json!("could not understand `--nope`")
        );
    }

    #[test]
    fn a_missing_unit_carries_the_names_that_do_exist() {
        let failure = Failure::from(llm_gateway::daemon::registry::Error::UnknownUnit {
            name: "nope".to_owned(),
            units: vec!["stable".to_owned()],
        });
        assert_eq!(failure.kind(), "unknown_unit");
        let value: Value = serde_json::from_str(&failure.to_json()).unwrap();
        assert_eq!(value["error"]["units"], json!(["stable"]));
    }
}
