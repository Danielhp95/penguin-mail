//! Drafts and sending over FakeJmap.

use mailrs_jmap::{Identity, SUBMISSION};

use super::{fastmail, over};
use crate::fake::FakeJmap;
use crate::{BackendError, MailBackend};

fn letter(from: &str, extra: &str) -> Vec<u8> {
    format!(
        "From: Me <{from}>\r\nTo: ada@example.com\r\n{extra}Subject: Hi\r\nMessage-ID: <out-1@example.com>\r\n\r\nHello\r\n"
    )
    .into_bytes()
}

fn alias(id: &str, email: &str) -> Identity {
    Identity {
        id: id.into(),
        name: String::new(),
        email: email.into(),
        reply_to: None,
        bcc: None,
        text_signature: String::new(),
        html_signature: String::new(),
        may_delete: true,
    }
}

#[tokio::test]
async fn bcc_goes_in_the_envelope_and_not_in_the_copy() {
    let h = fastmail().await;
    h.mail().send(&letter("me@example.com", "Bcc: secret@example.com\r\n"), None).await.unwrap();
    let sent = h.fake.with(|s| s.sent[0].clone());
    assert!(sent.rcpt_to.contains(&"secret@example.com".to_string()));
    assert!(sent.rcpt_to.contains(&"ada@example.com".to_string()));
    assert!(!String::from_utf8_lossy(&sent.raw).to_ascii_lowercase().contains("bcc:"));
}

#[tokio::test]
async fn the_copy_lands_in_sent_without_draft() {
    let h = fastmail().await;
    let id = h.mail().send(&letter("me@example.com", ""), None).await.unwrap();
    let (mailboxes, keywords) = h.fake.with(|s| (s.emails[&id].mailbox_ids.clone(), s.emails[&id].keywords.clone()));
    assert_eq!(mailboxes.into_iter().collect::<Vec<_>>(), [h.fake.mailbox("sent")]);
    assert!(!keywords.contains("$draft"));
}

#[tokio::test]
async fn an_alias_sends_as_its_own_identity() {
    let h = fastmail().await;
    h.fake.with(|s| s.identities.push(alias("I2", "Alias@Example.com")));
    h.mail().send(&letter("alias@example.com", ""), None).await.unwrap();
    assert_eq!(h.fake.with(|s| s.sent[0].identity_id.clone()), "I2");
}

#[tokio::test]
async fn a_whole_domain_identity_covers_any_address_there() {
    let h = fastmail().await;
    h.fake.with(|s| s.identities.push(alias("I3", "*@example.org")));
    h.mail().send(&letter("shop@example.org", ""), None).await.unwrap();
    assert_eq!(h.fake.with(|s| s.sent[0].identity_id.clone()), "I3");
}

#[tokio::test]
async fn a_draft_saved_twice_keeps_one_copy_under_a_new_id() {
    let h = fastmail().await;
    let first = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    let second = h.mail().save_draft(Some(&first.draft_id), &letter("me@example.com", ""), None).await.unwrap();
    assert_ne!(first.draft_id, second.draft_id);
    assert!(h.fake.with(|s| !s.emails.contains_key(&first.draft_id)));
    let listed: Vec<String> = h.mail().list_drafts().await.unwrap().into_iter().map(|d| d.draft_id).collect();
    assert_eq!(listed, std::slice::from_ref(&second.draft_id));
    let sent = h.mail().send_draft(&second.draft_id).await.unwrap();
    assert!(h.fake.with(|s| !s.emails.contains_key(&second.draft_id) && s.emails.contains_key(&sent)));
}

#[tokio::test]
async fn an_unchanged_draft_saved_again_keeps_its_one_copy() {
    let h = over(FakeJmap::new().keeping_one_copy()).await;
    let first = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    let second = h.mail().save_draft(Some(&first.draft_id), &letter("me@example.com", ""), None).await.unwrap();
    let listed: Vec<String> = h.mail().list_drafts().await.unwrap().into_iter().map(|d| d.draft_id).collect();
    assert_eq!(listed, std::slice::from_ref(&second.draft_id));
}

#[tokio::test]
async fn an_unchanged_draft_sends_as_the_server_s_own_copy_and_stays_in_sent() {
    let h = over(FakeJmap::new().keeping_one_copy()).await;
    let draft = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    let sent = h.mail().send_draft(&draft.draft_id).await.unwrap();
    assert_eq!(sent, draft.draft_id, "the server keeps one copy of the same bytes");
    let (mailboxes, keywords) = h.fake.with(|s| (s.emails[&sent].mailbox_ids.clone(), s.emails[&sent].keywords.clone()));
    assert_eq!(mailboxes.into_iter().collect::<Vec<_>>(), [h.fake.mailbox("sent")]);
    assert!(!keywords.contains("$draft"));
    assert!(h.mail().list_drafts().await.unwrap().is_empty());
}

#[tokio::test]
async fn saving_a_draft_with_the_bytes_of_another_message_is_refused_in_words() {
    let h = over(FakeJmap::new().keeping_one_copy()).await;
    let raw = letter("me@example.com", "");
    let kept = h.fake.deliver("inbox", &raw, 0);
    let refused = h.mail().save_draft(None, &raw, None).await;
    assert!(matches!(&refused, Err(BackendError::Refused(said)) if !said.contains("alreadyExists")), "{refused:?}");
    let (mailboxes, keywords) = h.fake.with(|s| (s.emails[&kept].mailbox_ids.clone(), s.emails[&kept].keywords.clone()));
    assert_eq!(mailboxes.into_iter().collect::<Vec<_>>(), [h.fake.mailbox("inbox")]);
    assert!(!keywords.contains("$draft"));
}

#[tokio::test]
async fn sending_from_an_unchanged_draft_keeps_the_sent_copy() {
    let h = over(FakeJmap::new().keeping_one_copy()).await;
    h.sync.refresh_labels().await.unwrap();
    let raw = letter("me@example.com", "");
    let draft = h.sync.save_draft(raw.clone(), None, None).await.unwrap();
    let sent = h.sync.send(raw, None, Some(draft.draft_id.clone())).await.unwrap();
    let (mailboxes, keywords) = h.fake.with(|s| (s.emails[&sent].mailbox_ids.clone(), s.emails[&sent].keywords.clone()));
    assert_eq!(mailboxes.into_iter().collect::<Vec<_>>(), [h.fake.mailbox("sent")]);
    assert!(!keywords.contains("$draft"));
}

#[tokio::test]
async fn a_draft_whose_old_copy_stays_still_saves_and_tries_the_old_one_again() {
    let h = fastmail().await;
    let first = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    h.fake.with(|s| s.refused.insert(first.draft_id.clone()));
    let second = h.mail().save_draft(Some(&first.draft_id), &letter("me@example.com", ""), None).await.unwrap();
    assert!(h.fake.with(|s| s.emails.contains_key(&second.draft_id)));
    assert_eq!(h.fake.with(|s| s.calls.iter().filter(|c| *c == "Email/set").count()), 1);
}

#[tokio::test]
async fn a_draft_whose_old_copy_is_gone_saves_without_trying_again() {
    let h = fastmail().await;
    let first = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    h.fake.destroy(&first.draft_id);
    h.mail().save_draft(Some(&first.draft_id), &letter("me@example.com", ""), None).await.unwrap();
    assert_eq!(h.fake.with(|s| s.calls.iter().filter(|c| *c == "Email/set").count()), 0);
}

#[tokio::test]
async fn a_read_only_account_refuses_to_send_in_words() {
    let h = over(FakeJmap::new().without_capability(SUBMISSION)).await;
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    assert!(matches!(refused, Err(BackendError::Refused(_))), "{refused:?}");
    assert!(h.fake.with(|s| s.sent.is_empty()));
}

#[tokio::test]
async fn a_read_only_account_refuses_to_save_or_send_a_draft() {
    let h = over(FakeJmap::new().without_capability(SUBMISSION)).await;
    let saved = h.mail().save_draft(None, &letter("me@example.com", ""), None).await;
    assert!(matches!(saved, Err(BackendError::Refused(_))), "{saved:?}");
    let draft = h.fake.deliver("drafts", &letter("me@example.com", ""), 0);
    let sent = h.mail().send_draft(&draft).await;
    assert!(matches!(sent, Err(BackendError::Refused(_))), "{sent:?}");
    assert!(h.fake.with(|s| s.sent.is_empty() && s.emails.len() == 1));
}

#[tokio::test]
async fn an_account_without_identities_says_so() {
    let h = fastmail().await;
    h.fake.with(|s| s.identities.clear());
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    assert!(matches!(refused, Err(BackendError::Refused(_))), "{refused:?}");
}

#[tokio::test]
async fn a_refused_submission_says_why_and_leaves_no_copy() {
    let h = over(FakeJmap::new().refusing_submissions("forbiddenToSend")).await;
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    let Err(BackendError::Refused(why)) = refused else { panic!("{refused:?}") };
    assert_eq!(why, "The server does not let this account send mail now.");
    assert!(h.fake.with(|s| s.sent.is_empty() && s.emails.is_empty()));
}

#[tokio::test]
async fn an_account_with_no_drafts_sent_or_archive_refuses_to_send_in_words() {
    let h = fastmail().await;
    h.fake.with(|s| s.mailboxes.retain(|_, m| !matches!(m.role.as_deref(), Some("drafts" | "sent" | "archive"))));
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    let Err(BackendError::Refused(why)) = refused else { panic!("{refused:?}") };
    assert_eq!(why, "The server has neither a Drafts nor a Sent mailbox to keep the message in.");
    assert!(h.fake.with(|s| s.sent.is_empty()));
}

#[tokio::test]
async fn a_draft_sent_on_a_try_whose_answer_was_lost_is_not_sent_again() {
    let h = fastmail().await;
    let draft = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    h.fake.with(|s| s.lose_submission_answer = true);
    let lost = h.mail().send_draft(&draft.draft_id).await;
    assert!(matches!(lost, Err(BackendError::Offline(_))), "{lost:?}");
    let sent = h.mail().send_draft(&draft.draft_id).await.unwrap();
    assert_eq!(h.fake.with(|s| s.sent.len()), 1);
    assert!(h.fake.with(|s| !s.emails.contains_key(&draft.draft_id) && s.emails.contains_key(&sent)));
}

/// Cyrus sends the draft's own copy and files it in Sent, so after a lost
/// answer the retry finds that copy again under the draft's id.
#[tokio::test]
async fn a_draft_the_server_keeps_once_goes_once_when_the_first_answer_was_lost() {
    let h = over(FakeJmap::new().keeping_one_copy()).await;
    let draft = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    h.fake.with(|s| s.lose_submission_answer = true);
    let lost = h.mail().send_draft(&draft.draft_id).await;
    assert!(matches!(lost, Err(BackendError::Offline(_))), "{lost:?}");
    let sent = h.mail().send_draft(&draft.draft_id).await.unwrap();
    assert_eq!(sent, draft.draft_id);
    assert_eq!(h.fake.with(|s| s.sent.len()), 1, "one submission in all");
    let (mailboxes, keywords) = h.fake.with(|s| (s.emails[&sent].mailbox_ids.clone(), s.emails[&sent].keywords.clone()));
    assert_eq!(mailboxes.into_iter().collect::<Vec<_>>(), [h.fake.mailbox("sent")]);
    assert!(!keywords.contains("$draft"));
}

#[tokio::test]
async fn without_sent_the_copy_goes_to_archive_and_is_no_draft() {
    let h = fastmail().await;
    h.fake.with(|s| s.mailboxes.retain(|_, m| m.role.as_deref() != Some("sent")));
    let id = h.mail().send(&letter("me@example.com", ""), None).await.unwrap();
    let (mailboxes, keywords) = h.fake.with(|s| (s.emails[&id].mailbox_ids.clone(), s.emails[&id].keywords.clone()));
    assert_eq!(mailboxes.into_iter().collect::<Vec<_>>(), [h.fake.mailbox("archive")]);
    assert!(!keywords.contains("$draft"));
    assert!(h.mail().list_drafts().await.unwrap().is_empty());
}

#[tokio::test]
async fn without_sent_or_archive_the_copy_waits_in_drafts_as_no_draft() {
    let h = fastmail().await;
    h.fake.with(|s| s.mailboxes.retain(|_, m| !matches!(m.role.as_deref(), Some("sent" | "archive"))));
    let id = h.mail().send(&letter("me@example.com", ""), None).await.unwrap();
    assert!(h.fake.with(|s| !s.emails[&id].keywords.contains("$draft")));
    assert!(h.mail().list_drafts().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_from_the_account_does_not_own_is_refused_in_words() {
    let h = fastmail().await;
    let refused = h.mail().send(&letter("stranger@example.net", ""), None).await;
    let Err(BackendError::Refused(why)) = refused else { panic!("{refused:?}") };
    assert_eq!(why, "The server does not let this account send from stranger@example.net.");
    assert!(h.fake.with(|s| s.sent.is_empty()));
}

#[tokio::test]
async fn a_message_too_large_to_send_says_so() {
    let h = over(FakeJmap::new().refusing_submissions("tooLarge")).await;
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    let Err(BackendError::Refused(why)) = refused else { panic!("{refused:?}") };
    assert_eq!(why, "The message is larger than the server sends.");
}

#[tokio::test]
async fn a_draft_with_bcc_sends_it_in_the_envelope_only_and_goes() {
    let h = fastmail().await;
    let draft = h.mail().save_draft(None, &letter("me@example.com", "Bcc: secret@example.com\r\n"), None).await.unwrap();
    h.mail().send_draft(&draft.draft_id).await.unwrap();
    let sent = h.fake.with(|s| s.sent[0].clone());
    assert!(sent.rcpt_to.contains(&"secret@example.com".to_string()));
    assert!(!String::from_utf8_lossy(&sent.raw).to_ascii_lowercase().contains("bcc:"));
    assert!(h.fake.with(|s| !s.emails.contains_key(&draft.draft_id)));
}

#[tokio::test]
async fn a_refused_draft_stays_in_drafts() {
    let h = over(FakeJmap::new().refusing_submissions("forbiddenToSend")).await;
    let draft = h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    let refused = h.mail().send_draft(&draft.draft_id).await;
    assert!(matches!(refused, Err(BackendError::Refused(_))), "{refused:?}");
    let listed: Vec<String> = h.mail().list_drafts().await.unwrap().into_iter().map(|d| d.draft_id).collect();
    assert_eq!(listed, std::slice::from_ref(&draft.draft_id));
}

#[tokio::test]
async fn a_refused_envelope_sender_reads_as_a_refused_from() {
    let h = over(FakeJmap::new().refusing_submissions("forbiddenMailFrom")).await;
    let refused = h.mail().send(&letter("me@example.com", ""), None).await;
    let Err(BackendError::Refused(why)) = refused else { panic!("{refused:?}") };
    assert_eq!(why, "The server does not let this account send from me@example.com.");
}

/// With no Sent mailbox to look in, only the Message-ID tells a sent copy
/// apart, and the draft carries the same one: the draft is never taken
/// for the message that went out.
#[tokio::test]
async fn without_sent_the_draft_is_not_taken_for_its_sent_copy() {
    let h = fastmail().await;
    h.fake.with(|s| s.mailboxes.retain(|_, m| m.role.as_deref() != Some("sent")));
    h.mail().save_draft(None, &letter("me@example.com", ""), None).await.unwrap();
    assert_eq!(h.mail().find_sent("<out-1@example.com>").await.unwrap(), None);
    let id = h.mail().send(&letter("me@example.com", ""), None).await.unwrap();
    assert_eq!(h.mail().find_sent("out-1@example.com").await.unwrap(), Some(id));
}

/// The outbox sends a message from a draft on Cyrus, which sends the
/// draft's own copy; the answer is lost. The retry finds that copy in Sent
/// under the draft's id and must not delete it as the draft.
#[tokio::test]
async fn an_outbox_retry_keeps_the_sent_copy_that_was_the_draft() {
    use std::collections::HashMap;
    use std::sync::Arc;

    use mailrs_store::outbox::{self, Queued};

    use crate::tests::Connected;
    use crate::{Outbox, Posted, now_millis};

    let h = over(FakeJmap::new().keeping_one_copy()).await;
    h.sync.refresh_labels().await.unwrap();
    let raw = letter("me@example.com", "");
    let draft = h.sync.save_draft(raw.clone(), None, None).await.unwrap();
    h.fake.with(|s| s.lose_submission_answer = true);
    let queue = Outbox::new(Arc::new(Connected(HashMap::from([(h.account_id, Arc::clone(&h.sync))]))), h.db.clone());
    let message = Queued {
        account_id: h.account_id,
        subject: "Hi".into(),
        recipients: "ada@example.com".into(),
        send_at: now_millis(),
        raw: Some(raw),
        draft_id: Some(draft.draft_id.clone()),
        composer: "{}".into(),
        ..Queued::default()
    };
    let posted = queue.post(message).await.unwrap();
    assert!(matches!(posted, Posted::Waiting(_)), "{posted:?}");

    h.db.write(|c| outbox::try_now(c, now_millis())).await.unwrap();
    let drained = queue.send_due(now_millis()).await.unwrap();
    assert_eq!(drained.sent.len(), 1, "the retry counts it as sent");
    assert_eq!(h.fake.with(|s| s.sent.len()), 1, "and sends it once");
    let kept = h.fake.with(|s| s.emails.get(&draft.draft_id).map(|e| e.mailbox_ids.clone()));
    assert_eq!(kept.map(|m| m.into_iter().collect::<Vec<_>>()), Some(vec![h.fake.mailbox("sent")]), "the sent copy stays");
}

/// A submission whose outcome no answer or look settles is no refusal: the
/// outbox keeps the message, looks in Sent on the retry, and sends once.
#[tokio::test]
async fn an_unclear_submission_is_retried_with_a_look_and_goes_once() {
    use std::collections::HashMap;
    use std::sync::Arc;

    use mailrs_store::outbox::{self, Queued};

    use crate::tests::Connected;
    use crate::{Outbox, Posted, now_millis};

    let h = fastmail().await;
    h.sync.refresh_labels().await.unwrap();
    h.fake.with(|s| s.unclear_submission_answer = true);
    let queue = Outbox::new(Arc::new(Connected(HashMap::from([(h.account_id, Arc::clone(&h.sync))]))), h.db.clone());
    let message = Queued {
        account_id: h.account_id,
        subject: "Hi".into(),
        recipients: "ada@example.com".into(),
        send_at: now_millis(),
        raw: Some(letter("me@example.com", "")),
        composer: "{}".into(),
        ..Queued::default()
    };
    let posted = queue.post(message).await.unwrap();
    assert!(matches!(posted, Posted::Waiting(_)), "{posted:?}");
    h.db.write(|c| outbox::try_now(c, now_millis())).await.unwrap();
    let drained = queue.send_due(now_millis()).await.unwrap();
    assert_eq!(drained.sent.len(), 1, "the retry finds it sent");
    assert_eq!(h.fake.with(|s| s.sent.len()), 1, "and sends it once");
}

/// The app stops while a first try is on its way, after the server may
/// have taken it. The row already counts the try, so the pass after the
/// claim lapses looks in Sent and finds it there rather than send again.
#[tokio::test]
async fn a_first_try_cut_off_mid_send_is_looked_for_in_sent() {
    use std::collections::HashMap;
    use std::sync::Arc;

    use mailrs_store::outbox::{self, Queued};

    use crate::tests::Connected;
    use crate::{Outbox, now_millis};

    let h = fastmail().await;
    h.sync.refresh_labels().await.unwrap();
    let queue = Outbox::new(Arc::new(Connected(HashMap::from([(h.account_id, Arc::clone(&h.sync))]))), h.db.clone());
    let raw = letter("me@example.com", "");
    let message = Queued {
        account_id: h.account_id,
        subject: "Hi".into(),
        recipients: "ada@example.com".into(),
        send_at: now_millis(),
        raw: Some(raw.clone()),
        composer: "{}".into(),
        ..Queued::default()
    };
    let id = h.db.write(move |c| outbox::put(c, &message)).await.unwrap();
    h.fake.with(|s| s.upload_delay = std::time::Duration::from_secs(3600));
    let uploading = async {
        while !h.fake.with(|s| s.calls.iter().any(|c| c == "upload")) {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        _ = queue.send_due(now_millis()) => panic!("the send was meant to hang"),
        () = uploading => {}
    }
    let row = h.db.read(move |c| outbox::find(c, id)).await.unwrap().unwrap();
    assert_eq!(row.attempts, 1, "the try counts before it goes");
    // The server took it before the app stopped, and the claim lapsed.
    h.fake.deliver("sent", &raw, now_millis());
    h.db.write(move |c| outbox::release(c, id)).await.unwrap();
    h.fake.with(|s| s.upload_delay = std::time::Duration::ZERO);
    let drained = queue.send_due(now_millis()).await.unwrap();
    assert_eq!(drained.sent.len(), 1);
    assert!(h.fake.with(|s| s.sent.is_empty()), "found in Sent, not sent again");
}

/// Deleting a draft by an id that names a message without `$draft`, such
/// as the sent copy a one-copy server made of it, leaves that message and
/// counts the draft as gone.
#[tokio::test]
async fn deleting_a_draft_never_destroys_a_message_that_is_no_draft() {
    let h = fastmail().await;
    let sent = h.mail().send(&letter("me@example.com", ""), None).await.unwrap();
    h.mail().delete_draft(&sent).await.unwrap();
    assert!(h.fake.with(|s| s.emails.contains_key(&sent)));
}
