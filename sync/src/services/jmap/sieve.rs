//! Sieve over JMAP (RFC 9661), spoken as ManageSieve's five calls, so the
//! rules and the script's own blocks come from the same `SieveRules` a
//! ManageSieve server uses. Scripts go by name; JMAP's ids stay here.

use std::sync::Arc;

use mailrs_domain::translate::gettext;
use mailrs_jmap::JmapError;
use mailrs_sieve::client::{Capabilities, Listed, ManageSieveApi, SieveError};
use mailrs_sieve::script::Extensions;

use super::JmapApi;

pub struct JmapSieve<J> {
    api: Arc<J>,
}

impl<J> JmapSieve<J> {
    pub fn new(api: Arc<J>) -> Self {
        JmapSieve { api }
    }
}

/// The server's own words never reach the person or the log, so a refusal
/// carries fixed text.
fn sieve_error(err: JmapError) -> SieveError {
    match err {
        JmapError::Unauthorized => SieveError::Auth(String::new()),
        JmapError::Network(detail) => SieveError::Network(detail),
        JmapError::NotFound => SieveError::NotFound,
        JmapError::AnswerTooLarge { .. } => SieveError::TooLarge,
        _ => SieveError::Refused(gettext("The server did not take the rules.")),
    }
}

impl<J: JmapApi> JmapSieve<J> {
    async fn script_named(&self, name: &str) -> Result<Option<mailrs_jmap::SieveScript>, SieveError> {
        let scripts = self.api.sieve_scripts().await.map_err(sieve_error)?;
        Ok(scripts.into_iter().find(|s| s.name.as_deref() == Some(name)))
    }
}

impl<J: JmapApi> ManageSieveApi for JmapSieve<J> {
    async fn capabilities(&self) -> Result<Capabilities, SieveError> {
        let extensions = self.api.sieve_extensions().await.map_err(sieve_error)?;
        Ok(Capabilities { implementation: None, sieve: Extensions::parse(&extensions.join(" ")), sasl: Vec::new(), starttls: false })
    }

    async fn scripts(&self) -> Result<Vec<Listed>, SieveError> {
        let scripts = self.api.sieve_scripts().await.map_err(sieve_error)?;
        Ok(scripts.into_iter().map(|s| Listed { name: s.name.unwrap_or(s.id), active: s.is_active }).collect())
    }

    async fn get(&self, name: &str) -> Result<String, SieveError> {
        let script = self.script_named(name).await?.ok_or(SieveError::NotFound)?;
        self.api.sieve_text(&script.blob_id).await.map_err(sieve_error)
    }

    async fn put(&self, name: &str, script: &str) -> Result<(), SieveError> {
        let existing = self.script_named(name).await?.map(|s| s.id);
        self.api.put_sieve(name, existing.as_deref(), script).await.map(drop).map_err(sieve_error)
    }

    async fn activate(&self, name: &str) -> Result<(), SieveError> {
        let script = self.script_named(name).await?.ok_or(SieveError::NotFound)?;
        self.api.activate_sieve(&script.id).await.map_err(sieve_error)
    }
}
