//! The mail an answer travels in.
//!
//! RFC 6047 says how an iTIP object rides in a message: the calendar part
//! carries the method it holds as a `Content-Type` parameter as well as a
//! property, so a mailer can tell a reply from an invitation without
//! reading the object. A line of prose goes beside it in
//! `multipart/alternative`, for the organizer whose mail client does not
//! read calendar parts at all.
//!
//! The part is base64, because the object means its own line endings:
//! quoted-printable would leave a mail server free to rewrite them, and an
//! object whose lines end in a bare newline is one Exchange turns down.

use chrono::{DateTime, Local, Utc};
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as MimeAddress;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::MimePart;
use mailrs_domain::calendar as model;
use mailrs_domain::invitation::{self, Answer, Invitation, Method, Occurrence, Scope, When};
use mailrs_domain::{Address, EpochMillis};

use crate::{BackendError, MailBackend};

/// The RFC 822 bytes of one iTIP message: `object` to `to`, from `me`.
/// `method` is the iTIP method the object holds, which the calendar part
/// names again in its own `Content-Type`.
pub(crate) fn itip(
    me: &Address,
    to: &Address,
    subject: &str,
    prose: &str,
    method: &str,
    object: &str,
    now: EpochMillis,
) -> Result<Vec<u8>, String> {
    let calendar = MimePart::new(
        ContentType::new("text/calendar")
            .attribute("method", method)
            .attribute("charset", "utf-8"),
        object.to_string(),
    )
    .transfer_encoding("base64");
    MessageBuilder::new()
        .from(address(me))
        .to(address(to))
        .subject(subject)
        .date(now / 1_000)
        .message_id(message_id(&me.email))
        .body(MimePart::new(
            "multipart/alternative",
            vec![MimePart::new("text/plain", prose.to_string()), calendar],
        ))
        .write_to_vec()
        .map_err(|err| err.to_string())
}

/// "Accepted: Q4 roadmap review", the subject line every mailer since
/// Outlook 97 has put an answer under.
pub(crate) fn reply_subject(answer: Answer, summary: &str) -> String {
    let said = match answer {
        Answer::Yes => "Accepted",
        Answer::No => "Declined",
        Answer::Maybe => "Tentative",
    };
    format!("{said}: {}", titled(summary))
}

/// What the organizer reads when their mail client shows no calendar part.
pub(crate) fn reply_prose(me: &Address, answer: Answer, summary: &str) -> String {
    let said = match answer {
        Answer::Yes => "accepted",
        Answer::No => "declined",
        Answer::Maybe => "tentatively accepted",
    };
    format!(
        "{} has {said} the invitation to {}.",
        me.display(),
        titled(summary)
    )
}

/// Mails the organizer of `event` the iTIP REPLY for `answer`, for a
/// calendar server that does not schedule. An event with no organizer, or
/// one `me` organizes, has nobody to tell.
pub(crate) async fn tell_organizer<M: MailBackend>(
    mail: &M,
    event: &model::Event,
    me: &str,
    answer: Answer,
    occurrence: Option<EpochMillis>,
    note: Option<&str>,
    now: EpochMillis,
) -> Result<(), BackendError> {
    let Some(organizer) = event.organizer.as_deref().map(str::trim).filter(|o| !o.is_empty() && !o.eq_ignore_ascii_case(me)) else {
        return Ok(());
    };
    let when = match event.all_day {
        true => {
            let day = |at: EpochMillis| DateTime::<Utc>::from_timestamp_millis(at).map(|d| d.date_naive());
            match (day(event.start), day(event.end - 1)) {
                (Some(first), Some(last)) => Some(When::Days { first, last: last.max(first) }),
                _ => None,
            }
        }
        false => Some(When::At { starts_at: event.start, ends_at: Some(event.end) }),
    };
    let occurrence = occurrence.map(|at| Occurrence {
        written: format!(":{}", DateTime::<Utc>::from_timestamp_millis(at).unwrap_or_default().format("%Y%m%dT%H%M%SZ")),
        at: Some(at),
    });
    let scope = match occurrence {
        Some(_) => Scope::Occurrence,
        None => Scope::Series,
    };
    let organizer = Address { name: None, email: organizer.to_string() };
    let invitation = Invitation {
        uid: event.uid.clone(),
        sequence: event.sequence,
        method: Method::Reply,
        summary: event.title.clone(),
        when,
        location: None,
        description: None,
        organizer: Some(organizer.clone()),
        guests: Vec::new(),
        repeats: None,
        occurrence,
        zone: None,
        rules: Vec::new(),
    };
    let me = Address { name: None, email: me.to_string() };
    let raw = itip(
        &me,
        &organizer,
        &reply_subject(answer, &invitation.summary),
        &reply_prose(&me, answer, &invitation.summary),
        "REPLY",
        &invitation::reply_with_note(&invitation, &me, answer, scope, note, now),
        now,
    )
    .map_err(BackendError::Refused)?;
    mail.send(&raw, None).await?;
    Ok(())
}

/// "New Time Proposed: Q4 roadmap review".
pub(super) fn counter_subject(summary: &str) -> String {
    format!("New Time Proposed: {}", titled(summary))
}

/// What the organizer reads when their mail client shows no calendar
/// part. The time carries the offset it is named in, since the organizer
/// may be reading it three zones away.
pub(super) fn counter_prose(me: &Address, summary: &str, when: &When) -> String {
    format!(
        "{} proposed a new time for {}: {}.",
        me.display(),
        titled(summary),
        spoken(when)
    )
}

/// A time in words, for the prose beside the calendar part.
fn spoken(when: &When) -> String {
    match when {
        When::At { starts_at, ends_at } => {
            let Some(start) = local(*starts_at) else {
                return "a time this computer could not read".to_string();
            };
            match ends_at.and_then(local) {
                Some(end) => format!(
                    "{} to {} ({})",
                    start.format("%A %-d %B %Y, %H:%M"),
                    end.format("%H:%M"),
                    start.format("UTC%:z")
                ),
                None => format!(
                    "{} ({})",
                    start.format("%A %-d %B %Y, %H:%M"),
                    start.format("UTC%:z")
                ),
            }
        }
        When::Days { first, last } if first == last => {
            format!("{}, all day", first.format("%A %-d %B %Y"))
        }
        When::Days { first, last } => format!(
            "{} to {}, all day",
            first.format("%-d %B"),
            last.format("%-d %B %Y")
        ),
    }
}

fn local(at: EpochMillis) -> Option<DateTime<Local>> {
    DateTime::from_timestamp_millis(at).map(|at| at.with_timezone(&Local))
}

/// The title as a sentence names it, for an event the organizer left
/// untitled as well as for one they named.
pub(super) fn titled(summary: &str) -> &str {
    match summary.trim().is_empty() {
        true => "this meeting",
        false => summary.trim(),
    }
}

fn address(who: &Address) -> MimeAddress<'static> {
    match who.name.as_deref().filter(|name| !name.trim().is_empty()) {
        Some(name) => MimeAddress::from((name.to_string(), who.email.clone())),
        None => MimeAddress::from(who.email.clone()),
    }
}

/// A `Message-ID` in the sender's own domain, as the composer makes one.
fn message_id(from_email: &str) -> String {
    let domain = from_email
        .rsplit_once('@')
        .map_or("penguin-mail.local", |(_, domain)| domain);
    format!("{}.itip@{domain}", mailrs_gmail::random_token(12))
}
