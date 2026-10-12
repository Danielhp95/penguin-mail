//! A read receipt over `MDN/send` (RFC 9007), which also marks the message
//! `$mdnsent`. A session without it answers `Unsupported`, and the account
//! sends the receipt as ordinary mail.

use mailrs_jmap::{MDN, MdnOut};

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
        let mdn = MdnOut {
            for_email_id: receipt.message_id.clone(),
            identity_id: identity.id,
            subject: receipt.subject.clone(),
            text_body: receipt.text.clone(),
        };
        self.api().send_mdn(&mdn).await.map_err(backend)
    }
}
