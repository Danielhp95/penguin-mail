//! The automatic reply over JMAP's `VacationResponse`: one object with an
//! on switch, an optional window, a subject (none answers with the
//! original's) and a body.

use chrono::{DateTime, SecondsFormat, Utc};
use mailrs_domain::{EpochMillis, Vacation};
use mailrs_jmap::VacationResponse;

use super::{Jmap, JmapApi, backend};
use crate::BackendError;
use crate::services::AutoReplyService;

fn millis(at: Option<&str>) -> Option<EpochMillis> {
    at.and_then(|at| DateTime::parse_from_rfc3339(at).ok()).map(|at| at.timestamp_millis())
}

fn date(at: Option<EpochMillis>) -> Option<String> {
    at.and_then(DateTime::<Utc>::from_timestamp_millis).map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
}

impl<J: JmapApi> AutoReplyService for Jmap<J> {
    async fn vacation(&self) -> Result<Vacation, BackendError> {
        let reply = self.api().vacation().await.map_err(backend)?;
        Ok(Vacation {
            enabled: reply.is_enabled,
            subject: reply.subject.unwrap_or_default(),
            body: reply.text_body.unwrap_or_default(),
            contacts_only: false,
            domain_only: false,
            start: millis(reply.from_date.as_deref()),
            end: millis(reply.to_date.as_deref()),
        })
    }

    async fn set_vacation(&self, vacation: &Vacation) -> Result<(), BackendError> {
        let reply = VacationResponse {
            is_enabled: vacation.enabled,
            from_date: date(vacation.start),
            to_date: date(vacation.end),
            subject: Some(vacation.subject.trim().to_string()).filter(|s| !s.is_empty()),
            text_body: Some(vacation.body.clone()),
            html_body: None,
        };
        self.api().set_vacation(&reply).await.map_err(backend)
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
