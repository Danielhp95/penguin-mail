//! The automatic reply over JMAP's `VacationResponse`: one object with an
//! on switch, an optional window, a subject (none answers with the
//! original's) and a text or HTML body. A save patches only what the
//! person changed.

use chrono::{DateTime, SecondsFormat, Utc};
use mailrs_domain::{EpochMillis, Vacation};
use mailrs_jmap::{VacationPatch, VacationResponse};

use super::{Jmap, JmapApi, backend};
use crate::BackendError;
use crate::services::AutoReplyService;

fn millis(at: Option<&str>) -> Option<EpochMillis> {
    at.and_then(|at| DateTime::parse_from_rfc3339(at).ok()).map(|at| at.timestamp_millis())
}

fn date(at: Option<EpochMillis>) -> Option<String> {
    at.and_then(DateTime::<Utc>::from_timestamp_millis).map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// The reply as the app shows it. A reply written elsewhere in HTML
/// alone shows as its text.
fn shown(reply: &VacationResponse) -> Vacation {
    let body = match (&reply.text_body, &reply.html_body) {
        (Some(text), _) => text.clone(),
        (None, Some(html)) => mailrs_mime::html::html_to_text(html),
        (None, None) => String::new(),
    };
    Vacation {
        enabled: reply.is_enabled,
        subject: reply.subject.clone().unwrap_or_default(),
        body,
        contacts_only: false,
        domain_only: false,
        start: millis(reply.from_date.as_deref()),
        end: millis(reply.to_date.as_deref()),
    }
}

/// What turns `held`, the server's reply, into `wanted`: only the fields
/// that differ from what the app showed. An edited body goes out as text
/// and clears the HTML, which would say something else.
fn patch(held: &VacationResponse, wanted: &Vacation) -> VacationPatch {
    let was = shown(held);
    let subject = wanted.subject.trim();
    VacationPatch {
        is_enabled: (wanted.enabled != was.enabled).then_some(wanted.enabled),
        from_date: (wanted.start != was.start).then(|| date(wanted.start)),
        to_date: (wanted.end != was.end).then(|| date(wanted.end)),
        subject: (subject != was.subject.trim()).then(|| Some(subject.to_string()).filter(|s| !s.is_empty())),
        text_body: (wanted.body != was.body).then(|| Some(wanted.body.clone())),
        html_body: (wanted.body != was.body && held.html_body.is_some()).then_some(None),
    }
}

impl<J: JmapApi> AutoReplyService for Jmap<J> {
    async fn vacation(&self) -> Result<Vacation, BackendError> {
        let reply = self.api().vacation().await.map_err(backend)?;
        Ok(shown(&reply))
    }

    /// Reads the reply first and sends only what the person changed, so a
    /// part the app does not show stays as the server holds it.
    async fn set_vacation(&self, vacation: &Vacation) -> Result<(), BackendError> {
        let held = self.api().vacation().await.map_err(backend)?;
        let changes = patch(&held, vacation);
        if changes.is_empty() {
            return Ok(());
        }
        self.api().set_vacation(&changes).await.map_err(backend)
    }

    fn keeps_subject(&self) -> bool {
        true
    }

    /// JMAP's reply has no such switch.
    fn limits_to_contacts(&self) -> bool {
        false
    }
}

impl<J: JmapApi> Jmap<J> {
    /// The server keeps an automatic reply this sign-in cannot reach.
    pub fn auto_reply_needs_sign_in(&self) -> bool {
        self.settings().auto_reply_needs_sign_in
    }
}
