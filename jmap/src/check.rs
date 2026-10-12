//! Whether a session URL takes these credentials, and what it offers.
//! Adding an account and signing in again both run this before anything
//! is kept.

use std::sync::Arc;

use crate::identity::identities;
use crate::session::{CALENDARS, CONTACTS, MAIL, MASKED_EMAIL, SIEVE, SUBMISSION, VACATION};
use crate::{JmapClient, JmapError, Session};

/// The services a session offers beside mail, read only from its
/// capabilities, never from who runs the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offered {
    pub submission: bool,
    pub vacation: bool,
    pub contacts: bool,
    pub sieve: bool,
    pub masked_email: bool,
    pub calendars: bool,
}

impl Offered {
    pub fn of(session: &Session) -> Offered {
        Offered {
            submission: session.offers(SUBMISSION),
            vacation: session.offers(VACATION),
            contacts: session.offers(CONTACTS),
            sieve: session.offers(SIEVE),
            masked_email: session.offers(MASKED_EMAIL),
            calendars: session.offers(CALENDARS),
        }
    }
}

/// What a sign-in check found.
#[derive(Debug, Clone)]
pub struct Checked {
    pub session: Arc<Session>,
    /// The address the account sends as, in lower case, when the server
    /// says one.
    pub address: Option<String>,
    /// Every address the account's identities name, in lower case: the
    /// addresses Identity/get says it may send as.
    pub addresses: Vec<String>,
    pub offered: Offered,
}

/// Fetches the session with the client's credentials and reads what it
/// offers. A session without mail is refused: Penguin Mail has nothing to
/// show for it.
pub async fn check(client: &JmapClient) -> Result<Checked, JmapError> {
    let session = client.refresh_session().await?;
    if !session.offers(MAIL) {
        return Err(JmapError::UnknownCapability(MAIL.to_string()));
    }
    let offered = Offered::of(&session);
    // Cyrus answers an identity whose email is empty, so the first identity
    // that holds an address counts.
    let addresses: Vec<String> = match offered.submission {
        true => identities(client)
            .await?
            .into_iter()
            .map(|i| i.email.trim().to_ascii_lowercase())
            .filter(|e| is_address(e))
            .collect(),
        false => Vec::new(),
    };
    let address = addresses
        .first()
        .cloned()
        .or_else(|| is_address(&session.username).then(|| session.username.trim().to_ascii_lowercase()));
    Ok(Checked { session, address, addresses, offered })
}

/// Whether two session URLs name the same server: one scheme, host and
/// port. A URL that does not parse names no server.
pub fn same_server(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => {
            a.scheme() == b.scheme()
                && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
                && a.port_or_known_default() == b.port_or_known_default()
        }
        _ => false,
    }
}

fn is_address(text: &str) -> bool {
    text.contains('@')
}

#[cfg(test)]
mod tests {
    use super::same_server;

    #[test]
    fn a_session_url_on_another_host_or_port_is_another_server() {
        assert!(same_server("https://api.fastmail.com/jmap/session", "https://API.fastmail.com:443/other"));
        assert!(!same_server("https://api.fastmail.com/jmap/session", "https://jmap.example.org/jmap/session"));
        assert!(!same_server("https://mail.example.org/jmap", "https://mail.example.org:8443/jmap"));
        assert!(!same_server("https://mail.example.org/jmap", "not a url"));
    }
}
