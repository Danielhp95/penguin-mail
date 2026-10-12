//! Send Later on a JMAP server that holds mail: the message waits in
//! Drafts with `penguin-mail-held`, the server sends it at its hour, and
//! until then the app can call it back. A server holds only when its
//! `maxDelayedSend` covers the delay and its `EmailSubmission/get`
//! answers, since a hold nobody can call back would leave Cancel and Send
//! Now promising what they cannot do. Anything else answers `Unsupported`,
//! and the app's own queue sends the message.

use mailrs_domain::mailbox::keyword::DRAFT;
use mailrs_domain::translate::gettext;
use mailrs_domain::{EpochMillis, Role};
use mailrs_jmap::{Canceled, HELD, Hold, JmapError, SubmissionState, max_delayed_send};

use super::{Jmap, JmapApi, backend};
use crate::services::imap::{envelope_of, without_bcc};
use crate::services::{Held, Settled, Unheld};
use crate::{BackendError, now_millis};

/// How long a hold the server names no hour for, or one past its hour,
/// waits before the outbox asks again.
pub(super) const ASK_AGAIN: EpochMillis = 60_000;

/// How far the server's clock may run ahead of this one. A held copy
/// whose hour is closer than this may have gone already.
const CLOCK_SLACK: EpochMillis = 5 * 60_000;

/// What a hold the server forgot left behind.
enum Fate {
    Sent,
    NotSent,
    /// The copy cannot say, or is gone.
    Unknown,
}

impl From<Fate> for Unheld {
    fn from(fate: Fate) -> Self {
        match fate {
            Fate::Sent => Unheld::AlreadySent,
            Fate::NotSent => Unheld::InDrafts,
            Fate::Unknown => Unheld::Unknown,
        }
    }
}

impl From<Fate> for Settled {
    fn from(fate: Fate) -> Self {
        match fate {
            Fate::Sent => Settled::Sent,
            Fate::NotSent => Settled::Canceled,
            Fate::Unknown => Settled::Unknown,
        }
    }
}

/// The copy a submission names, or the row's when it names none.
fn copy_of<'a>(state: &'a SubmissionState, message_id: &'a str) -> &'a str {
    if state.email_id.is_empty() { message_id } else { &state.email_id }
}

/// A submission state none of RFC 8621's three: no cancel, no send.
fn unreadable() -> BackendError {
    BackendError::Refused(gettext("Penguin Mail could not read the server's answer."))
}

/// When the server sends the hold, in milliseconds. The server writes it
/// in RFC 3339 with any offset, which names one instant.
fn send_at(state: &SubmissionState) -> Option<EpochMillis> {
    chrono::DateTime::parse_from_rfc3339(state.send_at.as_deref()?).ok().map(|at| at.timestamp_millis())
}

/// The whole seconds from `now` to `at`, rounded up so the message never
/// goes before the time the person picked.
fn seconds_until(at: EpochMillis, now: EpochMillis) -> u64 {
    u64::try_from(at.saturating_sub(now)).map_or(0, |wait| wait.div_ceil(1000))
}

/// A hold the server would not take. A server that turns the hold
/// parameter down kept nothing, so the app's own queue sends the message.
/// A lost answer is no refusal: the server may hold the message, and the
/// caller must look in Drafts before it queues a second copy.
fn refused_hold(err: JmapError) -> BackendError {
    match err {
        JmapError::Method { ref kind, .. } if kind == "invalidArguments" || kind == "invalidProperties" => BackendError::Unsupported,
        JmapError::HoldUnknown => BackendError::Offline(JmapError::HoldUnknown.to_string()),
        other => backend(other),
    }
}

impl<J: JmapApi> Jmap<J> {
    /// Whether the server holds a message `seconds` from now and can call
    /// it back. The probe runs once a run.
    async fn holds(&self, seconds: u64) -> Result<bool, BackendError> {
        let session = self.api().session().await.map_err(backend)?;
        if seconds == 0 || max_delayed_send(&session) < seconds {
            return Ok(false);
        }
        if let Some(known) = self.known().holds {
            return Ok(known);
        }
        let answers = self.api().answers_submission_get().await.map_err(backend)?;
        self.known().holds = Some(answers);
        Ok(answers)
    }

    pub(super) async fn hold_message(&self, raw: &[u8], at: EpochMillis) -> Result<Held, BackendError> {
        if !self.holds(seconds_until(at, now_millis())).await? {
            return Err(BackendError::Unsupported);
        }
        self.sending().await?;
        self.listed().await?;
        let drafts = self.known().roles.get(&Role::Drafts).cloned().ok_or(BackendError::Unsupported)?;
        let (from, to) = envelope_of(raw, &self.settings().address)?;
        let identity = self.identity(&from).await?;
        let blob = self.api().upload(without_bcc(raw)).await.map_err(backend)?;
        // Counted again after the upload, which can take a while, so the
        // message goes at the hour picked and not that much later.
        let seconds = seconds_until(at, now_millis());
        if !self.holds(seconds).await? {
            return Err(BackendError::Unsupported);
        }
        let hold = Hold { blob_id: blob, identity_id: identity.id, mail_from: from, rcpt_to: to, drafts, hold_for: seconds };
        let held = self.api().hold(&hold).await.map_err(refused_hold)?;
        Ok(Held { handle: held.id, message_id: held.email_id, thread_id: held.thread_id })
    }

    pub(super) async fn release(&self, handle: &str, message_id: &str, at: EpochMillis) -> Result<Unheld, BackendError> {
        let found = self.api().submissions(&[handle.to_string()]).await.map_err(backend)?;
        let Some(state) = found.into_iter().next() else {
            return self.forgotten(message_id, at).await.map(Unheld::from);
        };
        let copy = copy_of(&state, message_id);
        match state.undo_status.as_str() {
            "final" => {
                self.file(copy).await;
                Ok(Unheld::AlreadySent)
            }
            "pending" => match self.api().cancel_submission(handle).await.map_err(backend)? {
                Canceled::Canceled => {
                    self.take_off_hold(copy).await?;
                    Ok(Unheld::InDrafts)
                }
                Canceled::TooLate => {
                    self.file(copy).await;
                    Ok(Unheld::AlreadySent)
                }
                // The server may have sent it a moment ago and forgotten it.
                Canceled::Gone => self.forgotten(copy, at).await.map(Unheld::from),
            },
            "canceled" => {
                self.take_off_hold(copy).await?;
                Ok(Unheld::InDrafts)
            }
            _ => Err(unreadable()),
        }
    }

    pub(super) async fn settle_hold(&self, handle: &str, message_id: &str, at: EpochMillis) -> Result<Settled, BackendError> {
        let found = self.api().submissions(&[handle.to_string()]).await.map_err(backend)?;
        let Some(state) = found.into_iter().next() else {
            return self.forgotten(message_id, at).await.map(Settled::from);
        };
        let copy = copy_of(&state, message_id);
        match state.undo_status.as_str() {
            // A server still holding it past its hour has not sent it yet:
            // ask again in a while, not straight away.
            "pending" => {
                let now = now_millis();
                Ok(Settled::Waiting(send_at(&state).filter(|t| *t > now).unwrap_or(now + ASK_AGAIN)))
            }
            "final" => {
                self.file(copy).await;
                Ok(Settled::Sent)
            }
            "canceled" => {
                self.take_off_hold(copy).await?;
                Ok(Settled::Canceled)
            }
            _ => Err(unreadable()),
        }
    }

    /// What became of a hold the server no longer knows, read from the
    /// copy it left. Sending leaves the copy as it was, so a copy still
    /// held proves nothing once its hour is near: only one without `$draft`,
    /// or one in Sent, went, and only one held well before its hour did not.
    async fn forgotten(&self, email_id: &str, at: EpochMillis) -> Result<Fate, BackendError> {
        let got = self.api().emails(&[email_id.to_string()], &["id", "keywords", "mailboxIds"]).await.map_err(backend)?;
        let Some(email) = got.list.into_iter().find(|e| e.id == email_id) else {
            return Ok(Fate::Unknown);
        };
        self.listed().await?;
        let sent = self.known().roles.get(&Role::Sent).cloned();
        let has = |keyword: &str| email.keywords.get(keyword).copied().unwrap_or(false);
        let in_sent = sent.is_some_and(|sent| email.mailbox_ids.get(&sent).copied().unwrap_or(false));
        if !has(DRAFT) || in_sent {
            if !in_sent || has(DRAFT) || has(HELD) {
                self.file(email_id).await;
            }
            return Ok(Fate::Sent);
        }
        if !has(HELD) {
            // Taken off hold, which only a cancel the server took does.
            return Ok(Fate::NotSent);
        }
        if now_millis() < at.saturating_sub(CLOCK_SLACK) {
            self.take_off_hold(email_id).await?;
            return Ok(Fate::NotSent);
        }
        Ok(Fate::Unknown)
    }

    /// Takes the held keyword off the copy. A copy someone deleted has
    /// nothing left to take off.
    async fn take_off_hold(&self, email_id: &str) -> Result<(), BackendError> {
        match self.api().unhold(email_id).await {
            Ok(()) | Err(JmapError::NotFound) => Ok(()),
            Err(err) => Err(backend(err)),
        }
    }

    /// Files a hold the server sent where sent mail goes, as `send_raw`
    /// does: Sent, else Archive. With neither it stays in Drafts without
    /// `$draft`, so it never reads as a draft to send again. The message
    /// has gone whatever happens here, so a failure goes to the log.
    async fn file(&self, email_id: &str) {
        if let Err(err) = self.listed().await {
            return tracing::warn!(%err, "a held message went out, but the mailboxes would not list");
        }
        let (drafts, filed) = {
            let known = self.known();
            let role = |r| known.roles.get(&r).cloned();
            (role(Role::Drafts), role(Role::Sent).or_else(|| role(Role::Archive)))
        };
        let Some(drafts) = drafts else {
            return tracing::warn!("a held message went out, but the account has no Drafts mailbox to file it from");
        };
        let filed = filed.unwrap_or_else(|| drafts.clone());
        if let Err(err) = self.api().file_held(email_id, &drafts, &filed).await {
            tracing::warn!(err = %backend(err), "a held message went out but stayed in Drafts");
        }
    }
}

#[cfg(test)]
mod tests {
    use mailrs_jmap::SubmissionState;

    use super::{seconds_until, send_at};

    fn sending_at(at: &str) -> SubmissionState {
        SubmissionState { id: "S1".into(), email_id: "M1".into(), undo_status: "pending".into(), send_at: Some(at.into()) }
    }

    #[test]
    fn a_send_time_with_an_offset_names_the_same_instant() {
        let utc = send_at(&sending_at("2026-10-12T08:00:00Z")).unwrap();
        assert_eq!(send_at(&sending_at("2026-10-12T10:00:00+02:00")), Some(utc));
        assert_eq!(send_at(&sending_at("2026-10-12T03:00:00-05:00")), Some(utc));
    }

    #[test]
    fn a_wait_rounds_up_to_the_next_second() {
        assert_eq!(seconds_until(10_001, 0), 11);
        assert_eq!(seconds_until(10_000, 0), 10);
        assert_eq!(seconds_until(5, 10), 0, "a time already past");
    }
}
