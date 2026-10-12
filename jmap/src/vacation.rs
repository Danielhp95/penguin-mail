//! The automatic reply (RFC 8621 section 8): one object, `singleton`.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::mail::{Got, SetResponse};
use crate::session::VACATION;
use crate::{JmapClient, JmapError, Request};

const SINGLETON: &str = "singleton";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VacationResponse {
    #[serde(default)]
    pub is_enabled: bool,
    #[serde(default)]
    pub from_date: Option<String>,
    #[serde(default)]
    pub to_date: Option<String>,
    /// `None` answers with the original message's subject.
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub text_body: Option<String>,
    #[serde(default)]
    pub html_body: Option<String>,
}

/// The fields of the reply to change, each left out when `None`. A field
/// given `Some(None)` is cleared. A save sends only what the person
/// changed, so a part the app does not show, such as an HTML body, stays
/// as the server holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VacationPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_date: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_date: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_body: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html_body: Option<Option<String>>,
}

impl VacationPatch {
    /// Nothing to change.
    pub fn is_empty(&self) -> bool {
        *self == VacationPatch::default()
    }

    /// `reply` with this patch applied.
    pub fn applied_to(&self, reply: &VacationResponse) -> VacationResponse {
        VacationResponse {
            is_enabled: self.is_enabled.unwrap_or(reply.is_enabled),
            from_date: self.from_date.clone().unwrap_or_else(|| reply.from_date.clone()),
            to_date: self.to_date.clone().unwrap_or_else(|| reply.to_date.clone()),
            subject: self.subject.clone().unwrap_or_else(|| reply.subject.clone()),
            text_body: self.text_body.clone().unwrap_or_else(|| reply.text_body.clone()),
            html_body: self.html_body.clone().unwrap_or_else(|| reply.html_body.clone()),
        }
    }
}

impl JmapClient {
    fn vacation_account(session: &crate::Session) -> Result<String, JmapError> {
        session.account_for(VACATION).map(str::to_string).ok_or_else(|| JmapError::UnknownCapability(VACATION.into()))
    }

    pub async fn vacation(&self) -> Result<VacationResponse, JmapError> {
        let account = Self::vacation_account(&*self.session().await?)?;
        let mut request = Request::new();
        let id = request.call(VACATION, "VacationResponse/get", json!({ "accountId": account, "ids": [SINGLETON] }));
        let got: Got<VacationResponse> = self.send(&request).await?.take(&id, "VacationResponse/get")?;
        Ok(got.list.into_iter().next().unwrap_or_default())
    }

    pub async fn set_vacation(&self, reply: &VacationPatch) -> Result<(), JmapError> {
        let account = Self::vacation_account(&*self.session().await?)?;
        let mut request = Request::new();
        let id = request.call(VACATION, "VacationResponse/set", json!({ "accountId": account, "update": { SINGLETON: reply } }));
        let set: SetResponse = self.send(&request).await?.take(&id, "VacationResponse/set")?;
        match set.refusal(SINGLETON) {
            Some(refused) => Err(refused.error()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_names_only_what_it_changes() {
        let patch = VacationPatch { is_enabled: Some(true), to_date: Some(None), ..VacationPatch::default() };
        assert_eq!(serde_json::to_value(&patch).unwrap(), json!({ "isEnabled": true, "toDate": null }));
    }
}
