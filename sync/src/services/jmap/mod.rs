//! The JMAP adapter: an account on any JMAP server, over `mailrs-jmap`.
//! `J` is the client, the real `JmapClient` or `FakeJmap`. A message's id
//! is the server's Email id, which stays the same when the message moves,
//! so no remote ref is kept. The feed restates each changed message whole,
//! as the Microsoft adapter does with Graph's delta.

mod api;
mod bodies;
mod cadence;
mod calendar;
mod contacts;
mod errors;
mod feed;
#[cfg_attr(not(test), expect(dead_code, reason = "Q6 writes with the rest of it"))]
pub(crate) mod jscal;
mod fetch;
mod held;
mod identities;
mod mailboxes;
mod outgoing;
mod search;
mod storage;
mod sieve;
mod vacation;
mod writes;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use mailrs_domain::mailbox::keyword::{ANSWERED, DRAFT, FLAGGED, MUTED, SEEN};
use mailrs_domain::{EpochMillis, MailSet, RemoteMailbox, Role};
use mailrs_gmail::LabelColor;
use mailrs_jmap::{Identity, Mailbox};
use mailrs_mime::Parts;

pub use api::JmapApi;
pub use sieve::JmapSieve;
pub(crate) use errors::backend;
pub(crate) use fetch::received_millis;
pub(crate) use outgoing::{bare_message_id, has_message_id};
#[cfg(test)]
pub(crate) use bodies::numbered as numbered_for_tests;

use super::{
    Backfill, Changes, Found, Held, KeywordsPage, MailBackend, MailCapabilities, RawMessage, Relocated,
    RemoteRef, SearchQuery, Settled, Storage, SyncState, Unapplied, Unheld, Want,
};
use crate::api::{DraftRef, SavedDraft};
use crate::{BackendError, MailOp};

/// The keywords the adapter reads and restates. A JMAP server stores any
/// keyword; these are the ones the app acts on.
pub(super) const KEYWORDS: &[&str] = &[SEEN, FLAGGED, ANSWERED, DRAFT, MUTED];

/// What a JMAP account's mail service can do. `labels` follows the
/// session's `maxMailboxesPerEmail`, read at sign-in.
pub(super) fn capabilities_of(labels: bool) -> MailCapabilities {
    MailCapabilities {
        labels,
        server_threads: true,
        files_sent_mail: true,
        categories: false,
        delete_forever: true,
        batch_limit: 500,
        keywords: KEYWORDS,
        native_search: false,
        renames: false,
        restates: true,
        tags: false,
        focus: false,
        local_mailboxes: false,
        files_muted_replies: false,
    }
}

/// What the adapter needs to know about its account.
#[derive(Debug, Clone)]
pub struct JmapSettings {
    pub address: String,
    /// "Fastmail", "Stalwart", or the server's host.
    pub provider_name: String,
    pub window_days: i64,
    /// A message can sit in several mailboxes, from the session's
    /// `maxMailboxesPerEmail` at sign-in.
    pub labels: bool,
    /// The capabilities the session offered at sign-in, by URN.
    pub capabilities: Vec<String>,
    /// The automatic reply exists on the server but this sign-in cannot
    /// reach it, as with a Fastmail API token.
    pub auto_reply_needs_sign_in: bool,
}

/// What the adapter learned from its last listing. Its size follows the
/// account's mailboxes and identities, never its mail.
#[derive(Debug, Default)]
pub(super) struct Known {
    pub roles: BTreeMap<Role, String>,
    /// A mailbox's id to its path, `Parent/Child`.
    pub paths: HashMap<String, String>,
    pub mailboxes: HashMap<String, Mailbox>,
    /// The Mailbox state the last listing read.
    pub mailbox_state: String,
    pub listed: bool,
    pub window_open: bool,
    pub identities: Vec<Identity>,
    /// The server refused two mailbox conditions in one filter, as Apache
    /// James does, so a mailbox's older mail is found another way.
    pub one_mailbox_a_filter: bool,
    /// The server refused `anchor` in a query, as Apache James does, so a
    /// listing goes on by position.
    pub no_anchor: bool,
    /// Whether push works: `None` until the first watch says.
    pub push: Option<bool>,
    pub push_failures: u32,
    pub push_pause: Option<Duration>,
    /// Whether the server can call a hold back (`EmailSubmission/get`
    /// answers), asked the first time a message would be held.
    pub holds: Option<bool>,
    /// When the open push stream was opened; `None` while none is open.
    pub push_opened: Option<tokio::time::Instant>,
    /// The last read of the account's event changes, shared by every
    /// calendar that asks from the same state. Replaced by the next.
    pub calendar_changes: Option<Arc<calendar::EventChanges>>,
    /// Event `updated` stamps this adapter wrote, newest last, so its own
    /// later write does not read as a change made elsewhere.
    #[expect(dead_code, reason = "Q6 records its writes")]
    pub calendar_written: VecDeque<(String, String)>,
}

/// The blob ids of the parts of the last structures fetched, by part path.
#[derive(Debug, Default)]
pub(super) struct Handles {
    recent: VecDeque<(String, Vec<(String, String)>)>,
}

impl Handles {
    const KEPT: usize = 64;

    pub(super) fn remember(&mut self, id: &str, handles: Vec<(String, String)>) {
        self.recent.retain(|(m, _)| m != id);
        if self.recent.len() == Self::KEPT {
            self.recent.pop_front();
        }
        self.recent.push_back((id.to_string(), handles));
    }

    pub(super) fn get(&self, id: &str, path: &str) -> Option<String> {
        let (_, handles) = self.recent.iter().find(|(m, _)| m == id)?;
        handles.iter().find(|(p, _)| p == path).map(|(_, h)| h.clone())
    }
}

pub struct Jmap<J> {
    api: Arc<J>,
    settings: Arc<JmapSettings>,
    known: Arc<Mutex<Known>>,
    handles: Arc<Mutex<Handles>>,
}

// By hand: a derive would ask for `J: Clone`, and the client is shared.
impl<J> Clone for Jmap<J> {
    fn clone(&self) -> Self {
        Jmap {
            api: Arc::clone(&self.api),
            settings: Arc::clone(&self.settings),
            known: Arc::clone(&self.known),
            handles: Arc::clone(&self.handles),
        }
    }
}

impl<J: JmapApi> Jmap<J> {
    pub fn new(api: Arc<J>, settings: JmapSettings) -> Self {
        Jmap {
            api,
            settings: Arc::new(settings),
            known: Arc::new(Mutex::new(Known::default())),
            handles: Arc::new(Mutex::new(Handles::default())),
        }
    }

    pub(super) fn api(&self) -> &J {
        &self.api
    }

    pub(super) fn settings(&self) -> &JmapSettings {
        &self.settings
    }

    pub(super) fn known(&self) -> MutexGuard<'_, Known> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn handles(&self) -> MutexGuard<'_, Handles> {
        self.handles.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The mailbox's path, `Parent/Child`, once the account was listed:
    /// what a Sieve rule files into.
    pub fn mailbox_path(&self, id: &str) -> Option<String> {
        self.known().paths.get(id).cloned()
    }
}

impl<J: JmapApi> MailBackend for Jmap<J> {
    fn capabilities(&self) -> MailCapabilities {
        capabilities_of(self.settings.labels)
    }

    async fn storage(&self) -> Result<Option<Storage>, BackendError> {
        self.space().await
    }

    fn provider_name(&self) -> &str {
        &self.settings.provider_name
    }

    fn person_waiting(&self) -> bool {
        false
    }

    async fn stand_by(&self, wait: Duration) {
        tokio::time::sleep(wait).await;
    }

    fn made_by_person(&self, id: &str) -> bool {
        !self.known().roles.values().any(|r| r == id)
    }

    async fn unread_counts(&self) -> Result<HashMap<MailSet, i64>, BackendError> {
        self.listed().await?;
        let known = self.known();
        let mut counts = HashMap::new();
        for (id, mailbox) in &known.mailboxes {
            let unread = i64::try_from(mailbox.unread_emails).unwrap_or(i64::MAX);
            counts.insert(MailSet::Mailbox(id.clone()), unread);
            if let Some(role) = known.roles.iter().find(|(_, m)| *m == id).map(|(r, _)| *r) {
                counts.insert(MailSet::Role(role), unread);
            }
        }
        Ok(counts)
    }

    async fn mailboxes(&self) -> Result<Vec<RemoteMailbox>, BackendError> {
        self.list_mailboxes().await
    }

    async fn changes(&self, since: Option<&SyncState>) -> Result<Changes, BackendError> {
        self.feed(since).await
    }

    async fn create_mailbox(&self, name: &str) -> Result<RemoteMailbox, BackendError> {
        self.make_mailbox(name).await
    }

    async fn rename_mailbox(&self, id: &str, name: &str) -> Result<RemoteMailbox, BackendError> {
        self.move_mailbox(id, name).await
    }

    async fn delete_mailbox(&self, id: &str) -> Result<(), BackendError> {
        self.remove_mailbox(id).await
    }

    async fn set_mailbox_color(&self, _: &str, _: &LabelColor) -> Result<RemoteMailbox, BackendError> {
        Err(BackendError::Refused(mailrs_domain::translate::gettext("This server keeps no colors for its mailboxes.")))
    }

    async fn mailbox_threads(&self, id: &str) -> Result<u64, BackendError> {
        self.listed().await?;
        self.known().mailboxes.get(id).map(|m| m.total_threads).ok_or(BackendError::NotFound)
    }

    /// The feed covers every mailbox, so there is nothing to follow.
    fn follow(&self, _: &str) -> bool {
        false
    }

    fn set_window_open(&self, open: bool) {
        self.known().window_open = open;
    }

    fn poll_interval(&self) -> Option<Duration> {
        Some(self.poll_every())
    }

    async fn watch(&self) {
        self.watch_mail().await;
    }

    /// The feed names every keyword change, so there is nothing to page.
    async fn keywords_in(&self, _: &str, _: u32, _: RangeInclusive<u32>) -> Result<KeywordsPage, BackendError> {
        Ok(KeywordsPage::default())
    }

    async fn uidvalidity(&self, _: &str) -> Result<Option<u32>, BackendError> {
        Ok(None)
    }

    async fn keywords_stored(&self, _: &str) -> Result<&'static [&'static str], BackendError> {
        Ok(KEYWORDS)
    }

    async fn backfill(&self, days: i64, cursor: Option<&str>) -> Result<Backfill, BackendError> {
        self.backfill_page(days, cursor).await
    }

    async fn window_ids(&self, days: i64, mailbox: Option<&str>) -> Result<Vec<RemoteRef>, BackendError> {
        self.ids_in(Some(days), mailbox).await
    }

    async fn inbox_ids(&self) -> Result<Vec<RemoteRef>, BackendError> {
        self.inbox_listing().await
    }

    async fn search(&self, query: &SearchQuery, limit: usize) -> Result<Vec<RemoteRef>, BackendError> {
        self.find(query, limit).await
    }

    async fn find_sent(&self, message_id: &str) -> Result<Option<String>, BackendError> {
        self.sent_with(message_id).await
    }

    async fn fetch(&self, wants: Vec<Want>) -> Result<Found, BackendError> {
        self.fetch_metas(wants).await
    }

    async fn fetch_whole(&self, threads: Vec<String>) -> Result<Found, BackendError> {
        self.fetch_threads(threads).await
    }

    async fn fetch_raw(&self, ids: &[String]) -> Result<Vec<RawMessage>, BackendError> {
        self.raws(ids).await
    }

    /// The server files what the account sends, in the same request.
    async fn append(&self, _: &[u8], _: &str) -> Result<String, BackendError> {
        Err(BackendError::Refused(mailrs_domain::translate::gettext(
            "This server files sent mail itself.",
        )))
    }

    async fn fetch_structure(&self, id: &str) -> Result<Parts, BackendError> {
        self.structure(id).await
    }

    async fn fetch_part(&self, id: &str, path: &str) -> Result<Vec<u8>, BackendError> {
        self.part(id, path).await
    }

    fn mailbox_for(&self, role: Role) -> Option<String> {
        self.known().roles.get(&role).cloned()
    }

    fn set_of(&self, id: &str) -> MailSet {
        let role = self.known().roles.iter().find(|(_, m)| *m == id).map(|(r, _)| *r);
        match role {
            Some(role) => MailSet::Role(role),
            None => MailSet::Mailbox(id.to_string()),
        }
    }

    async fn apply(&self, messages: &[String], ops: &[MailOp]) -> Result<Vec<Relocated>, Unapplied> {
        self.write(messages, ops).await
    }

    async fn send(&self, raw: &[u8], _thread_id: Option<&str>) -> Result<String, BackendError> {
        self.send_raw(raw).await
    }

    /// The server threads a draft itself, so the thread id is not used.
    async fn save_draft(&self, draft_id: Option<&str>, raw: &[u8], _thread_id: Option<&str>) -> Result<SavedDraft, BackendError> {
        self.save(draft_id, raw).await
    }

    async fn send_draft(&self, draft_id: &str) -> Result<String, BackendError> {
        self.send_saved(draft_id).await
    }

    async fn delete_draft(&self, draft_id: &str) -> Result<(), BackendError> {
        self.drop_draft(draft_id).await
    }

    async fn list_drafts(&self) -> Result<Vec<DraftRef>, BackendError> {
        self.drafts().await
    }

    async fn schedule(&self, raw: &[u8], at: EpochMillis) -> Result<Held, BackendError> {
        self.hold_message(raw, at).await
    }

    async fn find_hold(&self, message_id: &str) -> Result<Option<Held>, BackendError> {
        self.held_copy(message_id).await
    }

    async fn unschedule(&self, handle: &str, message_id: &str, send_at: EpochMillis) -> Result<Unheld, BackendError> {
        self.release(handle, message_id, send_at).await
    }

    async fn settle(&self, handle: &str, message_id: &str, send_at: EpochMillis) -> Result<Settled, BackendError> {
        self.settle_hold(handle, message_id, send_at).await
    }
}
