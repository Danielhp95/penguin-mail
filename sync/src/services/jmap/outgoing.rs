//! Sending and drafts. A message goes as the identity whose address its
//! From header names, to every address in To, Cc and Bcc; the copy the
//! server keeps carries no Bcc header. One request imports the copy into
//! Drafts, submits it and moves it to Sent without `$draft`. A draft is a
//! message in Drafts with `$draft`; each save imports the new one, then
//! destroys the old, so its id changes on every save.

use mailrs_domain::Role;
use mailrs_domain::translate::{fill, gettext};
use mailrs_jmap::{EmailQuery, Identity, JmapError, SUBMISSION, Submission};
use serde_json::{Map, json};

use super::{Jmap, JmapApi, backend};
use crate::BackendError;
use crate::api::{DraftRef, SavedDraft};
use crate::services::imap::{envelope_of, without_bcc};

/// The most drafts one listing names.
const MOST_DRAFTS: usize = 500;

/// The identity that sends as `from`: its own address, then a whole-domain
/// identity (`*@domain`, as Fastmail keeps one for a domain), then the
/// first, which the server may still refuse.
pub(super) fn identity_for<'a>(identities: &'a [Identity], from: &str) -> Option<&'a Identity> {
    let domain = from.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase());
    let whole_domain = |i: &&Identity| {
        i.email.strip_prefix("*@").is_some_and(|d| domain.as_deref() == Some(d.to_ascii_lowercase().as_str()))
    };
    identities
        .iter()
        .find(|i| i.email.eq_ignore_ascii_case(from))
        .or_else(|| identities.iter().find(whole_domain))
        .or_else(|| identities.first())
}

/// Why the server refused a submission, in words a person reads.
fn refused_send(err: JmapError, from: &str) -> BackendError {
    let kind = match &err {
        JmapError::Method { kind, .. } => kind.as_str(),
        // `tooLarge` reads as this kind, and a submission request is small.
        JmapError::RequestTooLarge => "tooLarge",
        _ => return backend(err),
    };
    BackendError::Refused(match kind {
        "forbiddenFrom" | "forbiddenMailFrom" => fill(&gettext("The server does not let this account send from {address}."), &[("address", from)]),
        "forbiddenToSend" => gettext("The server does not let this account send mail now."),
        "tooManyRecipients" => gettext("The message names more recipients than the server sends to at once."),
        "noRecipients" | "invalidRecipients" => gettext("The server could not send to one of the recipients."),
        "tooLarge" => gettext("The message is larger than the server sends."),
        "invalidEmail" => gettext("The server could not read the message to send it."),
        _ => return backend(err),
    })
}

impl<J: JmapApi> Jmap<J> {
    /// Refuses in words when the sign-in carries no access to sending.
    async fn sending(&self) -> Result<(), BackendError> {
        let session = self.api().session().await.map_err(backend)?;
        match session.offers(SUBMISSION) {
            true => Ok(()),
            false => Err(BackendError::Refused(gettext(
                "This account can read mail but not send it: its sign-in has no access to sending.",
            ))),
        }
    }

    /// The identity for `from`, read again from the server when the ones
    /// kept have no exact match.
    async fn identity(&self, from: &str) -> Result<Identity, BackendError> {
        let kept = identity_for(&self.known().identities, from).filter(|i| i.email.eq_ignore_ascii_case(from)).cloned();
        if let Some(identity) = kept {
            return Ok(identity);
        }
        let fresh = self.api().identities().await.map_err(backend)?;
        let found = identity_for(&fresh, from).cloned();
        self.known().identities = fresh;
        found.ok_or_else(|| BackendError::Refused(gettext("This account has no address the server lets it send from.")))
    }

    pub(super) async fn send_raw(&self, raw: &[u8]) -> Result<String, BackendError> {
        self.sending().await?;
        self.listed().await?;
        let (from, to) = envelope_of(raw, &self.settings().address)?;
        let (drafts, filed) = {
            let known = self.known();
            let role = |r| known.roles.get(&r).cloned();
            (role(Role::Drafts), role(Role::Sent).or_else(|| role(Role::Archive)))
        };
        // The copy waits in Drafts and moves where sent mail is filed. With
        // nowhere to file it, it stays in Drafts without `$draft`, so it
        // never reads as a draft to send again.
        let (drafts, sent) = match (drafts, filed) {
            (drafts, Some(filed)) => (drafts, Some(filed)),
            (Some(drafts), None) => (None, Some(drafts)),
            (None, None) => {
                return Err(BackendError::Refused(gettext(
                    "The server has neither a Drafts nor a Sent mailbox to keep the message in.",
                )));
            }
        };
        let identity = self.identity(&from).await?;
        let blob = self.api().upload(without_bcc(raw)).await.map_err(backend)?;
        let submission = Submission { blob_id: blob, identity_id: identity.id, mail_from: from.clone(), rcpt_to: to, drafts, sent };
        let made = self.api().submit(&submission).await.map_err(|e| refused_send(e, &from))?;
        Ok(made.id)
    }

    pub(super) async fn save(&self, draft_id: Option<&str>, raw: &[u8]) -> Result<SavedDraft, BackendError> {
        self.sending().await?;
        self.listed().await?;
        let drafts = self
            .known()
            .roles
            .get(&Role::Drafts)
            .cloned()
            .ok_or_else(|| BackendError::Refused(gettext("The server has no Drafts mailbox.")))?;
        let blob = self.api().upload(raw.to_vec()).await.map_err(backend)?;
        let saved = self.api().import(&blob, &[drafts], &["$draft", "$seen"], draft_id).await.map_err(backend)?;
        // The new draft is saved either way. An old one the server would
        // not destroy gets one more try, so the person does not find two.
        if let (Some(old), Some(leftover)) = (draft_id, saved.leftover)
            && !matches!(leftover, JmapError::NotFound)
            && self.drop_draft(old).await.is_err()
        {
            // No error text: a server's description can name the message.
            tracing::warn!("an older copy of a saved draft stayed on the server");
        }
        let made = saved.email;
        Ok(SavedDraft { draft_id: made.id.clone(), message_id: made.id, thread_id: made.thread_id })
    }

    /// Sends the draft as the server holds it, through the same path as a
    /// new message so its Bcc stays in the envelope, then drops it.
    pub(super) async fn send_saved(&self, draft_id: &str) -> Result<String, BackendError> {
        self.sending().await?;
        let raw = self.raws(&[draft_id.to_string()]).await?.into_iter().next().ok_or(BackendError::NotFound)?;
        // A try whose answer was lost may have sent it already: the draft
        // stays until a good answer, so a retry would send it twice.
        let sent = match self.already_sent(&raw.bytes, draft_id).await? {
            Some(sent) => sent,
            None => self.send_raw(&raw.bytes).await?,
        };
        // A server that keeps one copy of the same bytes sent the draft
        // itself and filed it in Sent: there is nothing left to drop.
        if sent != draft_id && self.drop_draft(draft_id).await.is_err() {
            tracing::warn!("the draft stayed after its message went");
        }
        Ok(sent)
    }

    /// The sent copy of the draft `draft_id`, found by its Message-ID among
    /// the messages that are no drafts, wherever the copy was filed.
    async fn already_sent(&self, raw: &[u8], draft_id: &str) -> Result<Option<String>, BackendError> {
        let parts = mailrs_mime::parts(raw).unwrap_or_default();
        let Some(message_id) = parts.header("Message-ID").map(str::trim).filter(|m| !m.is_empty()) else {
            return Ok(None);
        };
        let header = json!({ "header": ["Message-ID", message_id] });
        let filter = json!({ "operator": "AND", "conditions": [header, { "notKeyword": "$draft" }] });
        // Two, so a draft that lost its `$draft` elsewhere is passed over.
        let query = EmailQuery { filter: Some(filter), position: 0, limit: 2, total: false };
        let found = self.api().query(&query).await.map_err(backend)?;
        Ok(found.hits.into_iter().map(|h| h.id).find(|id| id != draft_id))
    }

    pub(super) async fn drop_draft(&self, draft_id: &str) -> Result<(), BackendError> {
        let answer = self.api().set_emails(&Map::new(), &[draft_id.to_string()]).await.map_err(backend)?;
        match answer.refusal(draft_id) {
            Some(refused) if refused.kind != "notFound" => Err(backend(refused.error())),
            _ => Ok(()),
        }
    }

    pub(super) async fn drafts(&self) -> Result<Vec<DraftRef>, BackendError> {
        self.listed().await?;
        let Some(drafts) = self.known().roles.get(&Role::Drafts).cloned() else {
            return Ok(Vec::new());
        };
        let filter = json!({ "operator": "AND", "conditions": [{ "inMailbox": drafts }, { "hasKeyword": "$draft" }] });
        let query = EmailQuery { filter: Some(filter), position: 0, limit: MOST_DRAFTS, total: false };
        let found = self.api().query(&query).await.map_err(backend)?;
        Ok(found.hits.into_iter().map(|h| DraftRef { draft_id: h.id.clone(), message_id: h.id }).collect())
    }
}
