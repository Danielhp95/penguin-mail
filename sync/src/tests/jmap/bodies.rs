//! Raw messages and parts over FakeJmap.

use mailrs_jmap::BodyPart;

use super::{fastmail, raw};
use crate::MailBackend;
use crate::services::jmap::numbered_for_tests as numbered;

/// A message with a text body and one PDF file.
fn with_file(message_id: &str) -> Vec<u8> {
    format!(
        "From: Ada Lovelace <ada@example.com>\r\nTo: me@example.com\r\nSubject: Report\r\nMessage-ID: <{message_id}>\r\n\
         MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSee the report.\r\n\
         --b\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\nContent-Disposition: attachment; filename=\"report.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8gUERG\r\n--b--\r\n"
    )
    .into_bytes()
}

fn downloads(h: &super::Harness) -> usize {
    h.fake.with(|s| s.calls.iter().filter(|c| *c == "download").count())
}

#[tokio::test]
async fn a_raw_message_is_its_blob() {
    let h = fastmail().await;
    let bytes = raw("r1@x", "Raw");
    let id = h.fake.deliver("inbox", &bytes, 1_000);
    let got = h.mail().fetch_raw(std::slice::from_ref(&id)).await.unwrap();
    assert_eq!(got[0].bytes, bytes);
    assert_eq!(got[0].id, id);
}

#[tokio::test]
async fn a_message_past_the_cap_says_it_is_too_large() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &raw("big@x", "Big"), 1_000);
    h.fake.with(|s| {
        let blob = s.emails[&id].blob_id.clone();
        s.oversized.insert(blob);
    });
    match h.mail().fetch_raw(&[id]).await {
        Err(crate::BackendError::Refused(said)) => assert_eq!(said, "This message is too large to open."),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_structure_carries_the_text_and_leaves_the_file_on_the_server() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &with_file("s1@x"), 1_000);
    let parts = h.mail().fetch_structure(&id).await.unwrap();
    assert!(!parts.incomplete);
    assert_eq!(parts.header("Subject"), Some("Report"));
    assert_eq!(parts.find("1").unwrap().data.as_deref(), Some(&b"See the report."[..]));
    let file = parts.find("2").unwrap();
    assert!(file.attachment);
    assert_eq!(file.filename.as_deref(), Some("report.pdf"));
    assert_eq!(file.data, None);
}

#[tokio::test]
async fn a_file_comes_alone_by_its_part_path() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &with_file("p1@x"), 1_000);
    // Without a structure read first, the part asks for the structure itself.
    assert_eq!(h.mail().fetch_part(&id, "2").await.unwrap(), b"Hello PDF");
    let before = downloads(&h);
    assert_eq!(h.mail().fetch_part(&id, "2").await.unwrap(), b"Hello PDF");
    assert_eq!(downloads(&h) - before, 1, "the second read reuses the remembered blob id");
}

#[tokio::test]
async fn a_part_the_message_lacks_is_not_found() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &with_file("p2@x"), 1_000);
    assert!(matches!(h.mail().fetch_part(&id, "9").await, Err(crate::BackendError::NotFound)));
}

#[tokio::test]
async fn a_text_part_that_fails_to_download_marks_the_structure_incomplete() {
    let h = fastmail().await;
    let id = h.fake.deliver("inbox", &with_file("s2@x"), 1_000);
    h.fake.with(|s| {
        let blob = format!("{}/1", s.emails[&id].blob_id);
        s.oversized.insert(blob);
    });
    let parts = h.mail().fetch_structure(&id).await.unwrap();
    assert!(parts.incomplete);
}

#[test]
fn parts_take_imap_section_numbers() {
    let leaf = |t: &str, blob: &str| BodyPart { mime_type: t.into(), blob_id: Some(blob.into()), size: 5, ..BodyPart::default() };
    let alternative = BodyPart {
        mime_type: "multipart/alternative".into(),
        sub_parts: Some(vec![leaf("text/plain", "B1"), leaf("text/html", "B2")]),
        ..BodyPart::default()
    };
    let file = BodyPart { disposition: Some("attachment".into()), name: Some("a.pdf".into()), ..leaf("application/pdf", "B3") };
    let root = BodyPart { mime_type: "multipart/mixed".into(), sub_parts: Some(vec![alternative, file]), ..BodyPart::default() };
    let part = numbered(&root);
    assert_eq!(part.path, "");
    assert_eq!(part.children[0].children[1].path, "1.2");
    assert_eq!(part.children[1].path, "2");
    assert!(part.children[1].attachment);
    assert_eq!(part.children[1].filename.as_deref(), Some("a.pdf"));
    let single = numbered(&leaf("text/plain", "B9"));
    assert_eq!(single.path, "1");
}

/// A conversation goes out in batches: one `Email/get` names every
/// message's blob, rather than one per message, and the file reads as
/// one message at a time would have written it.
#[tokio::test]
async fn a_conversation_exports_its_messages_in_one_batch() {
    let h = fastmail().await;
    let first = h.fake.deliver("inbox", &raw("e1@x", "Lunch"), 1_000);
    let mut ids = vec![first.clone()];
    for n in 2..=3 {
        let reply = format!(
            "From: Ada Lovelace <ada@example.com>\r\nTo: me@example.com\r\nSubject: Re: Lunch\r\nMessage-ID: <e{n}@x>\r\nIn-Reply-To: <e1@x>\r\n\r\nReply {n}\r\n"
        );
        ids.push(h.fake.deliver("inbox", reply.as_bytes(), 1_000 + n));
    }
    h.bootstrap_all().await;
    let thread = h.fake.with(|s| s.emails[&first].thread_id.clone());
    let mut one_by_one = Vec::new();
    for id in &ids {
        let raw = h.mail().fetch_raw(std::slice::from_ref(id)).await.unwrap().remove(0);
        crate::export::append(&mut one_by_one, &raw.bytes);
    }
    let gets = |h: &super::Harness| h.fake.with(|s| s.calls.iter().filter(|c| *c == "Email/get").count());
    let before = gets(&h);
    let mbox = h.sync.export_mbox(&thread, None).await.unwrap();
    assert_eq!(String::from_utf8(mbox).unwrap(), String::from_utf8(one_by_one).unwrap());
    assert_eq!(gets(&h) - before, 1, "one for every message's blob");
}
