//! The addresses an account sends as (RFC 8621 section 6).

use serde::Deserialize;
use serde_json::json;

use crate::session::SUBMISSION;
use crate::{JmapClient, JmapError, Request};

/// A name and an address, as JMAP writes them in `replyTo` and `bcc`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EmailAddress {
    #[serde(default)]
    pub name: Option<String>,
    /// Group syntax can carry a name with no address.
    #[serde(default)]
    pub email: String,
}

/// One address the account may send as, with its signatures.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub reply_to: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub bcc: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub text_signature: String,
    #[serde(default)]
    pub html_signature: String,
    #[serde(default)]
    pub may_delete: bool,
}

#[derive(Deserialize)]
struct IdentityList {
    list: Vec<Identity>,
}

/// Every identity of the submission account, as the server orders them.
pub async fn identities(client: &JmapClient) -> Result<Vec<Identity>, JmapError> {
    let session = client.session().await?;
    let account = session
        .account_for(SUBMISSION)
        .ok_or_else(|| JmapError::UnknownCapability(SUBMISSION.into()))?;
    let mut request = Request::new();
    let id = request.call(SUBMISSION, "Identity/get", json!({ "accountId": account, "ids": null }));
    let mut answer = client.send(&request).await?;
    Ok(answer.take::<IdentityList>(&id, "Identity/get")?.list)
}
