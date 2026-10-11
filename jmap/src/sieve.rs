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
    /// id, and answers the script's id.
    pub async fn put_sieve(&self, name: &str, existing: Option<&str>, text: &str) -> Result<String, JmapError> {
        let account = self.sieve_account().await?;
        let blob = self.upload(&account, "application/sieve", text.as_bytes().to_vec()).await?.blob_id;
        let (args, key) = match existing {
            Some(id) => (json!({ "accountId": account, "update": { id: { "blobId": blob } } }), id.to_string()),
            None => (json!({ "accountId": account, "create": { "s": { "name": name, "blobId": blob } } }), "s".to_string()),
        };
        let mut request = Request::new();
        let call = request.call(SIEVE, "SieveScript/set", args);
        let set: SetResponse = self.send(&request).await?.take(&call, "SieveScript/set")?;
        if let Some(refused) = set.not_created.as_ref().and_then(|n| n.get(&key)).or_else(|| set.refusal(&key)) {
            return Err(refused.error());
        }
        match existing {
            Some(id) => Ok(id.to_string()),
            None => set
                .created
                .and_then(|c| c.get("s").and_then(|s| s["id"].as_str()).map(str::to_string))
                .ok_or_else(|| JmapError::Decode("the script came back without an id".into())),
        }
    }

    pub async fn activate_sieve(&self, id: &str) -> Result<(), JmapError> {
        let account = self.sieve_account().await?;
        let mut request = Request::new();
        let call = request.call(SIEVE, "SieveScript/set", json!({ "accountId": account, "onSuccessActivateScript": id }));
        let _: SetResponse = self.send(&request).await?.take(&call, "SieveScript/set")?;
        Ok(())
    }
}
