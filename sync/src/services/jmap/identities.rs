//! The addresses a JMAP account sends as, from `Identity/get`: its own,
//! its aliases and its domains' addresses, each with its name and
//! signature. Read only: Fastmail does not document `Identity/set`.

use mailrs_jmap::{Identity, SUBMISSION, Session};

use super::{Jmap, JmapApi, backend};
use crate::BackendError;
use crate::services::{IdentityService, SendAsAddress};

/// The visible text of an HTML signature: its tags out, its entities for
/// the few a signature uses, its spaces folded.
fn text_of(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The principal's own capability in the session (JMAP Sharing), where a
/// calendars server writes the address it sends invitations from.
const PRINCIPALS: &str = "urn:ietf:params:jmap:principals";

/// The address the server itself gives the signed-in user, for an identity
/// it answers with an empty email. Cyrus does that when the user id has no
/// domain (`user1` under its default domain) and fills the email with the
/// user id when it has one. Its test image takes any sender, so a refusal
/// cannot tell what it allows; only an address the server names counts:
/// the user name when it is a whole address, or the account's own address
/// when the user name is its local part or the principal's iMIP address
/// is that address. Never an address the server has not named.
fn own_address(session: &Session, stored: &str) -> Option<String> {
    let username = session.username.trim();
    if username.contains('@') {
        return Some(username.to_ascii_lowercase());
    }
    let stored = stored.trim().to_ascii_lowercase();
    let (local, _) = stored.split_once('@')?;
    let imip = session
        .account_for(SUBMISSION)
        .and_then(|id| session.accounts.get(id))
        .and_then(|account| account.account_capabilities.get(PRINCIPALS))
        .and_then(|principal| principal[mailrs_jmap::CALENDARS]["sendTo"]["imip"].as_str())
        .and_then(|uri| uri.to_ascii_lowercase().strip_prefix("mailto:").map(str::to_string));
    (local.eq_ignore_ascii_case(username) || imip.as_deref() == Some(stored.as_str())).then_some(stored)
}

impl<J: JmapApi> Jmap<J> {
    /// The account's identities as the server lists them, with an empty
    /// email filled from `own_address`, kept for sending.
    pub(super) async fn server_identities(&self) -> Result<Vec<Identity>, BackendError> {
        let session = self.api().session().await.map_err(backend)?;
        let mut listed = self.api().identities().await.map_err(backend)?;
        let own = own_address(&session, &self.settings().address);
        for identity in listed.iter_mut().filter(|i| i.email.trim().is_empty()) {
            if let Some(own) = &own {
                identity.email.clone_from(own);
            }
        }
        self.known().identities = listed.clone();
        Ok(listed)
    }
}

impl<J: JmapApi> IdentityService for Jmap<J> {
    async fn identities(&self) -> Result<Vec<SendAsAddress>, BackendError> {
        let session = self.api().session().await.map_err(backend)?;
        if !session.offers(SUBMISSION) {
            return Ok(vec![SendAsAddress { email: self.settings().address.clone(), name: None, signature: String::new(), default: true }]);
        }
        let listed = self.server_identities().await?;
        Ok(listed
            .into_iter()
            .filter(|identity| !identity.email.starts_with("*@") && identity.email.contains('@'))
            .enumerate()
            .map(|(at, identity)| SendAsAddress {
                email: identity.email.to_ascii_lowercase(),
                name: Some(identity.name).filter(|n| !n.trim().is_empty()),
                signature: match identity.text_signature.trim().is_empty() {
                    true => text_of(&identity.html_signature),
                    false => identity.text_signature,
                },
                default: at == 0,
            })
            .collect())
    }
}
