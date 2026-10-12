//! Send Later on a server that holds mail itself: the account hands the
//! message over, calls it back, and asks where it stands. On every other
//! server these answer `None` or `Unsupported`, and the outbox keeps the
//! message.

use mailrs_domain::EpochMillis;

use super::AccountSync;
use crate::{BackendError, Held, MailBackend, Settled, SyncError, Unheld};

impl AccountSync {
    /// Hands `raw` to the server to send at `at`. `None` when the server
    /// cannot hold it, which leaves the message to the outbox.
    pub async fn hold(&self, raw: &[u8], at: EpochMillis) -> Result<Option<Held>, SyncError> {
        match self.services.mail.schedule(raw, at).await {
            Ok(held) => {
                self.held_draft(&held).await;
                Ok(Some(held))
            }
            Err(BackendError::Unsupported) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// The server's hold on the message whose `Message-ID` header is
    /// `message_id`, for a hold whose answer never came back.
    pub async fn find_hold(&self, message_id: &str) -> Result<Option<Held>, SyncError> {
        let found = self.services.mail.find_hold(message_id).await?;
        if let Some(held) = &found {
            self.held_draft(held).await;
        }
        Ok(found)
    }

    /// Calls the hold `handle` back. `message_id` and `at` are the copy and
    /// the hour the outbox row keeps for it.
    pub async fn release_hold(&self, handle: &str, message_id: &str, at: EpochMillis) -> Result<Unheld, SyncError> {
        Ok(self.services.mail.unschedule(handle, message_id, at).await?)
    }

    /// Where the hold `handle` stands, with the row's copy and hour.
    pub async fn settle_hold(&self, handle: &str, message_id: &str, at: EpochMillis) -> Result<Settled, SyncError> {
        Ok(self.services.mail.settle(handle, message_id, at).await?)
    }

    /// The copy in Drafts is the draft the message reopens from.
    async fn held_draft(&self, held: &Held) {
        self.remember_draft(&held.message_id, &held.message_id).await;
    }
}
