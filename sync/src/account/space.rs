//! How full the account's space on its server is.

use super::AccountSync;
use crate::{MailBackend, Storage, SyncError};

impl AccountSync {
    /// `None` when the server does not say.
    pub async fn storage(&self) -> Result<Option<Storage>, SyncError> {
        Ok(self.services.mail.storage().await?)
    }
}

#[cfg(test)]
mod tests {
    use mailrs_gmail::DriveStorage;

    use crate::Storage;
    use crate::tests::harness;

    #[tokio::test]
    async fn the_account_says_how_full_its_server_is() {
        let h = harness().await;
        h.fake.with(|s| s.drive_storage = Some(DriveStorage { usage: 30, limit: Some(100) }));
        assert_eq!(h.sync.storage().await.unwrap(), Storage::new(30, 100, None));
    }
}
