//! Sieve over JMAP (RFC 9661): scripts are blobs, written by upload and
//! `SieveScript/set`, one of them active.

use serde::Deserialize;
use serde_json::json;

use crate::mail::{Got, SetResponse};
use crate::session::SIEVE;
use crate::{JmapClient, JmapError, Request};

/// The most a script may take when read back.
pub const MOST_SIEVE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SieveScript {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub blob_id: String,
    #[serde(default)]
    pub is_active: bool,
}

impl JmapClient {
    async fn sieve_account(&self) -> Result<String, JmapError> {
        let session = self.session().await?;
        session.account_for(SIEVE).map(str::to_string).ok_or_else(|| JmapError::UnknownCapability(SIEVE.into()))
    }

    /// The Sieve extensions the server runs (the account capability's
    /// `sieveExtensions`).
    pub async fn sieve_extensions(&self) -> Result<Vec<String>, JmapError> {
        let session = self.session().await?;
        let listed = session.account_capability(SIEVE).and_then(|c| c.get("sieveExtensions")).and_then(|e| e.as_array());
        Ok(listed.into_iter().flatten().filter_map(|e| e.as_str().map(str::to_string)).collect())
    }

    pub async fn sieve_scripts(&self) -> Result<Vec<SieveScript>, JmapError> {
        let account = self.sieve_account().await?;
        let mut request = Request::new();
        let id = request.call(SIEVE, "SieveScript/get", json!({ "accountId": account, "ids": null }));
        Ok(self.send(&request).await?.take::<Got<SieveScript>>(&id, "SieveScript/get")?.list)
    }

    pub async fn sieve_text(&self, blob_id: &str) -> Result<String, JmapError> {
        let account = self.sieve_account().await?;
        let bytes = self.download(&account, blob_id, "script.sieve", "application/sieve", MOST_SIEVE_BYTES).await?;
        String::from_utf8(bytes).map_err(|_| JmapError::Decode("the script is not UTF-8".into()))
    }

    /// Writes `text` as the script `name`, over `existing` when it has an
    /// id, and answers the script's id. The active script stays as it was.
    pub async fn put_sieve(&self, name: &str, existing: Option<&str>, text: &str) -> Result<String, JmapError> {
        self.write_sieve(name, existing, text, false).await
    }

    /// Like `put_sieve`, and makes the script the active one in the same
    /// call, which servers honour more reliably than a call of its own.
    pub async fn put_sieve_active(&self, name: &str, existing: Option<&str>, text: &str) -> Result<String, JmapError> {
        self.write_sieve(name, existing, text, true).await
    }

    async fn write_sieve(&self, name: &str, existing: Option<&str>, text: &str, activate: bool) -> Result<String, JmapError> {
        let account = self.sieve_account().await?;
        let blob = self.upload(&account, "application/sieve", text.as_bytes().to_vec()).await?.blob_id;
        let (mut args, key) = match existing {
            Some(id) => (json!({ "accountId": account, "update": { id: { "blobId": blob } } }), id.to_string()),
            None => (json!({ "accountId": account, "create": { "s": { "name": name, "blobId": blob } } }), "s".to_string()),
        };
        if activate {
            args["onSuccessActivateScript"] = json!(if existing.is_some() { key.clone() } else { "#s".to_string() });
        }
        let mut request = Request::new();
        let call = request.call(SIEVE, "SieveScript/set", args);
        let mut answer = self.send(&request).await?;
        let set: SetResponse = answer.take(&call, "SieveScript/set")?;
        if let Some(refused) = set.not_created.as_ref().and_then(|n| n.get(&key)).or_else(|| set.refusal(&key)) {
            return Err(refused.error());
        }
        let id = match existing {
            Some(id) if set.updated.as_ref().is_some_and(|u| u.contains_key(id)) => id.to_string(),
            Some(_) => return Err(JmapError::Decode("the script was not updated".into())),
            None => set
                .created
                .and_then(|c| c.get("s").and_then(|s| s["id"].as_str()).map(str::to_string))
                .ok_or_else(|| JmapError::Decode("the script came back without an id".into()))?,
        };
        if activate {
            Self::activated(&mut answer, &call, &id)?;
        }
        Ok(id)
    }

    /// Makes script `id` the active one.
    pub async fn activate_sieve(&self, id: &str) -> Result<(), JmapError> {
        let account = self.sieve_account().await?;
        let mut request = Request::new();
        let call = request.call(SIEVE, "SieveScript/set", json!({ "accountId": account, "onSuccessActivateScript": id }));
        let mut answer = self.send(&request).await?;
        let _: SetResponse = answer.take(&call, "SieveScript/set")?;
        Self::activated(&mut answer, &call, id)
    }

    /// The implicit `SieveScript/set` answer that follows
    /// `onSuccessActivateScript`, under the same call id: it must say the
    /// script was updated.
    fn activated(answer: &mut crate::Response, call: &crate::CallId, id: &str) -> Result<(), JmapError> {
        let set: SetResponse = answer.take(call, "SieveScript/set")?;
        if let Some(refused) = set.refusal(id) {
            return Err(refused.error());
        }
        match set.updated.as_ref().is_some_and(|u| u.contains_key(id)) {
            true => Ok(()),
            false => Err(JmapError::Decode("the script was not activated".into())),
        }
    }
}
