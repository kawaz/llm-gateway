//! 枠をトークンを消費せずに聞く口 (DR-0007)。
//!
//! この口は Anthropic の OAuth にだけあり、Bedrock には無い。「無い」ことは
//! 空実装ではなく [`crate::provider::Preset`] が `None` を返すことで示す。

use crate::credential::Credential;
use crate::egress::BoxFuture;
use crate::limits;
use crate::provider::QuotaApi;
use crate::quota::QuotaLimit;
use crate::{Error, Result};

/// `/api/oauth/usage` で枠を聞く。
pub struct OauthUsage {
    base_url: String,
}

impl OauthUsage {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl QuotaApi for OauthUsage {
    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<Vec<QuotaLimit>>> {
        Box::pin(async move {
            limits::fetch(http, &self.base_url, credential)
                .await
                .ok_or_else(|| Error::Config("quota の応答を取得または解釈できません".to_owned()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 接続先は preset を組むときに決まる (Wire と同じ根を使う)。
    #[test]
    fn asks_the_same_host_as_the_wire() {
        let api = OauthUsage::new("https://api.anthropic.com");
        assert_eq!(api.base_url, "https://api.anthropic.com");
    }

    /// 届かなければ失敗を返す。optional なのは capability の有無であり、
    /// capability を呼んだ後の通信失敗まで成功扱いにはしない。
    #[tokio::test]
    async fn an_unreachable_host_is_an_error() {
        // 誰も待ち受けていない先。接続が拒まれた時点で戻る。
        let api = OauthUsage::new("http://127.0.0.1:1");
        let result = api
            .fetch(&reqwest::Client::new(), &Credential::for_test("tok"))
            .await;

        assert!(result.is_err());
    }
}
