//! How full the account is, from its quotas (RFC 9425): the tightest byte
//! quota on the account that counts mail. A session without the quota
//! capability costs no call.

use mailrs_jmap::mail_storage;

use super::{Jmap, JmapApi, backend};
use crate::BackendError;
use crate::services::Storage;

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn space(&self) -> Result<Option<Storage>, BackendError> {
        let session = self.api().session().await.map_err(backend)?;
        if session.mail_quota_account().is_none() {
            return match &self.storage_fallback { Some(read) => read(session.username.clone()).await, None => Ok(None) };
        }
        let quotas = self.api().quotas().await.map_err(backend)?;
        Ok(mail_storage(&quotas).and_then(|q| Storage::new(q.used, q.hard_limit, q.warn_limit)))
    }
}
