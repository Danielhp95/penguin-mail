//! Read receipts over JMAP (RFC 9007). The app always sends its own
//! translated subject and text: Apache James fills in English words of
//! its own when the client gives none.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::mail::SetError;
use crate::session::MAIL;
use crate::{JmapClient, JmapError, Request};

pub const MDN: &str = "urn:ietf:params:jmap:mdn";

/// A receipt saying the message `for_email_id` was shown, sent by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnOut {
    pub for_email_id: String,
    pub identity_id: String,
    pub subject: String,
    pub text_body: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    #[serde(default)]
    sent: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    not_sent: Option<BTreeMap<String, SetError>>,
}

impl JmapClient {
    /// Sends the receipt and marks the message `$mdnsent` in the same
    /// request.
    pub async fn send_mdn(&self, mdn: &MdnOut) -> Result<(), JmapError> {
        let session = self.session().await?;
        let account = session
            .account_for(MDN)
            .ok_or_else(|| JmapError::UnknownCapability(MDN.into()))?
            .to_string();
        let mut request = Request::new();
        request.also_using(MAIL);
        let sent = request.call(MDN, "MDN/send", json!({
            "accountId": account,
            "identityId": mdn.identity_id,
            "send": { "k": {
                "forEmailId": mdn.for_email_id,
                "subject": mdn.subject,
                "textBody": mdn.text_body,
                "includeOriginalMessage": false,
                "disposition": { "actionMode": "manual-action", "sendingMode": "mdn-sent-manually", "type": "displayed" }
            } },
            "onSuccessUpdateEmail": { "#k": { "keywords/$mdnsent": true } }
        }));
        let mut answer = self.send(&request).await?;
        let said: Answer = answer.take(&sent, "MDN/send")?;
        if let Some(refused) = said.not_sent.as_ref().and_then(|n| n.get("k")) {
            // The server's description is its own English; keep only the kind.
            return Err(JmapError::Method { kind: refused.kind.clone(), description: String::new() });
        }
        match said.sent.as_ref().is_some_and(|s| s.contains_key("k")) {
            true => Ok(()),
            false => Err(JmapError::Decode("the server said nothing about the receipt".into())),
        }
    }
}
