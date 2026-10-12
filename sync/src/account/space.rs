//! How full the account's space on its server is.

use mailrs_domain::translate::gettext;

use super::AccountSync;
use crate::{BackendError, MailBackend, Storage, SyncError};

impl AccountSync {
    /// `None` when the server does not say. A failure carries no text of
    /// the server's, which can name an address, since the caller only
    /// logs it.
    pub async fn storage(&self) -> Result<Option<Storage>, SyncError> {
        self.services.mail.storage().await.map_err(|err| quiet(err).into())
    }
}

/// `err` without the words a server or the network put in it.
pub(crate) fn quiet(err: BackendError) -> BackendError {
    match err {
        BackendError::Offline(_) => BackendError::Offline(String::new()),
        err if err.is_transient() => BackendError::Offline(String::new()),
        BackendError::Refused(_) | BackendError::Gmail(_) | BackendError::FileMissing(_) => {
            BackendError::Refused(gettext("The server refused this request."))
        }
        other => other,
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

    #[tokio::test]
    async fn a_failed_read_carries_no_server_text() {
        let h = harness().await;
        h.fake.fail_next(mailrs_gmail::GmailError::Http { status: 400, body: "no quota for ada@example.com".into() });
        let err = h.sync.storage().await.unwrap_err();
        let (shown, logged) = (err.to_string(), format!("{err:?}"));
        assert!(!shown.contains("ada@example.com") && !logged.contains("ada@example.com"), "{logged}");
    }
}
