//! Answering a read receipt: `MDN/send` where the server offers it, a
//! report sent as mail where it does not, and nothing at all for no.

use super::{Harness, over};
use crate::fake::FakeJmap;

const ASKS: &[u8] = b"From: Ines Duarte <ines@allotment.example>\r\nTo: me@example.com\r\nSubject: Rota\r\n\
Message-ID: <rota@allotment.example>\r\nDisposition-Notification-To: ines@allotment.example\r\n\r\nThe rota is up.\r\n";

/// A message that asks for a receipt, synced and opened, as the card
/// sees it.
async fn asked_by(h: &Harness, raw: &[u8]) -> String {
    let id = h.fake.deliver("inbox", raw, chrono::Utc::now().timestamp_millis());
    h.bootstrap_all().await;
    h.sync.body(&id).await.unwrap();
    id
}

async fn asked(h: &Harness) -> String {
    asked_by(h, ASKS).await
}

#[tokio::test]
async fn a_receipt_goes_through_mdn_send_where_the_server_offers_it() {
    let h = over(FakeJmap::new().with_capability(mailrs_jmap::MDN)).await;
    let id = asked(&h).await;
    h.sync.answer_receipt(&id, true).await.unwrap();
    let sent = h.fake.with(|s| s.mdns.clone());
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].subject, "Read: Rota");
    assert!(h.fake.with(|s| s.sent.is_empty()), "no second receipt as mail");
    assert!(h.sync.receipt_answered(&id).await.unwrap());
}

#[tokio::test]
async fn without_mdn_the_receipt_goes_as_a_report() {
    let h = over(FakeJmap::new()).await;
    let id = asked(&h).await;
    h.sync.answer_receipt(&id, true).await.unwrap();
    let sent = h.fake.with(|s| s.sent.clone());
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].rcpt_to, ["ines@allotment.example"]);
    let raw = String::from_utf8_lossy(&sent[0].raw).into_owned();
    assert!(raw.contains("multipart/report") && raw.contains("Original-Message-ID: <rota@allotment.example>"), "{raw}");
    let read = mailrs_mime::read(&sent[0].raw).read_report.expect("the report reads back");
    assert_eq!((read.by.as_str(), read.displayed), ("me@example.com", true));
    assert!(h.fake.with(|s| s.emails[&id].keywords.contains("$mdnsent")), "other mail programs do not ask again");
}

#[tokio::test]
async fn declining_sends_nothing_and_asks_no_more() {
    let h = over(FakeJmap::new().with_capability(mailrs_jmap::MDN)).await;
    let id = asked(&h).await;
    assert!(!h.sync.receipt_answered(&id).await.unwrap());
    h.sync.answer_receipt(&id, false).await.unwrap();
    assert!(h.fake.with(|s| s.mdns.is_empty() && s.sent.is_empty()));
    assert!(h.sync.receipt_answered(&id).await.unwrap());
    assert!(h.fake.with(|s| s.emails[&id].keywords.contains("$mdnsent")), "RFC 3503 marks a refusal too");
}

#[tokio::test]
async fn a_message_that_asks_nothing_is_refused() {
    let h = over(FakeJmap::new()).await;
    let id = h.fake.deliver("inbox", b"From: a@example.com\r\nSubject: s\r\n\r\nhi\r\n", chrono::Utc::now().timestamp_millis());
    h.bootstrap_all().await;
    h.sync.body(&id).await.unwrap();
    assert!(h.sync.answer_receipt(&id, true).await.is_err());
    assert!(h.fake.with(|s| s.sent.is_empty()));
}

#[tokio::test]
async fn a_receipt_for_an_address_other_than_the_sender_is_refused() {
    let h = over(FakeJmap::new().with_capability(mailrs_jmap::MDN)).await;
    let id = asked_by(
        &h,
        b"From: Ines <ines@allotment.example>\r\nTo: me@example.com\r\nSubject: Rota\r\nMessage-ID: <x@allotment.example>\r\n\
Disposition-Notification-To: tracker@elsewhere.example\r\n\r\nhi\r\n",
    )
    .await;
    assert!(h.sync.answer_receipt(&id, true).await.is_err());
    assert!(h.fake.with(|s| s.mdns.is_empty() && s.sent.is_empty()));
    assert!(!h.sync.receipt_answered(&id).await.unwrap());
}

#[tokio::test]
async fn a_second_answer_sends_nothing_more() {
    let h = over(FakeJmap::new().with_capability(mailrs_jmap::MDN)).await;
    let id = asked(&h).await;
    h.sync.answer_receipt(&id, true).await.unwrap();
    h.sync.answer_receipt(&id, true).await.unwrap();
    assert_eq!(h.fake.with(|s| s.mdns.len()), 1);
}

#[tokio::test]
async fn a_restart_that_lost_the_local_record_still_sends_nothing_more() {
    let h = over(FakeJmap::new()).await;
    let id = asked(&h).await;
    // The answer went out and the server marked it, but this computer never
    // wrote its record.
    h.fake.with(|s| s.emails.get_mut(&id).unwrap().keywords.insert("$mdnsent".into()));
    h.bootstrap_all().await;
    h.sync.answer_receipt(&id, true).await.unwrap();
    assert!(h.fake.with(|s| s.sent.is_empty() && s.mdns.is_empty()));
}

#[tokio::test]
async fn a_lost_answer_is_not_asked_again_after_a_restart() {
    let h = over(FakeJmap::new().with_capability(mailrs_jmap::MDN)).await;
    let id = asked(&h).await;
    // The server took the receipt but the call that carried it failed here,
    // as when the connection drops before the answer arrives.
    h.fake.with(|s| s.emails.remove(&id));
    assert!(h.sync.answer_receipt(&id, true).await.is_err());
    assert!(h.sync.receipt_answered(&id).await.unwrap(), "recorded before the send, so a retry cannot double it");
}

/// A message from Ines to `to`, asking for a receipt.
fn asking_to(to: &str) -> Vec<u8> {
    format!(
        "From: Ines Duarte <ines@allotment.example>\r\nTo: {to}\r\nSubject: Rota\r\n\
Message-ID: <rota-{to}>\r\nDisposition-Notification-To: ines@allotment.example\r\n\r\nThe rota is up.\r\n"
    )
    .into_bytes()
}

#[tokio::test]
async fn a_receipt_for_mail_to_a_masked_address_goes_from_that_address() {
    let h = over(FakeJmap::new().with_capability(mailrs_jmap::MASKED_EMAIL)).await;
    let crate::AnyMail::Jmap(jmap) = h.mail() else { unreachable!() };
    let masked = jmap.make_masked("https://allotment.example", "Allotment").await.unwrap().email;
    lets_send_as(&h, &masked);
    let id = asked_by(&h, &asking_to(&masked)).await;
    assert!(h.sync.receipt_open(&id).await.unwrap(), "the card asks");
    h.sync.answer_receipt(&id, true).await.unwrap();
    let sent = h.fake.with(|s| s.sent.clone());
    assert_eq!(sent.len(), 1);
    let read = mailrs_mime::read(&sent[0].raw);
    assert_eq!(read.read_report.expect("a report").by, masked, "the sender learns nothing of the real address");
    let raw = String::from_utf8_lossy(&sent[0].raw).into_owned();
    assert!(!raw.contains("me@example.com"), "{raw}");
}

/// The server lets the account send as `address` too.
fn lets_send_as(h: &Harness, address: &str) {
    h.fake.with(|s| {
        let mut identity = s.identities[0].clone();
        identity.id = "I2".into();
        identity.email = address.to_string();
        s.identities.push(identity);
    });
}

#[tokio::test]
async fn mdn_send_never_names_another_address_than_the_one_mail_came_to() {
    let h = over(FakeJmap::new().with_capability(mailrs_jmap::MDN).with_capability(mailrs_jmap::MASKED_EMAIL)).await;
    let crate::AnyMail::Jmap(jmap) = h.mail() else { unreachable!() };
    let masked = jmap.make_masked("https://allotment.example", "Allotment").await.unwrap().email;
    let id = asked_by(&h, &asking_to(&masked)).await;
    // The server has no identity for the masked address, so MDN/send would
    // write the receipt from the account's own address.
    let _ = h.sync.answer_receipt(&id, true).await;
    assert!(h.fake.with(|s| s.mdns.is_empty()), "no receipt from the real address");
    let leaked = h.fake.with(|s| s.sent.iter().any(|sent| String::from_utf8_lossy(&sent.raw).contains("me@example.com")));
    assert!(!leaked);
}

#[tokio::test]
async fn mail_that_reached_none_of_my_addresses_asks_for_no_receipt() {
    let h = over(FakeJmap::new()).await;
    let id = asked_by(&h, &asking_to("allotment-list@lists.example")).await;
    assert!(!h.sync.receipt_open(&id).await.unwrap(), "no card");
    assert!(h.sync.answer_receipt(&id, true).await.is_err());
    assert!(h.fake.with(|s| s.sent.is_empty()));
}

#[tokio::test]
async fn an_answered_receipt_is_open_no_longer() {
    let h = over(FakeJmap::new()).await;
    let id = asked(&h).await;
    assert!(h.sync.receipt_open(&id).await.unwrap());
    h.sync.answer_receipt(&id, false).await.unwrap();
    assert!(!h.sync.receipt_open(&id).await.unwrap());
}
