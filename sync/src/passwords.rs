//! Where an IMAP or JMAP account's password or token and a Microsoft
//! account's refresh token live: the desktop keyring, under one service name for each kind,
//! with the account's id as the user. An outgoing server with a password
//! of its own keeps it beside the incoming one, under the id and `/smtp`.
//! The store keeps the servers and never the secret.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use mailrs_domain::{Account, AccountId, Provider};
use mailrs_gmail::{KeyringTokenStore, MemoryTokenStore, TokenStore};
use mailrs_domain::translate::{fill, gettext};

/// The keyring would not read, keep or delete a password.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PasswordError {
    #[error("{}", refused(.0))]
    Keyring(String),
}

fn refused(reason: &str) -> String {
    fill(&gettext("The keyring refused: {reason}"), &[("reason", reason)])
}

/// Passwords by account. Implementations may block, so async callers run
/// them in `spawn_blocking`.
pub trait PasswordStore: Send + Sync {
    fn load(&self, account_id: AccountId) -> Result<Option<String>, PasswordError>;
    fn save(&self, account_id: AccountId, password: &str) -> Result<(), PasswordError>;
    /// Succeeds when there is nothing to delete.
    fn delete(&self, account_id: AccountId) -> Result<(), PasswordError>;
    /// The outgoing server's own password, for an account whose SMTP
    /// server takes a different one; `None` means the incoming password.
    fn load_outgoing(&self, account_id: AccountId) -> Result<Option<String>, PasswordError>;
    fn save_outgoing(&self, account_id: AccountId, password: &str) -> Result<(), PasswordError>;
    /// Succeeds when there is nothing to delete.
    fn delete_outgoing(&self, account_id: AccountId) -> Result<(), PasswordError>;
}

/// Passwords under one service name: the desktop keyring through Secret
/// Service outside a Flatpak, and, in a Flatpak build, the Secret
/// portal's own encrypted store instead, which no other app can read.
pub struct KeyringPasswords {
    service: String,
}

impl KeyringPasswords {
    /// One service for every IMAP account, apart from Google's refresh
    /// tokens under `mailrs`, so removing one kind never touches the other.
    pub const SERVICE: &'static str = "penguin-mail-imap";

    pub fn new() -> Self {
        KeyringPasswords {
            service: Self::SERVICE.to_string(),
        }
    }

    /// Microsoft's refresh tokens, apart from IMAP passwords and Google's
    /// tokens, keyed by account id like the passwords. Both the desktop
    /// keyring and the Flatpak's Secret portal file key by service name,
    /// so this works on either.
    pub const MICROSOFT_SERVICE: &'static str = "penguin-mail-microsoft";

    pub fn microsoft() -> Self {
        KeyringPasswords {
            service: Self::MICROSOFT_SERVICE.to_string(),
        }
    }

    /// The own Google client an account signed in with, keyed by account
    /// id. Apart from Google's refresh tokens, which are keyed by address,
    /// so an entry here can never be read as a token.
    pub const GOOGLE_CLIENT_SERVICE: &'static str = "penguin-mail-google-client";

    pub fn google_client() -> Self {
        KeyringPasswords {
            service: Self::GOOGLE_CLIENT_SERVICE.to_string(),
        }
    }

    /// JMAP passwords and tokens, apart from IMAP passwords, so a JMAP
    /// account keeps its CalDAV app password under `SERVICE` beside them.
    pub const JMAP_SERVICE: &'static str = "penguin-mail-jmap";

    pub fn jmap() -> Self {
        KeyringPasswords {
            service: Self::JMAP_SERVICE.to_string(),
        }
    }

    #[cfg(not(feature = "packaging-flatpak"))]
    fn entry(&self, user: &str) -> Result<keyring::Entry, PasswordError> {
        keyring::Entry::new(&self.service, user).map_err(keyring_error)
    }
}

impl Default for KeyringPasswords {
    fn default() -> Self {
        Self::new()
    }
}

/// The keyring's user name for an account: its id. Each account row owns
/// one entry, and the entry stays with the row if its address changes.
fn user(account_id: AccountId) -> String {
    account_id.to_string()
}

/// The keyring's user name for an account's outgoing password, beside the
/// incoming one under the same service.
fn outgoing_user(account_id: AccountId) -> String {
    format!("{account_id}/smtp")
}

#[cfg(not(feature = "packaging-flatpak"))]
impl KeyringPasswords {
    fn load_as(&self, user: &str) -> Result<Option<String>, PasswordError> {
        match self.entry(user)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(keyring_error(err)),
        }
    }

    fn save_as(&self, user: &str, password: &str) -> Result<(), PasswordError> {
        self.entry(user)?
            .set_password(password)
            .map_err(keyring_error)
    }

    fn delete_as(&self, user: &str) -> Result<(), PasswordError> {
        match self.entry(user)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(keyring_error(err)),
        }
    }
}

#[cfg(feature = "packaging-flatpak")]
impl KeyringPasswords {
    fn load_as(&self, user: &str) -> Result<Option<String>, PasswordError> {
        mailrs_gmail::secret_portal::load(&self.service, user).map_err(PasswordError::Keyring)
    }

    fn save_as(&self, user: &str, password: &str) -> Result<(), PasswordError> {
        mailrs_gmail::secret_portal::save(&self.service, user, password)
            .map_err(PasswordError::Keyring)
    }

    fn delete_as(&self, user: &str) -> Result<(), PasswordError> {
        mailrs_gmail::secret_portal::delete(&self.service, user).map_err(PasswordError::Keyring)
    }
}

impl PasswordStore for KeyringPasswords {
    fn load(&self, account_id: AccountId) -> Result<Option<String>, PasswordError> {
        self.load_as(&user(account_id))
    }

    fn save(&self, account_id: AccountId, password: &str) -> Result<(), PasswordError> {
        self.save_as(&user(account_id), password)
    }

    fn delete(&self, account_id: AccountId) -> Result<(), PasswordError> {
        self.delete_as(&user(account_id))
    }

    fn load_outgoing(&self, account_id: AccountId) -> Result<Option<String>, PasswordError> {
        self.load_as(&outgoing_user(account_id))
    }

    fn save_outgoing(&self, account_id: AccountId, password: &str) -> Result<(), PasswordError> {
        self.save_as(&outgoing_user(account_id), password)
    }

    fn delete_outgoing(&self, account_id: AccountId) -> Result<(), PasswordError> {
        self.delete_as(&outgoing_user(account_id))
    }
}

#[cfg(not(feature = "packaging-flatpak"))]
fn keyring_error(err: keyring::Error) -> PasswordError {
    PasswordError::Keyring(err.to_string())
}

/// Passwords in memory, for tests and the demo, which must never reach
/// the person's keyring.
/// Keyed by the user name the keyring would use, so the incoming and
/// outgoing passwords sit apart as they do there.
#[derive(Default)]
pub struct MemoryPasswords {
    passwords: Mutex<HashMap<String, String>>,
}

impl MemoryPasswords {
    fn load_as(&self, user: String) -> Result<Option<String>, PasswordError> {
        let passwords = self.passwords.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(passwords.get(&user).cloned())
    }

    fn save_as(&self, user: String, password: &str) -> Result<(), PasswordError> {
        let mut passwords = self.passwords.lock().unwrap_or_else(PoisonError::into_inner);
        passwords.insert(user, password.to_string());
        Ok(())
    }

    fn delete_as(&self, user: String) -> Result<(), PasswordError> {
        let mut passwords = self.passwords.lock().unwrap_or_else(PoisonError::into_inner);
        passwords.remove(&user);
        Ok(())
    }
}

impl PasswordStore for MemoryPasswords {
    fn load(&self, account_id: AccountId) -> Result<Option<String>, PasswordError> {
        self.load_as(user(account_id))
    }

    fn save(&self, account_id: AccountId, password: &str) -> Result<(), PasswordError> {
        self.save_as(user(account_id), password)
    }

    fn delete(&self, account_id: AccountId) -> Result<(), PasswordError> {
        self.delete_as(user(account_id))
    }

    fn load_outgoing(&self, account_id: AccountId) -> Result<Option<String>, PasswordError> {
        self.load_as(outgoing_user(account_id))
    }

    fn save_outgoing(&self, account_id: AccountId, password: &str) -> Result<(), PasswordError> {
        self.save_as(outgoing_user(account_id), password)
    }

    fn delete_outgoing(&self, account_id: AccountId) -> Result<(), PasswordError> {
        self.delete_as(outgoing_user(account_id))
    }
}

/// The app's password store: the keyring, or memory in the demo. One
/// type, so the core holds one field whichever it runs with.
pub enum Passwords {
    Keyring(KeyringPasswords),
    Memory(MemoryPasswords),
}

impl PasswordStore for Passwords {
    fn load(&self, account_id: AccountId) -> Result<Option<String>, PasswordError> {
        match self {
            Passwords::Keyring(store) => store.load(account_id),
            Passwords::Memory(store) => store.load(account_id),
        }
    }

    fn save(&self, account_id: AccountId, password: &str) -> Result<(), PasswordError> {
        match self {
            Passwords::Keyring(store) => store.save(account_id, password),
            Passwords::Memory(store) => store.save(account_id, password),
        }
    }

    fn delete(&self, account_id: AccountId) -> Result<(), PasswordError> {
        match self {
            Passwords::Keyring(store) => store.delete(account_id),
            Passwords::Memory(store) => store.delete(account_id),
        }
    }

    fn load_outgoing(&self, account_id: AccountId) -> Result<Option<String>, PasswordError> {
        match self {
            Passwords::Keyring(store) => store.load_outgoing(account_id),
            Passwords::Memory(store) => store.load_outgoing(account_id),
        }
    }

    fn save_outgoing(&self, account_id: AccountId, password: &str) -> Result<(), PasswordError> {
        match self {
            Passwords::Keyring(store) => store.save_outgoing(account_id, password),
            Passwords::Memory(store) => store.save_outgoing(account_id, password),
        }
    }

    fn delete_outgoing(&self, account_id: AccountId) -> Result<(), PasswordError> {
        match self {
            Passwords::Keyring(store) => store.delete_outgoing(account_id),
            Passwords::Memory(store) => store.delete_outgoing(account_id),
        }
    }
}

/// Where each kind of account keeps what signs it in: a Google account's
/// refresh token under its address, and the own client it signed in with
/// under its id when it has one; an IMAP or POP3 account's password, a
/// JMAP account's password or token and a Microsoft account's refresh
/// token under its id. Clones share the stores.
#[derive(Clone)]
pub struct Secrets {
    pub google: Arc<dyn TokenStore>,
    pub google_clients: Arc<Passwords>,
    pub passwords: Arc<Passwords>,
    pub microsoft: Arc<Passwords>,
    pub jmap: Arc<Passwords>,
}

impl Secrets {
    /// The desktop keyring, under the service names every earlier version
    /// used, so existing accounts find their secrets.
    pub fn keyring() -> Self {
        Secrets {
            google: Arc::new(KeyringTokenStore::new()),
            google_clients: Arc::new(Passwords::Keyring(KeyringPasswords::google_client())),
            passwords: Arc::new(Passwords::Keyring(KeyringPasswords::new())),
            microsoft: Arc::new(Passwords::Keyring(KeyringPasswords::microsoft())),
            jmap: Arc::new(Passwords::Keyring(KeyringPasswords::jmap())),
        }
    }

    /// Memory, for the demo and tests, which must never reach the
    /// person's keyring.
    pub fn memory() -> Self {
        Secrets {
            google: Arc::new(MemoryTokenStore::default()),
            google_clients: Arc::new(Passwords::Memory(MemoryPasswords::default())),
            passwords: Arc::new(Passwords::Memory(MemoryPasswords::default())),
            microsoft: Arc::new(Passwords::Memory(MemoryPasswords::default())),
            jmap: Arc::new(Passwords::Memory(MemoryPasswords::default())),
        }
    }

    /// Deletes what signs `account` in from the store its provider keeps
    /// it in, off the runtime: a Google account's own client goes with its
    /// token. Succeeds when nothing was kept.
    pub async fn forget(&self, account: &Account) -> Result<(), PasswordError> {
        let id = account.id;
        let delete: Box<dyn FnOnce() -> Result<(), PasswordError> + Send> = match account.provider {
            Provider::Gmail => {
                let (tokens, email) = (Arc::clone(&self.google), account.email.clone());
                let clients = Arc::clone(&self.google_clients);
                Box::new(move || {
                    tokens.delete(&email).map_err(|err| match err {
                        mailrs_gmail::GmailError::Keyring(reason) => PasswordError::Keyring(reason),
                        err => PasswordError::Keyring(err.to_string()),
                    })?;
                    clients.delete(id)
                })
            }
            Provider::Imap | Provider::Pop3 => {
                let passwords = Arc::clone(&self.passwords);
                Box::new(move || first_error([passwords.delete_outgoing(id), passwords.delete(id)]))
            }
            Provider::Microsoft => {
                let tokens = Arc::clone(&self.microsoft);
                Box::new(move || tokens.delete(id))
            }
            Provider::Jmap => {
                let (jmap, passwords) = (Arc::clone(&self.jmap), Arc::clone(&self.passwords));
                // The token or password, and the CalDAV app password kept
                // beside it in part 3. Nothing may stay under the id: ids
                // are reused, and the next account would read it.
                Box::new(move || {
                    first_error([jmap.delete(id), passwords.delete(id), passwords.delete_outgoing(id)])
                })
            }
        };
        tokio::task::spawn_blocking(delete)
            .await
            .map_err(|err| PasswordError::Keyring(err.to_string()))?
    }
}

/// The first error among deletes that all ran, so one the keyring refuses
/// does not leave the others behind.
pub(crate) fn first_error<const N: usize>(deletes: [Result<(), PasswordError>; N]) -> Result<(), PasswordError> {
    deletes.into_iter().find_map(Result::err).map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::{KeyringPasswords, MemoryPasswords, PasswordError, PasswordStore, Secrets, first_error, outgoing_user, user};

    #[test]
    fn every_imap_password_sits_under_one_service_by_account_id() {
        assert_eq!(KeyringPasswords::SERVICE, "penguin-mail-imap");
        assert_eq!(user(7), "7");
    }

    #[test]
    fn a_password_comes_back_until_it_is_deleted() {
        let store = MemoryPasswords::default();
        assert_eq!(store.load(1).unwrap(), None);
        store.save(1, "secret").unwrap();
        assert_eq!(store.load(1).unwrap().as_deref(), Some("secret"));
        store.delete(1).unwrap();
        store.delete(1).unwrap();
        assert_eq!(store.load(1).unwrap(), None);
    }

    #[test]
    fn an_outgoing_password_is_kept_apart_from_the_incoming_one() {
        let store = MemoryPasswords::default();
        store.save(1, "incoming").unwrap();
        assert_eq!(store.load_outgoing(1).unwrap(), None);
        store.save_outgoing(1, "outgoing").unwrap();
        assert_eq!(store.load(1).unwrap().as_deref(), Some("incoming"));
        assert_eq!(store.load_outgoing(1).unwrap().as_deref(), Some("outgoing"));
        store.delete_outgoing(1).unwrap();
        assert_eq!(store.load_outgoing(1).unwrap(), None);
        assert_eq!(store.load(1).unwrap().as_deref(), Some("incoming"));
    }

    #[test]
    fn the_outgoing_password_sits_beside_the_incoming_one_under_its_own_user() {
        assert_eq!(outgoing_user(7), "7/smtp");
    }

    #[tokio::test]
    async fn forgetting_an_imap_account_deletes_both_its_passwords() {
        let secrets = Secrets::memory();
        let account = mailrs_domain::Account {
            id: 3,
            email: "dana@fastmail.com".into(),
            state: mailrs_domain::AccountState::Ok,
            provider: mailrs_domain::Provider::Imap,
            provider_name: Some("Fastmail".into()),
        };
        secrets.passwords.save(3, "incoming").unwrap();
        secrets.passwords.save_outgoing(3, "outgoing").unwrap();
        secrets.forget(&account).await.unwrap();
        assert_eq!(secrets.passwords.load(3).unwrap(), None);
        assert_eq!(secrets.passwords.load_outgoing(3).unwrap(), None);
    }

    #[test]
    fn jmap_secrets_sit_under_their_own_service() {
        assert_eq!(KeyringPasswords::JMAP_SERVICE, "penguin-mail-jmap");
    }

    #[tokio::test]
    async fn forgetting_a_jmap_account_deletes_its_secret_and_its_password() {
        let secrets = Secrets::memory();
        let account = mailrs_domain::Account {
            id: 4,
            email: "me@fastmail.com".into(),
            state: mailrs_domain::AccountState::Ok,
            provider: mailrs_domain::Provider::Jmap,
            provider_name: Some("Fastmail".into()),
        };
        secrets.jmap.save(4, "fmu1-token").unwrap();
        secrets.passwords.save(4, "caldav").unwrap();
        secrets.passwords.save_outgoing(4, "smtp").unwrap();
        secrets.forget(&account).await.unwrap();
        assert_eq!(secrets.jmap.load(4).unwrap(), None);
        assert_eq!(secrets.passwords.load(4).unwrap(), None);
        assert_eq!(secrets.passwords.load_outgoing(4).unwrap(), None);
    }

    #[test]
    fn the_first_failed_delete_is_the_answer() {
        let refused = |why: &str| Err(PasswordError::Keyring(why.into()));
        let answer = first_error([Ok(()), refused("first"), Ok(()), refused("third")]);
        assert!(matches!(answer, Err(PasswordError::Keyring(why)) if why == "first"));
        assert!(first_error([Ok(()), Ok(())]).is_ok());
    }
}
