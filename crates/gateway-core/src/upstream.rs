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
    /// パスが上流で別の場所へ読み替わりうる形をしている (404)。
    UnsafePath,
}

/// `<rest>` のパスが、上流で別の場所へ読み替わらない形か。
///
/// 許可の判定は受けたパスの文字列で行うので、上流 (や途中の URL 処理) が
/// 正規化すると判定と実際の行き先がずれる (`/v1/../admin` は `/admin` に届く)。
/// 正規化して通すのでなく、読み替わりうる形は全部断る方が単純で安全。
///
/// 断るもの: 途中の空の段 (`//`)、`.` / `..` の段、`\`、段の中の
/// percent-encoded な `/` `\\` `.` (`%2F` / `%5C` / `%2E`、大小どちらも)。
/// 末尾の `/` は API の形としてありうるので通す。
pub fn safe_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    if path.contains('\\') {
        return false;
    }
    let segments: Vec<&str> = rest.split('/').collect();
    let last = segments.len() - 1;
    segments.iter().enumerate().all(|(i, seg)| {
        let lowered = seg.to_ascii_lowercase();
        !(seg.is_empty() && i != last)
            && *seg != "."
            && *seg != ".."
            && !lowered.contains("%2f")
            && !lowered.contains("%5c")
            && !lowered.contains("%2e")
    })
}

impl UpstreamSpec {
    /// `<rest>` のパス部分 (クエリを除く) と method で判定する。
    pub fn decide(&self, method: &str, path: &str) -> Decision {
        if !safe_path(path) {
            return Decision::UnsafePath;
        }
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

    /// 上流へ出す URL。`path` は [`safe_path`] を通った `<rest>` のパス、
    /// `query` は `?` を除いたクエリ (無ければ `None`)。
    ///
    /// パスとクエリを別々に置くので、`?` や `#` がパスに混ざって別の URL に
    /// ならない (パスの側では符号化される)。段の中の percent-encoding は
    /// そのまま運ぶ。
    pub fn target(&self, path: &str, query: Option<&str>) -> Result<url::Url, String> {
        let mut url = self.parsed_url()?;
        let joined = format!("{}{path}", url.path().trim_end_matches('/'));
        url.set_path(&joined);
        url.set_query(query);
        Ok(url)
    }

    /// 上流の起点を検査する。scheme は `http` / `https`、host が要る。
    /// userinfo・クエリ・fragment は書けない (path prefix は書ける)。
    pub fn check_url(&self) -> Result<(), String> {
        self.parsed_url().map(|_| ())
    }

    fn parsed_url(&self) -> Result<url::Url, String> {
        let url = url::Url::parse(&self.url)
            .map_err(|e| format!("upstream url `{}` is not a URL: {e}", self.url))?;
        let refuse = |why: &str| Err(format!("upstream url `{}` {why}", self.url));
        if !matches!(url.scheme(), "http" | "https") {
            return refuse("must use http or https");
        }
        if url.host_str().is_none_or(str::is_empty) {
            return refuse("must have a host");
        }
        if !url.username().is_empty() || url.password().is_some() {
            return refuse("must not carry a user name or password (put the secret in `secret`)");
        }
        if url.query().is_some() {
            return refuse("must not have a query string");
        }
        if url.fragment().is_some() {
            return refuse("must not have a fragment");
        }
        Ok(url)
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
            s.target("/v1/items", Some("x=1")).unwrap().as_str(),
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

    /// 上流で別の場所へ読み替わりうるパスは、許可の判定より前に断る。
    #[test]
    fn unsafe_paths_are_refused_before_the_allow_list() {
        let s = spec(&SAMPLE.replace("post /v1/items/*", "GET /v1/*")).unwrap();
        for bad in [
            "/v1/../admin",
            "/v1/%2e%2e/admin",
            "/v1/%2E/x",
            "/v1//x",
            "/v1/a%2Fb",
            "/v1/a%5cb",
            "/v1/./x",
            "/v1/a\\b",
        ] {
            assert_eq!(s.decide("GET", bad), Decision::UnsafePath, "{bad}");
        }
        assert_eq!(
            s.decide("GET", "/v1/x/"),
            Decision::Allowed,
            "a trailing slash is fine"
        );
        assert_eq!(s.decide("GET", "/v1/a%20b"), Decision::Allowed);
    }

    /// 組み立てた URL のパスに `?` や `#` が混ざらない。path prefix は残る。
    #[test]
    fn the_target_is_built_by_segments() {
        let mut s = spec(SAMPLE).unwrap();
        s.url = "https://api.example.com/base/".into();
        let url = s.target("/v1/a%20b", Some("q=1&r=2")).unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.example.com/base/v1/a%20b?q=1&r=2"
        );
        assert_eq!(
            s.target("/v1/x?y#z", None).unwrap().as_str(),
            "https://api.example.com/base/v1/x%3Fy%23z"
        );
    }

    #[test]
    fn the_origin_is_checked() {
        let mut s = spec(SAMPLE).unwrap();
        for bad in [
            "https://api.example/x?y=",
            "https://user:pw@host/",
            "https://api.example/#f",
            "ftp://api.example/",
            "not a url",
        ] {
            s.url = bad.into();
            assert!(s.check_url().is_err(), "{bad}");
        }
        s.url = "http://127.0.0.1:9/prefix".into();
        assert!(s.check_url().is_ok());
    }
}
