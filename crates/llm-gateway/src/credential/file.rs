//! 平文ファイルへの保存。
//!
//! 置き場の実装は汎用層 ([`gateway_core::credential::file`]) にあり、ここでは
//! 値を [`StoredCredential`] に定める。

use super::StoredCredential;

pub use gateway_core::credential::file::FileGuard;

/// ディレクトリ 1 つを認証情報の置き場にする。1 ファイル 1 [`StoredCredential`]。
pub type FileStore = gateway_core::credential::file::FileStore<StoredCredential>;

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::credential::{CredentialId, Persistence};

    /// 手で書いたファイルを読める (Bedrock は login できないので手で置く)。
    #[test]
    fn reads_a_hand_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        fs::write(
            dir.path().join("bedrock.json"),
            r#"{"type":"bedrock_api_key","priority":5,
                "excluded_models":["claude-opus-*"],
                "payload":{"api_key":"ak","expired":"2026-08-02T10:08:18+09:00"}}"#,
        )
        .unwrap();

        let got = store.load(&CredentialId::new("bedrock")).unwrap();
        assert_eq!(got.payload.secret(), "ak");
        assert_eq!(got.excluded_models, vec!["claude-opus-*"]);
        assert!(!got.accepts_model("claude-opus-5"));
    }

    /// 旧形式 (平坦・cpa 互換) はどのファイルかを言って断る。
    ///
    /// 読めないままだと「なぜ動かないのか」が分からない。login し直せば
    /// 新しい形で書き直される。
    #[test]
    fn legacy_flat_file_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        fs::write(
            dir.path().join("claude-someone.json"),
            r#"{"type":"claude","email":"a@b.c","access_token":"at",
                "refresh_token":"rt","expired":"2026-07-28T02:54:00+09:00",
                "excluded-models":["claude-opus-*"]}"#,
        )
        .unwrap();

        let err = store
            .load(&CredentialId::new("claude-someone"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("claude-someone.json"), "{err}");
    }

    /// 現行形式のファイルを読んで、権利の下で書き戻すとバイト単位で同じになる。
    ///
    /// 置き場の実装を汎用層へ移しても、手元に実在するファイルの形は変えない。
    fn assert_rewrites_identically(name: &str, text: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.json"));
        fs::write(&path, text).unwrap();
        let store = FileStore::open(dir.path()).unwrap();

        let guard = store.lock(&CredentialId::new(name)).unwrap();
        let value = store.reload(&guard).unwrap();
        store.store(&guard, &value).unwrap();
        drop(guard);

        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn claude_oauth_file_is_rewritten_byte_for_byte() {
        assert_rewrites_identically(
            "claude-someone",
            r#"{
  "type": "claude_oauth",
  "priority": 10,
  "disabled": false,
  "excluded_models": [
    "claude-opus-*"
  ],
  "last_refresh": "2026-07-27T18:54:00+09:00",
  "denied_beta_expires_ms": 86400000,
  "denied_beta": {
    "context-1m-2025-08-07": "2026-07-27T18:54:00+09:00"
  },
  "payload": {
    "access_token": "at-1",
    "refresh_token": "rt-1",
    "expired": "2026-07-28T02:54:00+09:00",
    "email": "someone@example.com",
    "scope": "user:inference"
  }
}"#,
        );
    }

    #[test]
    fn codex_oauth_file_is_rewritten_byte_for_byte() {
        assert_rewrites_identically(
            "codex-someone",
            r#"{
  "type": "codex_oauth",
  "priority": 0,
  "disabled": true,
  "excluded_models": [],
  "last_refresh": "",
  "denied_beta_expires_ms": 3600000,
  "denied_beta": {},
  "payload": {
    "access_token": "at-2",
    "refresh_token": "rt-2",
    "expired": "2026-08-01T00:00:00Z",
    "email": "someone@example.com",
    "id_token": "it-2",
    "account_id": "acc-2"
  }
}"#,
        );
    }

    #[test]
    fn bedrock_api_key_file_is_rewritten_byte_for_byte() {
        assert_rewrites_identically(
            "bedrock",
            r#"{
  "type": "bedrock_api_key",
  "priority": 5,
  "disabled": false,
  "excluded_models": [
    "claude-opus-*",
    "claude-haiku-*"
  ],
  "last_refresh": "",
  "denied_beta_expires_ms": 86400000,
  "denied_beta": {},
  "payload": {
    "api_key": "ak",
    "expired": "2026-08-02T10:08:18+09:00",
    "region": "us-east-1"
  }
}"#,
        );
    }
}
