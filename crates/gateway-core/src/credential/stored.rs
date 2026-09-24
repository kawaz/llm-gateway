//! 保存される認証情報の形。
//!
//! 2 層に分ける。**トップ**は人が触ってよい層 — 運用の設定と、gateway が
//! 観測して書き戻した値。**payload** は上流が返した認証情報そのままで、人が
//! 手で直すものではない。平坦に混ぜると、どれを直してよいのか読み手に区別が
//! 付かない。
//!
//! トップのうち、どの置き方でも意味を持つのは `priority` / `disabled` だけ。
//! それ以外 (更新の記録や、上流ごとの運用の設定) は利用側の拡張欄 `X` が持つ。
//! 更新の記録を書くのは更新する側だけで、更新の無い秘密 (固定の鍵など) には
//! 意味が無いため。

use serde::{Deserialize, Serialize, Serializer};

/// payload の種別と中身。
///
/// 保存ファイルでは `type` をトップの先頭に、中身を末尾の `payload` に置く。
/// 読み手が先に見たいのは触ってよい層で、payload は最後でよい。
pub trait TaggedPayload {
    /// 保存ファイルに書く `type`。
    fn type_name(&self) -> &'static str;

    /// `payload` 欄の中身を書く。
    fn serialize_body<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error>;

    /// `priority` / `disabled` が既定値 (`0` / `false`) なら書き出しで省くか。
    ///
    /// 使い分けの無い種別 (固定の秘密など) では、この 2 つは意味を持たない。
    /// 読むときは省略を既定値で受けるので、省いて書けば人が置いた最小の形の
    /// ままバイト単位で書き戻せる。
    const OMITS_DEFAULT_TOP: bool = false;
}

/// ディスク上の認証情報。`X` はトップの拡張欄、`P` は payload。
///
/// 読み込みは派生のまま (`type` と `payload` は `P` が読む)。書き出しは
/// キーの並びを固定するため [`Serialize`] を手で書く。
#[derive(Debug, Clone, Deserialize)]
pub struct StoredCredential<X, P> {
    /// 小さいほど先に選ばれる。
    #[serde(default)]
    pub priority: i32,

    /// true の間は選択対象から外す。
    #[serde(default)]
    pub disabled: bool,

    /// 利用側が持つトップの欄。
    #[serde(flatten)]
    pub ext: X,

    /// 上流から受け取った認証情報。
    #[serde(flatten)]
    pub payload: P,
}

impl<X: Default, P> StoredCredential<X, P> {
    /// 運用の設定を既定にして作る。初めての login で使う。
    pub fn new(payload: P) -> Self {
        Self {
            priority: 0,
            disabled: false,
            ext: X::default(),
            payload,
        }
    }
}

/// 並びは `type`、`priority`、`disabled`、拡張欄、`payload`。
impl<X: Serialize, P: TaggedPayload> Serialize for StoredCredential<X, P> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a, X, B> {
            #[serde(rename = "type")]
            type_name: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            priority: Option<i32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            disabled: Option<bool>,
            #[serde(flatten)]
            ext: &'a X,
            payload: B,
        }

        struct Body<'a, P>(&'a P);

        impl<P: TaggedPayload> Serialize for Body<'_, P> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.serialize_body(serializer)
            }
        }

        Out {
            type_name: self.payload.type_name(),
            priority: (!P::OMITS_DEFAULT_TOP || self.priority != 0).then_some(self.priority),
            disabled: (!P::OMITS_DEFAULT_TOP || self.disabled).then_some(self.disabled),
            ext: &self.ext,
            payload: Body(&self.payload),
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    struct Note {
        #[serde(default)]
        label: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type", content = "payload", rename_all = "snake_case")]
    enum Secret {
        Fixed(Key),
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Key {
        key: String,
    }

    impl TaggedPayload for Secret {
        fn type_name(&self) -> &'static str {
            match self {
                Self::Fixed(_) => "fixed",
            }
        }
        fn serialize_body<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self {
                Self::Fixed(k) => k.serialize(serializer),
            }
        }
    }

    type Stored = StoredCredential<Note, Secret>;

    #[test]
    fn writes_type_first_and_payload_last() {
        let mut c = Stored::new(Secret::Fixed(Key { key: "k".into() }));
        c.priority = 3;
        c.ext.label = "l".into();
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"type":"fixed","priority":3,"disabled":false,"label":"l","payload":{"key":"k"}}"#
        );
    }

    #[test]
    fn round_trips_and_defaults_the_top() {
        let c: Stored = serde_json::from_str(r#"{"type":"fixed","payload":{"key":"k"}}"#).unwrap();
        assert_eq!(c.priority, 0);
        assert!(!c.disabled);
        assert_eq!(c.ext, Note::default());
        let again: Stored = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(again.payload, c.payload);
    }
}
