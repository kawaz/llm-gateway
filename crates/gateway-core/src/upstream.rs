//! 無変換で中継する行き先 (DR-0030 §2 / §4)。
//!
//! 行き先ごとに「上流の起点 + 固定の秘密 + 許可する endpoint」を設定で
//! 登録する。`/ns-<ns>/<name>/<rest>` の `<rest>` を上流の起点へそのまま
//! 連結し、認証だけを登録済みのものへ差し替えて流す。対象は設定に書いた
//! ものだけで、任意の URL を中継する口にはしない。
//!
//! ここが持つのは設定の形と「通してよいか」の判定だけ。実際に流すのは
//! 利用側 (HTTP の骨格) の仕事。

use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

/// 1 つの行き先。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSpec {
    /// 上流の起点。`<rest>` をここへそのまま連結する (path prefix を含めてよい)。
    pub url: String,
    /// 載せる固定の秘密の識別子 (置き場のファイル名の stem)。
    pub secret: String,
    /// 秘密の載せ方。
    pub auth: AuthPlacement,
    /// 通してよい `METHOD パス` の並び。パスは `*` を含むパターン。
    pub allow: Vec<Allow>,
}

/// 秘密の載せ方。載せ方は上流 API の形で決まり、秘密の値とは別物なので
/// 行き先の側に書く。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum AuthPlacement {
    /// `Authorization: Bearer <秘密>`。設定では `"bearer"`。
    #[serde(serialize_with = "bearer_word")]
    Bearer,
    /// 指定したヘッダに秘密をそのまま載せる。設定では `{ header = "x-api-key" }`。
    Header { header: String },
}

fn bearer_word<S: serde::Serializer>(s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str("bearer")
}

impl<'de> Deserialize<'de> for AuthPlacement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Word(String),
            Header { header: String },
        }
        match Raw::deserialize(deserializer).map_err(|_| {
            de::Error::custom(r#"auth must be "bearer" or { header = "<header name>" }"#)
        })? {
            Raw::Word(word) if word == "bearer" => Ok(Self::Bearer),
            Raw::Word(word) => Err(de::Error::custom(format!(
                r#"unknown auth `{word}`. Write "bearer" or {{ header = "<header name>" }}"#
            ))),
            Raw::Header { header } if header.trim().is_empty() => {
                Err(de::Error::custom("auth.header must not be empty"))
            }
            Raw::Header { header } => Ok(Self::Header { header }),
        }
    }
}

/// 通してよい 1 組。設定では `"POST /v1/chat/*"` の 1 文字列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allow {
    /// 大文字の HTTP method。
    pub method: String,
    /// `/` で始まるパスのパターン (`*` 可)。
    pub path: String,
}

impl fmt::Display for Allow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.method, self.path)
    }
}

impl Serialize for Allow {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Allow {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(de::Error::custom)
    }
}

impl std::str::FromStr for Allow {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, String> {
        let hint =
            || format!("allow entry `{raw}` must be `METHOD /path` (for example `GET /v1/items`)");
        let (method, path) = raw.trim().split_once(' ').ok_or_else(hint)?;
        let path = path.trim();
        if method.is_empty()
            || !method.bytes().all(|b| b.is_ascii_alphabetic())
            || !path.starts_with('/')
        {
            return Err(hint());
        }
        Ok(Self {
            method: method.to_ascii_uppercase(),
            path: path.to_owned(),
        })
    }
}

/// 通してよいかの判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// どのパスにも当たらない (404)。
    NotFound,
    /// パスは当たるが method が違う (405)。
    MethodNotAllowed,
}

impl UpstreamSpec {
    /// `<rest>` のパス部分 (クエリを除く) と method で判定する。
    pub fn decide(&self, method: &str, path: &str) -> Decision {
        let mut path_matched = false;
        for allow in &self.allow {
            if crate::pattern::matches(&allow.path, path) {
                if allow.method.eq_ignore_ascii_case(method) {
                    return Decision::Allowed;
                }
                path_matched = true;
            }
        }
        if path_matched {
            Decision::MethodNotAllowed
        } else {
            Decision::NotFound
        }
    }

    /// 上流へ出す URL。`rest` は `/` で始まるパスとクエリ。
    pub fn target(&self, rest: &str) -> String {
        format!("{}{rest}", self.url.trim_end_matches('/'))
    }
}

/// 行き先の名前を検査する。`reserved` は利用側が URL の同じ段で使っている名前。
///
/// 名前は URL の 1 段になるので、`/` や空白を含めない。既定は上流の FQDN を
/// そのまま書く慣習で、任意のラベルも書ける。
pub fn check_name(name: &str, reserved: &[&str]) -> Result<(), String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "upstream name `{name}` may contain only letters, digits, `.`, `_` and `-`"
        ));
    }
    if reserved.contains(&name) {
        return Err(format!(
            "upstream name `{name}` is reserved (reserved: {}). Choose another label",
            reserved.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(toml_text: &str) -> Result<UpstreamSpec, toml::de::Error> {
        toml::from_str(toml_text)
    }

    const SAMPLE: &str = r#"
url = "https://api.example.com/"
secret = "ex"
auth = "bearer"
allow = ["GET /v1/items", "post /v1/items/*"]
"#;

    #[test]
    fn reads_the_documented_shape() {
        let s = spec(SAMPLE).unwrap();
        assert_eq!(s.auth, AuthPlacement::Bearer);
        assert_eq!(s.allow[1].method, "POST");
        assert_eq!(
            s.target("/v1/items?x=1"),
            "https://api.example.com/v1/items?x=1"
        );

        let h = spec(&SAMPLE.replace(r#""bearer""#, r#"{ header = "x-api-key" }"#)).unwrap();
        assert_eq!(
            h.auth,
            AuthPlacement::Header {
                header: "x-api-key".into()
            }
        );
    }

    #[test]
    fn decides_404_405_and_allowed() {
        let s = spec(SAMPLE).unwrap();
        assert_eq!(s.decide("GET", "/v1/items"), Decision::Allowed);
        assert_eq!(s.decide("POST", "/v1/items/9"), Decision::Allowed);
        assert_eq!(s.decide("DELETE", "/v1/items"), Decision::MethodNotAllowed);
        assert_eq!(s.decide("GET", "/v1/other"), Decision::NotFound);
    }

    #[test]
    fn bad_entries_say_what_to_write() {
        for bad in [
            r#"auth = "basic""#,
            r#"auth = { header = "" }"#,
            r#"allow = ["GET v1"]"#,
            r#"allow = ["/v1"]"#,
        ] {
            let text = SAMPLE
                .lines()
                .map(|l| {
                    let key = bad.split(' ').next().unwrap();
                    if l.starts_with(key) { bad } else { l }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(spec(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn names_are_checked() {
        let reserved = ["v1", "gw"];
        assert!(check_name("api.example.com", &reserved).is_ok());
        assert!(check_name("my_label-2", &reserved).is_ok());
        for bad in ["", "a/b", "a b", "v1", "gw"] {
            assert!(check_name(bad, &reserved).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn writes_back_the_same_words() {
        let s = spec(SAMPLE).unwrap();
        let again: UpstreamSpec = toml::from_str(&toml::to_string(&s).unwrap()).unwrap();
        assert_eq!(again, s);
    }
}
