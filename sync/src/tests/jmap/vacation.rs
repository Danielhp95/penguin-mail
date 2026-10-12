//! The automatic reply over FakeJmap.

use mailrs_domain::Vacation;
use mailrs_jmap::VACATION;

use super::over;
use crate::fake::FakeJmap;
use crate::{AutoReplyService, MailBackend};

#[tokio::test]
async fn the_reply_round_trips_with_its_dates_and_subject() {
    let h = over(FakeJmap::new().with_capability(VACATION)).await;
    let reply = h.sync.services().auto_reply.clone().expect("the session offers a reply");
    let wanted = Vacation {
        enabled: true,
        subject: "Away".into(),
        body: "Back on Monday".into(),
        contacts_only: false,
        domain_only: false,
        start: Some(1_760_000_000_000),
        end: Some(1_760_600_000_000),
    };
    reply.set_vacation(&wanted).await.unwrap();
    assert_eq!(reply.vacation().await.unwrap(), wanted);
    assert!(reply.keeps_subject() && !reply.limits_to_contacts());
}

#[tokio::test]
async fn an_empty_subject_answers_with_the_original_s() {
    let h = over(FakeJmap::new().with_capability(VACATION)).await;
    let reply = h.sync.services().auto_reply.clone().unwrap();
    reply.set_vacation(&Vacation { enabled: true, body: "Away".into(), ..Vacation::default() }).await.unwrap();
    assert_eq!(h.fake.with(|s| s.vacation.subject.clone()), None);
    let _ = h.mail().capabilities();
}

#[tokio::test]
async fn a_token_that_cannot_reach_the_reply_says_why() {
    let h = over(FakeJmap::new()).await;
    let mut services = h.sync.services().clone();
    services.mail = crate::services::AnyMail::Jmap(crate::Jmap::new(
        std::sync::Arc::new(crate::AnyJmap::from(std::sync::Arc::clone(&h.fake))),
        crate::JmapSettings {
            address: "me@fastmail.com".into(),
            provider_name: "Fastmail".into(),
            window_days: 30,
            labels: true,
            capabilities: vec![mailrs_jmap::MAIL.into()],
            auto_reply_needs_sign_in: true,
        },
    ));
    let offers = services.offers();
    assert!(!offers.auto_reply);
    assert!(offers.auto_reply_needs_sign_in);
}

/// A reply written elsewhere in HTML alone shows as its text, and turning
/// it off and on again sends only the switch, so the HTML stays.
#[tokio::test]
async fn an_html_only_reply_survives_off_and_on() {
    let h = over(FakeJmap::new().with_capability(VACATION)).await;
    let html = "<p>Away until <b>Monday</b>.</p>";
    h.fake.with(|s| {
        s.vacation = mailrs_jmap::VacationResponse {
            is_enabled: true,
            subject: Some("Away".into()),
            html_body: Some(html.into()),
            to_date: Some("2026-10-20T17:00:00Z".into()),
            ..Default::default()
        }
    });
    let reply = h.sync.services().auto_reply.clone().unwrap();
    let shown = reply.vacation().await.unwrap();
    assert_eq!(shown.body.trim(), "Away until Monday.");
    reply.set_vacation(&Vacation { enabled: false, ..shown.clone() }).await.unwrap();
    reply.set_vacation(&Vacation { enabled: true, ..shown }).await.unwrap();
    let kept = h.fake.with(|s| s.vacation.clone());
    assert!(kept.is_enabled);
    assert_eq!(kept.html_body.as_deref(), Some(html));
    assert_eq!(kept.text_body, None);
    assert_eq!(kept.to_date.as_deref(), Some("2026-10-20T17:00:00Z"));
}

/// An edited body replaces both bodies: the HTML would say something else.
#[tokio::test]
async fn an_edited_body_replaces_the_html() {
    let h = over(FakeJmap::new().with_capability(VACATION)).await;
    h.fake.with(|s| s.vacation = mailrs_jmap::VacationResponse { is_enabled: true, html_body: Some("<p>Old</p>".into()), ..Default::default() });
    let reply = h.sync.services().auto_reply.clone().unwrap();
    let shown = reply.vacation().await.unwrap();
    reply.set_vacation(&Vacation { body: "New words".into(), ..shown }).await.unwrap();
    let kept = h.fake.with(|s| s.vacation.clone());
    assert_eq!(kept.text_body.as_deref(), Some("New words"));
    assert_eq!(kept.html_body, None);
}
