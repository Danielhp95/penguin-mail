//! Fastmail's Masked Email (fastmail.com/dev): addresses made for one
//! site, which the person can turn off or delete. A pending one goes by
//! itself after a day unless mail arrives.

use serde::Deserialize;
use serde_json::json;

use crate::mail::{Got, SetResponse};
use crate::session::MASKED_EMAIL;
use crate::{JmapClient, JmapError, Request};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaskedEmail {
    pub id: String,
    pub email: String,
    /// `pending`, `enabled`, `disabled` or `deleted`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub for_domain: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_message_at: Option<String>,
}

/// The states a person can set; `pending` is the server's alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskedState {
    Enabled,
    Disabled,
    Deleted,
}

impl MaskedState {
    pub fn as_str(self) -> &'static str {
        match self {
            MaskedState::Enabled => "enabled",
            MaskedState::Disabled => "disabled",
            MaskedState::Deleted => "deleted",
        }
    }
}

impl JmapClient {
    async fn masked_account(&self) -> Result<String, JmapError> {
        let session = self.session().await?;
        session.account_for(MASKED_EMAIL).map(str::to_string).ok_or_else(|| JmapError::UnknownCapability(MASKED_EMAIL.into()))
    }

    pub async fn masked_emails(&self) -> Result<Vec<MaskedEmail>, JmapError> {
        let account = self.masked_account().await?;
        let mut request = Request::new();
        let id = request.call(MASKED_EMAIL, "MaskedEmail/get", json!({ "accountId": account, "ids": null }));
        Ok(self.send(&request).await?.take::<Got<MaskedEmail>>(&id, "MaskedEmail/get")?.list)
    }

    /// Makes an enabled address for `for_domain`, the site it is for.
    pub async fn create_masked(&self, for_domain: &str, description: &str) -> Result<MaskedEmail, JmapError> {
        let account = self.masked_account().await?;
        let mut request = Request::new();
        let id = request.call(MASKED_EMAIL, "MaskedEmail/set", json!({
            "accountId": account,
            "create": { "m": { "state": "enabled", "forDomain": for_domain, "description": description } }
        }));
        let set: SetResponse = self.send(&request).await?.take(&id, "MaskedEmail/set")?;
        if let Some(refused) = set.not_created.as_ref().and_then(|n| n.get("m")) {
            return Err(refused.error());
        }
        let made = set.created.and_then(|mut c| c.remove("m")).ok_or_else(|| JmapError::Decode("no masked address came back".into()))?;
        let mut masked: MaskedEmail = serde_json::from_value(made).map_err(|e| crate::error::decode("MaskedEmail/set", &e))?;
        if masked.for_domain.is_empty() {
            masked.for_domain = for_domain.to_string();
        }
        if masked.description.is_empty() {
            masked.description = description.to_string();
        }
        Ok(masked)
    }

    /// Turns an address on (`enabled`), off (`disabled`) or away
    /// (`deleted`).
    pub async fn set_masked_state(&self, id: &str, state: MaskedState) -> Result<(), JmapError> {
        let account = self.masked_account().await?;
        let mut request = Request::new();
        let call = request.call(MASKED_EMAIL, "MaskedEmail/set", json!({ "accountId": account, "update": { id: { "state": state.as_str() } } }));
        let set: SetResponse = self.send(&request).await?.take(&call, "MaskedEmail/set")?;
        match set.refusal(id) {
            Some(refused) => Err(refused.error()),
            None => Ok(()),
        }
    }
}
