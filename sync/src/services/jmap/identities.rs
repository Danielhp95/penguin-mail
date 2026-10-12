//! The addresses a JMAP account sends as, from `Identity/get`: its own,
//! its aliases and its domains' addresses, each with its name and
//! signature. Read only: Fastmail does not document `Identity/set`.

use mailrs_jmap::SUBMISSION;

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

impl<J: JmapApi> IdentityService for Jmap<J> {
    async fn identities(&self) -> Result<Vec<SendAsAddress>, BackendError> {
        let session = self.api().session().await.map_err(backend)?;
        if !session.offers(SUBMISSION) {
            return Ok(vec![SendAsAddress { email: self.settings().address.clone(), name: None, signature: String::new(), default: true }]);
        }
        let listed = self.api().identities().await.map_err(backend)?;
        self.known().identities = listed.clone();
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
