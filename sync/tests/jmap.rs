//! The JMAP adapter over the real client, against Cyrus, Stalwart and
//! Apache James in Docker. The server's own JMAP, through the test's own
//! client in `mailrs_testmail`, plays the other client and the witness.
//! One test, as every Docker file here has.

use std::collections::BTreeMap;
use std::time::Duration;

use mailrs_domain::query::{Query, Term};
use mailrs_domain::{AccountId, ChangeEvent, Target};
use mailrs_jmap::{Auth, JmapClient};
use mailrs_store::{Db, accounts};
use mailrs_sync::{
    AccountServices, AccountSync, DEFAULT_BODY_CACHE_BYTES, JmapSettings, MailBackend, SearchQuery, TriageAction,
};
use mailrs_testmail::{JmapKind, JmapServer};
use rusqlite::OptionalExtension;
use serde_json::json;

const WINDOW_DAYS: i64 = 30;

/// How long the push step waits for the server to report a delivery.
const PUSH_WAIT: Duration = Duration::from_secs(10);

/// How often an account looks when push does not work.
const NO_PUSH_POLL: Duration = Duration::from_secs(60);

const MAIL: [&str; 2] = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"];

#[test]
fn the_jmap_adapter_syncs_and_writes_against_every_server() {
    if mailrs_testmail::jmap_docker().is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(mailrs_sync::WORKER_STACK)
        .enable_all()
        .build()
        .expect("a runtime");
    let body = runtime.spawn(async move {
        let password = mailrs_testmail::password();
        for kind in JmapKind::ALL {
            eprintln!("{kind:?}");
            let Some(server) = JmapServer::start(kind, &password).await else {
                return;
            };
            Run::new(&server, &password).await.all().await;
        }
    });
    runtime.block_on(body).unwrap_or_else(|panic| std::panic::resume_unwind(panic.into_panic()));
}

struct Run<'a> {
    server: &'a JmapServer,
    password: &'a str,
    db: Db,
    account_id: AccountId,
    sync: AccountSync,
    _events: async_channel::Receiver<ChangeEvent>,
    _dir: tempfile::TempDir,
}

fn services(server: &JmapServer, password: &str, db: &Db, account_id: AccountId) -> AccountServices {
    let client = JmapClient::new(&server.session_url, Auth::Basic { user: server.user.clone(), password: password.to_string() })
        .expect("a client for the server");
    let settings = JmapSettings {
        address: server.address.clone(),
        provider_name: format!("{:?}", server.kind),
        window_days: WINDOW_DAYS,
        labels: true,
    };
    AccountServices::jmap(db.clone(), account_id, client, settings)
}

fn incoming(message_id: &str, subject: &str, to: &str) -> Vec<u8> {
    format!(
        "From: Ada Lovelace <ada@example.test>\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <{message_id}>\r\nDate: {}\r\n\r\n{subject} body\r\n",
        chrono::Utc::now().to_rfc2822()
    )
    .into_bytes()
}

impl<'a> Run<'a> {
    async fn new(server: &'a JmapServer, password: &'a str) -> Run<'a> {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let db = Db::open(&dir.path().join("mail.db")).expect("the store opens");
        let address = server.address.clone();
        let account_id = db
            .write(move |c| Ok(accounts::insert_jmap_account(c, &address, "JMAP", 0)?.expect("a fresh store")))
            .await
            .expect("the account row");
        let (events, receiver) = async_channel::unbounded();
        let sync = AccountSync::new(account_id, services(server, password, &db, account_id), db.clone(), events)
            .with_limits(WINDOW_DAYS, DEFAULT_BODY_CACHE_BYTES);
        Run { server, password, db, account_id, sync, _events: receiver, _dir: dir }
    }

    fn kind(&self) -> JmapKind {
        self.server.kind
    }

    fn id(&self, name: &str) -> String {
        format!("{name}.{:?}@example.test", self.kind())
    }

    fn mail(&self) -> &mailrs_sync::AnyMail {
        &self.sync.services().mail
    }

    /// The store's id and thread for the message with `message_id`.
    async fn stored(&self, message_id: &str) -> Option<(String, String)> {
        let (account, wanted) = (self.account_id, message_id.to_string());
        self.db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT id, thread_id FROM messages WHERE account_id = ?1 AND rfc822_msgid IN (?2, '<' || ?2 || '>')",
                    rusqlite::params![account, wanted],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?)
            })
            .await
            .expect("the store answers")
    }

    /// The server's word on where `email` sits and what it carries, or
    /// `None` when the server no longer has it.
    async fn on_server(&self, email: &str) -> Option<(BTreeMap<String, bool>, BTreeMap<String, bool>)> {
        let account = self.server.mail_account(self.password).await;
        let got = self
            .server
            .call(
                self.password,
                &MAIL,
                json!([["Email/get", {"accountId": account, "ids": [email], "properties": ["mailboxIds", "keywords"]}, "0"]]),
            )
            .await;
        let found = got["methodResponses"][0][1]["list"].get(0)?;
        let map = |v: &serde_json::Value| serde_json::from_value(v.clone()).unwrap_or_default();
        Some((map(&found["mailboxIds"]), map(&found["keywords"])))
    }

    /// The id of the server's mailbox with `role`, if it has one.
    async fn role(&self, role: &str) -> Option<String> {
        let account = self.server.mail_account(self.password).await;
        let got = self
            .server
            .call(self.password, &MAIL, json!([["Mailbox/get", {"accountId": account, "ids": null, "properties": ["role"]}, "0"]]))
            .await;
        got["methodResponses"][0][1]["list"]
            .as_array()
            .and_then(|list| list.iter().find(|m| m["role"] == role))
            .and_then(|m| m["id"].as_str())
            .map(str::to_string)
    }

    async fn sent_mailbox(&self) -> String {
        self.role("sent").await.unwrap_or_else(|| panic!("{:?} has no Sent mailbox", self.kind()))
    }

    async fn all(self) {
        let started = std::time::Instant::now();
        self.mail_arrives_and_lands_in_the_store().await;
        self.a_flag_reaches_the_server().await;
        self.archive_keeps_the_message_in_a_mailbox().await;
        self.search_finds_by_subject().await;
        self.a_body_opens().await;
        self.a_send_lands_in_sent().await;
        self.a_draft_saves_replaces_and_sends().await;
        eprintln!("{:?}: the steps took {:?}", self.kind(), started.elapsed());
        self.a_delivery_during_a_watch_reaches_the_store().await;
    }

    async fn mail_arrives_and_lands_in_the_store(&self) {
        self.server.deliver(self.password, &incoming(&self.id("welcome"), "Welcome", &self.server.address)).await;
        self.sync.bootstrap().await.expect("the first sync");
        assert!(self.stored(&self.id("welcome")).await.is_some(), "{:?}: the welcome message", self.kind());
        let later = self.server.deliver(self.password, &incoming(&self.id("later"), "Later", &self.server.address)).await;
        self.sync.incremental().await.expect("a look");
        assert_eq!(self.stored(&self.id("later")).await.map(|(id, _)| id), Some(later), "{:?}", self.kind());
    }

    async fn a_flag_reaches_the_server(&self) {
        let (id, thread) = self.stored(&self.id("welcome")).await.expect("stored");
        self.sync.triage_all(&[Target::thread(self.account_id, thread)], &TriageAction::Star, None).await.expect("a star");
        let (_, keywords) = self.on_server(&id).await.expect("still there");
        assert_eq!(keywords.get("$flagged"), Some(&true), "{:?}", self.kind());
    }

    /// Archive on a label account takes the message out of the Inbox. It
    /// lands in Archive, or, on a server without one, the adapter refuses
    /// and the message stays where it was.
    async fn archive_keeps_the_message_in_a_mailbox(&self) {
        let (id, thread) = self.stored(&self.id("later")).await.expect("stored");
        let (inbox, archive) = (self.role("inbox").await.expect("an Inbox"), self.role("archive").await);
        let archived = self.sync.triage_all(&[Target::thread(self.account_id, thread)], &TriageAction::Archive, None).await;
        let (mailboxes, _) = self.on_server(&id).await.expect("still there");
        let mailboxes: Vec<String> = mailboxes.into_keys().collect();
        match archive {
            Some(archive) => {
                archived.expect("archive");
                assert_eq!(mailboxes, [archive], "{:?}: out of the Inbox, into Archive", self.kind());
            }
            None => {
                eprintln!("{:?}: no Archive mailbox, so Archive is refused", self.kind());
                assert!(archived.is_err(), "{:?}: Archive without an Archive mailbox", self.kind());
                assert_eq!(mailboxes, [inbox], "{:?}: the message stays in the Inbox", self.kind());
            }
        }
    }

    async fn search_finds_by_subject(&self) {
        let query = Query::term(Term::Subject("Welcome".into()));
        let found = self.mail().search(&SearchQuery::Tree(query), 10).await.expect("a search");
        let (id, _) = self.stored(&self.id("welcome")).await.expect("stored");
        assert!(found.iter().any(|r| r.id == id), "{:?}: {found:?}", self.kind());
    }

    async fn a_body_opens(&self) {
        let (id, _) = self.stored(&self.id("welcome")).await.expect("stored");
        let raw = self.mail().fetch_raw(&[id]).await.expect("the raw message");
        assert!(String::from_utf8_lossy(&raw[0].bytes).contains("Welcome body"), "{:?}", self.kind());
    }

    async fn a_send_lands_in_sent(&self) {
        let raw = format!(
            "From: {0}\r\nTo: {0}\r\nSubject: Sent here\r\nMessage-ID: <{1}>\r\n\r\nOut\r\n",
            self.server.address,
            self.id("sent")
        );
        let id = self.mail().send(raw.as_bytes(), None).await.expect("a send");
        let (mailboxes, keywords) = self.on_server(&id).await.expect("the sent copy");
        let sent = self.sent_mailbox().await;
        assert_eq!(mailboxes.into_keys().collect::<Vec<_>>(), std::slice::from_ref(&sent), "{:?}: the copy sits in Sent", self.kind());
        assert!(!keywords.contains_key("$draft"), "{:?}: {keywords:?}", self.kind());
        // A mailbox's share of the window, as the relist after a lost state
        // asks for it.
        let listed = self.mail().window_ids(WINDOW_DAYS, Some(&sent)).await.expect("Sent's window");
        assert!(listed.iter().any(|r| r.id == id), "{:?}: {listed:?}", self.kind());
    }

    /// A draft edited, then saved again unchanged, keeps one copy on the
    /// server, which then sends it and files it in Sent. Cyrus keeps one
    /// copy of the same bytes and answers the unchanged save with the copy
    /// it has; the others make a new one.
    async fn a_draft_saves_replaces_and_sends(&self) {
        let letter = |body: &str| format!("From: {0}\r\nTo: {0}\r\nSubject: Draft\r\n\r\n{body}\r\n", self.server.address).into_bytes();
        let mail = self.mail();
        let first = mail.save_draft(None, &letter("One"), None).await.expect("a draft");
        let edited = mail.save_draft(Some(&first.draft_id), &letter("Two"), None).await.expect("an edit");
        assert!(self.on_server(&first.draft_id).await.is_none(), "{:?}: the old copy is gone", self.kind());
        let again = mail.save_draft(Some(&edited.draft_id), &letter("Two"), None).await.expect("an unchanged save");
        let listed: Vec<String> = mail.list_drafts().await.expect("the drafts").into_iter().map(|d| d.draft_id).collect();
        assert_eq!(listed, std::slice::from_ref(&again.draft_id), "{:?}: one draft", self.kind());
        if again.draft_id == edited.draft_id {
            eprintln!("{:?}: an unchanged save keeps the draft's id", self.kind());
        }

        let sent = mail.send_draft(&again.draft_id).await.expect("the draft goes out");
        let (mailboxes, keywords) = self.on_server(&sent).await.expect("the sent draft");
        assert_eq!(mailboxes.into_keys().collect::<Vec<_>>(), [self.sent_mailbox().await], "{:?}", self.kind());
        assert!(!keywords.contains_key("$draft"), "{:?}: {keywords:?}", self.kind());
        assert!(mail.list_drafts().await.expect("the drafts").is_empty(), "{:?}", self.kind());

        let third = mail.save_draft(None, &letter("Three"), None).await.expect("another draft");
        mail.delete_draft(&third.draft_id).await.expect("a delete");
        assert!(mail.list_drafts().await.expect("the drafts").is_empty(), "{:?}", self.kind());
    }

    /// A server with working EventSource push wakes the watch when mail
    /// arrives; one without it, or whose stream fails, leaves the poll to
    /// find the mail. Either way the next look stores it. The log says
    /// which the server gave.
    ///
    /// The watch runs on an adapter of its own, dropped once the wait is
    /// over: Cyrus 3.13 answers no other request, on any connection, while
    /// an EventSource stream is open, so there the delivery waits until the
    /// watch gives up and its stream closes.
    async fn a_delivery_during_a_watch_reaches_the_store(&self) {
        let watcher = services(self.server, self.password, &self.db, self.account_id).mail;
        let mut watching = tokio::spawn({
            let watcher = watcher.clone();
            async move { watcher.watch().await }
        });
        // Let the stream open before the delivery, so the delivery is what
        // it reports.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let early = watching.is_finished();
        let started = std::time::Instant::now();
        let raw = incoming(&self.id("pushed"), "Pushed", &self.server.address);
        let delivery = self.server.deliver(self.password, &raw);
        let waited = async {
            let woke = tokio::time::timeout(PUSH_WAIT, &mut watching).await.is_ok();
            watching.abort();
            let pushed = watcher.poll_interval() != Some(NO_PUSH_POLL);
            drop(watcher);
            (woke, pushed)
        };
        let (_, (woke, pushed)) = tokio::join!(delivery, waited);
        let said = match (early, woke, pushed) {
            (true, _, _) => "the watch ended before the delivery",
            (false, true, true) => "push woke the watch on the delivery",
            (false, true, false) => "the push stream failed, so the minute poll looks",
            (false, false, _) => "the watch slept through the delivery, so the minute poll looks",
        };
        eprintln!("{:?}: {said}; the delivery took {:?}", self.kind(), started.elapsed());
        self.sync.incremental().await.expect("a look");
        assert!(self.stored(&self.id("pushed")).await.is_some(), "{:?}: the pushed message", self.kind());
    }
}
