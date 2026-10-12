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
