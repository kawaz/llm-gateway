//! 名前の照合。
//!
//! `alpha-beta-*` のような書き方を、除外リスト・ルーティング・エイリアスで使う。正規表現を持ち出すほどの用途ではないので、`*` だけ扱う。

/// `*` を任意の並びとして照合する。`*` は何個あってもよい。
///
/// 大小は区別する。モデル名は小文字で固定されているので、無視すると
/// 意図しないものを拾う方が怖い。
pub fn matches(pattern: &str, name: &str) -> bool {
    // `*` が無ければ全体が一致しなければならない。
    let Some((head, tail_patterns)) = pattern.split_once('*') else {
        return pattern == name;
    };

    // 先頭は前方一致。
    let Some(mut rest) = name.strip_prefix(head) else {
        return false;
    };

    let parts: Vec<&str> = tail_patterns.split('*').collect();
    let (last, middle) = parts
        .split_last()
        .expect("split always returns at least one element");

    // 中間の断片は、順に現れればよい。
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }

    // 末尾の断片は後方一致 (`*` で終わるなら空文字なので必ず真)。
    rest.len() >= last.len() && rest.ends_with(last)
}

/// どれか 1 つでも当たるか。
pub fn matches_any<S: AsRef<str>>(patterns: &[S], name: &str) -> bool {
    patterns.iter().any(|p| matches(p.as_ref(), name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_name() {
        assert!(matches("vendor-large-5", "vendor-large-5"));
        assert!(!matches("vendor-large-5", "vendor-large-4-8"));
        assert!(!matches("vendor-large-5", "vendor-large-50"));
    }

    /// 実際に使う形 (cpa の除外リストと同じ書き方)。
    #[test]
    fn trailing_wildcard() {
        assert!(matches("vendor-large-*", "vendor-large-5"));
        assert!(matches("vendor-large-*", "vendor-large-4-1-20250805"));
        assert!(!matches("vendor-large-*", "vendor-medium-5"));
    }

    /// `vendor-large-4*` は 4 系だけを狙う (5 を巻き込まない)。
    #[test]
    fn version_prefix_does_not_over_match() {
        assert!(matches("vendor-large-4*", "vendor-large-4-8"));
        assert!(matches("vendor-large-4*", "vendor-large-4-1-20250805"));
        assert!(!matches("vendor-large-4*", "vendor-large-5"));
    }

    #[test]
    fn leading_wildcard() {
        assert!(matches("*-20250805", "vendor-large-4-1-20250805"));
        assert!(!matches("*-20250805", "vendor-large-5"));
    }

    #[test]
    fn surrounding_wildcards() {
        assert!(matches("*large*", "vendor-large-5"));
        assert!(!matches("*large*", "vendor-medium-5"));
    }

    #[test]
    fn multiple_wildcards() {
        assert!(matches("vendor-*-4-*", "vendor-large-4-8"));
        assert!(matches("vendor-*-4-*", "vendor-medium-4-5-20250929"));
        assert!(!matches("vendor-*-4-*", "vendor-large-5"));
    }

    #[test]
    fn bare_wildcard_matches_everything() {
        assert!(matches("*", "vendor-large-5"));
        assert!(matches("*", ""));
    }

    /// 同じ断片が複数回出てもよい。
    #[test]
    fn repeated_fragment() {
        assert!(matches("a*a*a", "abacada"));
        assert!(!matches("a*a*a", "aba"));
    }

    #[test]
    fn empty_inputs() {
        assert!(matches("", ""));
        assert!(!matches("", "x"));
        assert!(!matches("x", ""));
    }

    #[test]
    fn case_is_significant() {
        assert!(!matches("vendor-large-*", "Vendor-Large-5"));
    }

    /// 実運用の除外リスト (cpa の設定から)。
    #[test]
    fn realistic_exclusion_list() {
        let excluded = ["vendor-3-*", "vendor-large-4*", "vendor-medium-4-*"];

        for hidden in [
            "vendor-3-5-medium-20241022",
            "vendor-large-4-8",
            "vendor-large-4-1-20250805",
            "vendor-medium-4-6",
        ] {
            assert!(matches_any(&excluded, hidden), "{hidden} should be hidden");
        }
        for shown in ["vendor-large-5", "vendor-medium-5", "vendor-small-5"] {
            assert!(!matches_any(&excluded, shown), "{shown} should be shown");
        }
    }

    #[test]
    fn empty_pattern_list_matches_nothing() {
        let empty: [&str; 0] = [];
        assert!(!matches_any(&empty, "vendor-large-5"));
    }
}
