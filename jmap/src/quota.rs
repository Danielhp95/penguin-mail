//! Quotas (RFC 9425): what the server lets the account keep, and how much
//! of it is used. The app shows one number per account, the bytes its
//! mail may take.

use serde::Deserialize;
use serde_json::json;

use crate::{JmapClient, JmapError, Request};

pub const QUOTA: &str = "urn:ietf:params:jmap:quota";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    pub id: String,
    /// `octets` or `count`.
    pub resource_type: String,
    /// `account`, `domain` or `global`.
    pub scope: String,
    pub used: u64,
    pub hard_limit: u64,
    #[serde(default)]
    pub warn_limit: Option<u64>,
    /// The data types it counts, such as `Mail`; none means every type.
    #[serde(default)]
    pub types: Vec<String>,
}

#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    list: Vec<Quota>,
}

/// The quota that bounds the account's mail in bytes: `octets`, at account
/// scope, counting Mail or every type, with a limit. Of several, the one
/// with the least room left, since that is the one mail bounces off.
pub fn mail_storage(quotas: &[Quota]) -> Option<&Quota> {
    quotas
        .iter()
        .filter(|q| q.resource_type == "octets" && q.scope == "account" && q.hard_limit > 0)
        .filter(|q| q.types.is_empty() || q.types.iter().any(|t| t == "Mail"))
        .min_by_key(|q| q.hard_limit.saturating_sub(q.used))
}

impl JmapClient {
    /// Every quota the session's quota account answers for.
    pub async fn quotas(&self) -> Result<Vec<Quota>, JmapError> {
        let session = self.session().await?;
        let account = session
            .account_for(QUOTA)
            .ok_or_else(|| JmapError::UnknownCapability(QUOTA.into()))?
            .to_string();
        let mut request = Request::new();
        let got = request.call(QUOTA, "Quota/get", json!({ "accountId": account, "ids": null }));
        let mut answer = self.send(&request).await?;
        Ok(answer.take::<Listing>(&got, "Quota/get")?.list)
    }
}
