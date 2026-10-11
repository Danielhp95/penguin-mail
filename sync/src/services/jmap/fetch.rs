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

/// The window's filters, listed one after the other: mail received in the
/// last `days` days, then the older mail in `inbox`, which the window holds
/// whatever its age. One `OR` would say it in one filter, but Apache James
/// refuses a mailbox under `OR`. The two never overlap (`after` takes the
/// instant itself, `before` does not), and in this order the whole listing
/// stays newest first. A window of `mailrs_sync::ALL_MAIL` (0 days) takes
/// every message, which no filter says best.
pub(super) fn window_filters(days: i64, inbox: Option<&str>) -> Vec<Option<Value>> {
    let Some(after) = window_start(days) else {
        return vec![None];
    };
    let mut filters = vec![Some(json!({ "after": after }))];
    if let Some(inbox) = inbox {
        filters.push(Some(json!({ "inMailbox": inbox, "before": after })));
    }
    filters
}

/// When a window of `days` days starts, or `None` for every message.
fn window_start(days: i64) -> Option<String> {
    (days > 0).then(|| (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// A backfill cursor: which of the window's filters, and the position in
/// its listing. The first filter's cursor is the bare position.
fn cursor_of(filter: usize, position: u64) -> String {
    match filter {
        0 => position.to_string(),
        _ => format!("{filter}:{position}"),
    }
}

fn parse_cursor(text: &str) -> Option<(usize, u64)> {
    match text.split_once(':') {
        Some((filter, position)) => Some((filter.parse().ok()?, position.parse().ok()?)),
        None => Some((0, text.parse().ok()?)),
    }
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
        let (mut at, mut position) = match cursor {
            Some(text) => parse_cursor(text).ok_or(BackendError::StateLost)?,
            None => (0, 0),
        };
        let filters = window_filters(days, self.inbox_id().as_deref());
        let mut refs = Vec::new();
        // A page fills from one filter and then the next, so a short window
        // still brings the Inbox's older mail in the first page.
        while let Some(filter) = filters.get(at) {
            if refs.len() >= PAGE {
                return Ok(Backfill { refs, next: Some(cursor_of(at, position)) });
            }
            let query = EmailQuery { filter: filter.clone(), position, limit: PAGE - refs.len(), total: false };
            let found = self.api().query(&query).await.map_err(backend)?;
            // A server may answer fewer hits than asked, so only an empty
            // answer ends a filter's listing.
            match found.hits.is_empty() {
                true => (at, position) = (at + 1, 0),
                false => {
                    position += found.hits.len() as u64;
                    refs.extend(found.hits.into_iter().map(remote_ref));
                }
            }
        }
        Ok(Backfill { refs, next: None })
    }

    /// Every id `filter` matches, up to `most`, `IDS_PER_CALL` a call.
    async fn every_id(&self, filter: Option<Value>, most: usize) -> Result<Vec<RemoteRef>, BackendError> {
        let mut out = Vec::new();
        while out.len() < most {
            let limit = IDS_PER_CALL.min(most - out.len());
            let query = EmailQuery { filter: filter.clone(), position: out.len() as u64, limit, total: false };
            let found = self.api().query(&query).await.map_err(backend)?;
            if found.hits.is_empty() {
                break;
            }
            out.extend(found.hits.into_iter().take(most - out.len()).map(remote_ref));
        }
        Ok(out)
    }

    /// The ids the window holds, in `mailbox` when given. The Inbox is held
    /// whole whatever its age, so a window never narrows it.
    pub(super) async fn ids_in(&self, days: Option<i64>, mailbox: Option<&str>) -> Result<Vec<RemoteRef>, BackendError> {
        self.listed().await?;
        let inbox = self.inbox_id();
        let start = days.and_then(window_start);
        let (mailbox, start) = match (mailbox, start) {
            (Some(mailbox), Some(start)) if Some(mailbox) != inbox.as_deref() => (mailbox, start),
            (Some(mailbox), _) => return self.every_id(Some(json!({ "inMailbox": mailbox })), MOST_IDS).await,
            (None, _) => {
                let mut out = Vec::new();
                for filter in days.map_or_else(|| vec![None], |days| window_filters(days, inbox.as_deref())) {
                    let left = MOST_IDS - out.len();
                    if left == 0 {
                        break;
                    }
                    out.extend(self.every_id(filter, left).await?);
                }
                return Ok(out);
            }
        };
        // A mailbox other than the Inbox lists what the window holds of it:
        // its recent mail, and its older mail that is also in the Inbox,
        // which is stored whatever its age. Apache James takes one mailbox
        // condition a filter, so the older part lists the Inbox's older mail
        // and keeps what also sits in `mailbox`.
        let mut out = self.every_id(Some(json!({ "inMailbox": mailbox, "after": start })), MOST_IDS).await?;
        let Some(inbox) = inbox else {
            return Ok(out);
        };
        let left = MOST_IDS - out.len();
        let older = self.every_id(Some(json!({ "inMailbox": inbox, "before": start })), left).await?;
        let ids: Vec<String> = older.iter().map(|r| r.id.clone()).collect();
        let got = self.api().emails(&ids, &["id", "mailboxIds"]).await.map_err(backend)?;
        let held: std::collections::HashSet<String> =
            got.list.into_iter().filter(|e| e.mailbox_ids.get(mailbox).copied().unwrap_or(false)).map(|e| e.id).collect();
        out.extend(older.into_iter().filter(|r| held.contains(&r.id)));
        Ok(out)
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
        self.every_id(Some(json!({ "inMailbox": inbox })), MOST_IDS).await
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
