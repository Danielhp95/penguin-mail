//! A read receipt over `MDN/send` (RFC 9007), which also marks the message
//! `$mdnsent`. A session without it answers `Unsupported`, and the account
//! sends the receipt as ordinary mail.

use mailrs_jmap::error::refused_outright;
use mailrs_jmap::{JmapError, MDN, MdnOut};

use super::outgoing::sends_as;
use super::{Jmap, JmapApi, backend};
use crate::BackendError;
use crate::services::ReceiptOut;

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn mdn(&self, receipt: &ReceiptOut) -> Result<(), BackendError> {
        let session = self.api().session().await.map_err(backend)?;
        if !session.offers(MDN) {
            return Err(BackendError::Unsupported);
        }
        let identity = self.identity(&receipt.from).await?;
        // The server writes the receipt from the identity's address. One
        // for another address, which `identity` falls back to, would tell
        // the sender an address the mail never went to; the receipt goes
        // as ordinary mail from the right address instead.
        if !sends_as(&identity, &receipt.from) {
            return Err(BackendError::Unsupported);
        }
        let mdn = MdnOut {
            for_email_id: receipt.message_id.clone(),
            identity_id: identity.id,
            subject: receipt.subject.clone(),
            text_body: receipt.text.clone(),
        };
        self.api().send_mdn(&mdn).await.map_err(|err| match err {
            JmapError::Unauthorized | JmapError::RateLimited { .. } | JmapError::Network(_) => backend(err),
            err if refused_outright(&err) => backend(err),
            // `serverFail`, or an answer that would not read, may follow a
            // receipt the server sent: no refusal, so it is not sent again.
            _ => BackendError::Offline("the server may have sent the receipt; no clear answer came back".into()),
        })
    }
}
