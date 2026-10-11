//! The window, ids, metadata and whole threads. A listing is `Email/query`,
//! newest first, a page or an id batch a request.

use mailrs_domain::{Address, Memberships, MessageMeta, Role};
use mailrs_jmap::{Email, EmailAddress, EmailQuery, META};
use serde_json::{Value, json};

use super::{Jmap, JmapApi, backend};
use crate::BackendError;
use crate::services::{Backfill, Found, RemoteRef, Want};

/// One page of the window.
const PAGE: usize = 200;

/// The most ids a window or inbox listing returns.
const MOST_IDS: usize = 20_000;

/// Ids one call of an id listing asks for.
const IDS_PER_CALL: usize = 1000;

/// The most messages of one thread a whole-thread fetch keeps.
const MOST_IN_THREAD: usize = 500;

pub(super) fn received_millis(email: &Email) -> i64 {
    email
        .received_at
        .as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.timestamp_millis())
        .unwrap_or_default()
}

/// The window's filter: mail received in the last `days` days, or sitting
/// in `inbox` whatever its age. A window of `mailrs_sync::ALL_MAIL` (0
/// days) takes every message, which no filter says best.
pub(super) fn window_filter(days: i64, inbox: Option<&str>) -> Option<Value> {
    if days <= 0 {
        return None;
    }
    let after = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    Some(match inbox {
        Some(inbox) => json!({ "operator": "OR", "conditions": [{ "after": after }, { "inMailbox": inbox }] }),
        None => json!({ "after": after }),
    })
}

fn address(from: &EmailAddress) -> Option<Address> {
    (!from.email.is_empty()).then(|| Address { name: from.name.clone().filter(|n| !n.is_empty()), email: from.email.clone() })
}

fn remote_ref(hit: mailrs_jmap::Hit) -> RemoteRef {
    RemoteRef { id: hit.id, thread_id: hit.thread_id }
}

impl<J: JmapApi> Jmap<J> {
    /// A message's metadata. The roles come from the last mailbox listing,
    /// so call `listed()` first.
    pub(super) fn meta_of(&self, email: &Email) -> MessageMeta {
        let mailboxes: Vec<String> = email.mailbox_ids.iter().filter(|(_, on)| **on).map(|(id, _)| id.clone()).collect();
        let roles: Vec<Role> = self.known().roles.iter().filter(|(_, id)| mailboxes.contains(id)).map(|(role, _)| *role).collect();
        let keywords: Vec<String> = email.keywords.iter().filter(|(_, on)| **on).map(|(k, _)| k.clone()).collect();
        let people = |list: &Option<Vec<EmailAddress>>| list.iter().flatten().filter_map(address).collect::<Vec<_>>();
        MessageMeta {
            account_id: 0,
            id: email.id.clone(),
            thread_id: email.thread_id.clone(),
            rfc822_msgid: email
                .message_id
                .as_ref()
                .and_then(|ids| ids.first())
                .map(|m| m.trim().trim_matches(['<', '>']).to_string()),
            from: email.from.iter().flatten().find_map(address),
            to: people(&email.to),
            cc: people(&email.cc),
            subject: email.subject.clone().unwrap_or_default(),
            date: received_millis(email),
            snippet: email.preview.as_deref().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" "),
            size: i64::try_from(email.size).unwrap_or(i64::MAX),
            has_attachments: email.has_attachment,
            held: Memberships { mailboxes, keywords, categories: Vec::new() },
            roles,
            list_unsubscribe: email.list_unsubscribe.as_deref().map(|v| v.trim().to_string()),
            one_click: email.list_unsubscribe_post.as_deref().is_some_and(|v| v.to_ascii_lowercase().contains("one-click")),
        }
    }

    fn inbox_id(&self) -> Option<String> {
        self.known().roles.get(&Role::Inbox).cloned()
    }

    pub(super) async fn backfill_page(&self, days: i64, cursor: Option<&str>) -> Result<Backfill, BackendError> {
        self.listed().await?;
        let position = match cursor {
            Some(text) => text.parse::<u64>().map_err(|_| BackendError::StateLost)?,
            None => 0,
        };
        let query = EmailQuery { filter: window_filter(days, self.inbox_id().as_deref()), position, limit: PAGE, total: false };
        let found = self.api().query(&query).await.map_err(backend)?;
        let got = found.hits.len();
        let refs = found.hits.into_iter().map(remote_ref).collect();
        // A server may answer fewer hits than asked, so only an empty page
        // is the last one.
        let next = (got > 0).then(|| (position + got as u64).to_string());
        Ok(Backfill { refs, next })
    }

    /// Every id `filter` matches, up to `MOST_IDS`, `IDS_PER_CALL` a call.
    async fn every_id(&self, filter: Option<Value>) -> Result<Vec<RemoteRef>, BackendError> {
        let mut out = Vec::new();
        loop {
            let query = EmailQuery { filter: filter.clone(), position: out.len() as u64, limit: IDS_PER_CALL, total: false };
            let found = self.api().query(&query).await.map_err(backend)?;
            let got = found.hits.len();
            out.extend(found.hits.into_iter().map(remote_ref));
            if got == 0 || out.len() >= MOST_IDS {
                out.truncate(MOST_IDS);
                return Ok(out);
            }
        }
    }

    /// The ids the window holds, in `mailbox` when given. The Inbox is held
    /// whole whatever its age, so a window never narrows it.
    pub(super) async fn ids_in(&self, days: Option<i64>, mailbox: Option<&str>) -> Result<Vec<RemoteRef>, BackendError> {
        self.listed().await?;
        let inbox = self.inbox_id();
        // A mailbox other than the Inbox lists what the window holds, which
        // includes the whole Inbox: a message there is stored whatever its age.
        let window = days.and_then(|days| window_filter(days, inbox.as_deref()));
        let filter = match (mailbox, window) {
            (Some(mailbox), _) if Some(mailbox) == inbox.as_deref() => Some(json!({ "inMailbox": mailbox })),
            (Some(mailbox), Some(window)) => Some(json!({ "operator": "AND", "conditions": [{ "inMailbox": mailbox }, window] })),
            (Some(mailbox), None) => Some(json!({ "inMailbox": mailbox })),
            (None, window) => window,
        };
        self.every_id(filter).await
    }

    /// Every message in the Inbox, refused as `Unsupported` past
    /// `MOST_IDS`, which leaves nothing to compare.
    pub(super) async fn inbox_listing(&self) -> Result<Vec<RemoteRef>, BackendError> {
        self.listed().await?;
        let inbox = self.inbox_id().ok_or(BackendError::NotFound)?;
        let total = self.known().mailboxes.get(&inbox).map_or(0, |m| m.total_emails);
        if usize::try_from(total).unwrap_or(usize::MAX) > MOST_IDS {
            return Err(BackendError::Unsupported);
        }
        self.every_id(Some(json!({ "inMailbox": inbox }))).await
    }

    pub(super) async fn fetch_metas(&self, wants: Vec<Want>) -> Result<Found, BackendError> {
        self.listed().await?;
        let ids: Vec<String> = wants.into_iter().map(|w| w.id).collect();
        let got = self.api().emails(&ids, META).await.map_err(backend)?;
        Ok(Found { metas: got.list.iter().map(|e| self.meta_of(e)).collect(), gone: got.not_found, ..Found::default() })
    }

    pub(super) async fn fetch_threads(&self, threads: Vec<String>) -> Result<Found, BackendError> {
        self.listed().await?;
        let got = self.api().threads(&threads).await.map_err(backend)?;
        let mut found = Found { gone_threads: got.not_found, ..Found::default() };
        for thread in &got.threads {
            let mut metas: Vec<MessageMeta> =
                got.emails.iter().filter(|e| e.thread_id == thread.id).map(|e| self.meta_of(e)).collect();
            metas.sort_by_key(|m| m.date);
            metas.truncate(MOST_IN_THREAD);
            found.metas.extend(metas.iter().cloned());
            found.whole.push(metas);
        }
        Ok(found)
    }

    pub(super) async fn sent_with(&self, message_id: &str) -> Result<Option<String>, BackendError> {
        self.listed().await?;
        let header = json!({ "header": ["Message-ID", format!("<{}>", message_id.trim().trim_matches(['<', '>']))] });
        let filter = match self.known().roles.get(&Role::Sent) {
            Some(sent) => json!({ "operator": "AND", "conditions": [header, { "inMailbox": sent }] }),
            None => header,
        };
        let query = EmailQuery { filter: Some(filter), position: 0, limit: 1, total: false };
        let found = self.api().query(&query).await.map_err(backend)?;
        Ok(found.hits.into_iter().next().map(|h| h.id))
    }
}
