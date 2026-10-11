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

    pub async fn set_vacation(&self, reply: &VacationResponse) -> Result<(), JmapError> {
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
