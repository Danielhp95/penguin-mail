//! Switching an IMAP account to JMAP: matching stored mail to JMAP ids.

use std::sync::Arc;

use mailrs_jmap::{
    Email, EmailQuery, Got, Identity, Imported, JmapError, Listed, MailChanges, Mailbox, MailboxWrite, Pushed, Queried, Saved,
    Session, SetResponse, Submission, Threads,
};
use mailrs_store::switch::{StoredMailbox, StoredMessage};
use serde_json::{Map, Value};

use crate::fake::FakeJmap;
use crate::services::JmapApi;
use crate::switch::{match_mailboxes, match_messages};

fn raw(message_id: &str) -> Vec<u8> {
    format!("From: a@example.com\r\nTo: me@example.com\r\nSubject: s\r\nMessage-ID: <{message_id}>\r\n\r\nx\r\n").into_bytes()
}

fn stored(id: &str, message_id: Option<&str>, date: i64) -> StoredMessage {
    StoredMessage { id: id.into(), message_id: message_id.map(str::to_string), date }
}

/// A server whose `Email/query` answers at most `most` hits a call, as the
/// real client does when `maxObjectsInGet` is small.
struct Clamped {
    fake: Arc<FakeJmap>,
    most: usize,
}

type Answer<T> = Result<T, JmapError>;

impl JmapApi for Clamped {
    async fn session(&self) -> Answer<Arc<Session>> {
        self.fake.session().await
    }
    async fn mailboxes(&self) -> Answer<Listed<Mailbox>> {
        self.fake.mailboxes().await
    }
    async fn mail_changes(&self, email: &str, mailbox: &str, max: usize) -> Answer<MailChanges> {
        self.fake.mail_changes(email, mailbox, max).await
    }
    async fn email_state(&self) -> Answer<String> {
        self.fake.email_state().await
    }
    async fn query(&self, query: &EmailQuery) -> Answer<Queried> {
        self.fake.query(&EmailQuery { limit: query.limit.min(self.most), ..query.clone() }).await
    }
    async fn query_after(&self, query: &EmailQuery, anchor: &str) -> Answer<Queried> {
        self.fake.query_after(&EmailQuery { limit: query.limit.min(self.most), ..query.clone() }, anchor).await
    }
    async fn emails(&self, ids: &[String], properties: &[&str]) -> Answer<Got<Email>> {
        self.fake.emails(ids, properties).await
    }
    async fn threads(&self, ids: &[String]) -> Answer<Threads> {
        self.fake.threads(ids).await
    }
    async fn set_emails(&self, update: &Map<String, Value>, destroy: &[String]) -> Answer<SetResponse> {
        self.fake.set_emails(update, destroy).await
    }
    async fn set_mailbox(&self, write: &MailboxWrite) -> Answer<String> {
        self.fake.set_mailbox(write).await
    }
    async fn download(&self, blob_id: &str, limit: usize) -> Answer<Vec<u8>> {
        self.fake.download(blob_id, limit).await
    }
    async fn upload(&self, bytes: Vec<u8>) -> Answer<String> {
        self.fake.upload(bytes).await
    }
    async fn import(&self, blob_id: &str, mailbox_ids: &[String], keywords: &[&str], destroy: Option<&str>) -> Answer<Saved> {
        self.fake.import(blob_id, mailbox_ids, keywords, destroy).await
    }
    async fn submit(&self, submission: &Submission) -> Answer<Imported> {
        self.fake.submit(submission).await
    }
    async fn identities(&self) -> Answer<Vec<Identity>> {
        self.fake.identities().await
    }
    async fn push(&self) -> Answer<Pushed> {
        self.fake.push().await
    }
    fn push_paused(&self) -> Option<tokio::time::Instant> {
        self.fake.push_paused()
    }
}

#[tokio::test]
async fn a_message_matches_by_its_message_id_and_date() {
    let fake = FakeJmap::new();
    let jmap_id = fake.deliver("inbox", &raw("a@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some("a@x"), 1_000_500), stored("INBOX/7/2", None, 0)]).await.unwrap();
    assert_eq!(moved.len(), 1, "no Message-ID, no match");
    assert_eq!(moved[0].from, "INBOX/7/1");
    assert_eq!(moved[0].to, jmap_id);
    assert_eq!(moved[0].thread, fake.with(|s| s.emails[&jmap_id].thread_id.clone()));
}

#[tokio::test]
async fn a_stored_id_with_angle_brackets_still_matches() {
    let fake = FakeJmap::new();
    let jmap_id = fake.deliver("inbox", &raw("a@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some(" <a@x> "), 1_000_000)]).await.unwrap();
    assert_eq!(moved[0].to, jmap_id);
}

#[tokio::test]
async fn two_copies_of_one_message_id_never_share_a_jmap_id() {
    let fake = FakeJmap::new();
    let first = fake.deliver("inbox", &raw("dup@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some("dup@x"), 1_000_000), stored("Copy/3/1", Some("dup@x"), 1_000_000)])
        .await
        .unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].to, first);
}

#[tokio::test]
async fn two_copies_on_the_server_go_to_two_stored_copies() {
    let fake = FakeJmap::new();
    let first = fake.deliver("inbox", &raw("dup@x"), 1_000_000);
    let second = fake.deliver("inbox", &raw("dup@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some("dup@x"), 1_000_000), stored("Copy/3/1", Some("dup@x"), 1_000_000)])
        .await
        .unwrap();
    let mut to: Vec<_> = moved.iter().map(|m| m.to.clone()).collect();
    to.sort();
    let mut want = [first, second];
    want.sort();
    assert_eq!(to, want);
}

#[tokio::test]
async fn a_far_off_date_is_another_message() {
    let fake = FakeJmap::new();
    fake.deliver("inbox", &raw("old@x"), 1_000_000);
    let moved = match_messages(&fake, &[stored("INBOX/7/1", Some("old@x"), 1_000_000 + 3 * 86_400_000)]).await.unwrap();
    assert!(moved.is_empty());
}

#[tokio::test]
async fn a_server_that_clamps_its_query_pages_loses_no_match() {
    let fake = Arc::new(FakeJmap::new());
    let ids: Vec<String> = (0..7).map(|n| format!("m{n}@x")).collect();
    let jmap: Vec<String> = ids.iter().map(|id| fake.deliver("inbox", &raw(id), 1_000_000)).collect();
    let kept: Vec<StoredMessage> = ids.iter().enumerate().map(|(n, id)| stored(&format!("INBOX/7/{n}"), Some(id), 1_000_000)).collect();
    let moved = match_messages(&Clamped { fake: Arc::clone(&fake), most: 2 }, &kept).await.unwrap();
    let mut to: Vec<_> = moved.into_iter().map(|m| m.to).collect();
    to.sort();
    let mut want = jmap;
    want.sort();
    assert_eq!(to, want);
}

#[test]
fn mailboxes_match_by_role_then_by_path() {
    let listed = vec![
        Mailbox { id: "mb-inbox".into(), name: "Inbox".into(), role: Some("inbox".into()), ..Default::default() },
        Mailbox { id: "mb-trips".into(), name: "Trips".into(), ..Default::default() },
        Mailbox { id: "mb-rome".into(), name: "Rome".into(), parent_id: Some("mb-trips".into()), ..Default::default() },
    ];
    let stored = vec![
        StoredMailbox { id: "INBOX".into(), name: "Inbox".into(), role: Some("inbox".into()) },
        StoredMailbox { id: "INBOX.Trips.Rome".into(), name: "Trips.Rome".into(), role: None },
        StoredMailbox { id: "Gone".into(), name: "Gone".into(), role: None },
    ];
    let mut matched = match_mailboxes(&stored, &listed);
    matched.sort();
    assert_eq!(matched, [("INBOX".to_string(), "mb-inbox".to_string()), ("INBOX.Trips.Rome".to_string(), "mb-rome".to_string())]);
}
