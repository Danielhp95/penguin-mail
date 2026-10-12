//! Switching an IMAP account to JMAP on the same server. The stored mail
//! is matched to its JMAP ids by Message-ID and date, the mailboxes by
//! role and then by path, and the store rewrites the account in one
//! transaction (`mailrs_store::switch`). OBJECTID would name each
//! message's Email id directly, but the IMAP client cannot read `EMAILID`
//! yet; the Message-ID match works on every server.

use std::collections::{BTreeSet, HashMap};

use mailrs_jmap::{EmailQuery, Mailbox};
use mailrs_store::switch::{Moved, StoredMailbox, StoredMessage};
use serde_json::json;

use crate::BackendError;
use crate::services::JmapApi;
use crate::services::jmap::{backend, bare_message_id, has_message_id, received_millis};

/// Message-IDs one search asks for.
const IDS_PER_QUERY: usize = 100;

/// How far apart two dates of one message may be: the IMAP INTERNALDATE
/// and JMAP's `receivedAt` come from the same delivery, so a day covers
/// any zone or rounding.
const SAME_MESSAGE_WITHIN: i64 = 86_400_000;

/// Hits one search page asks for. The server may answer fewer.
const PAGE: usize = 500;

/// A mailbox path as one comparable string: IMAP's `.` or `/` separators
/// read as `/`, the `INBOX.` prefix of a dotted namespace drops, case folds.
fn normal(path: &str) -> String {
    let path = path.strip_prefix("INBOX.").or_else(|| path.strip_prefix("INBOX/")).unwrap_or(path);
    path.replace('.', "/").to_lowercase()
}

/// Each listed mailbox's path, joined by `/` from its parents' names.
fn paths(listed: &[Mailbox]) -> HashMap<String, String> {
    let by_id: HashMap<&str, &Mailbox> = listed.iter().map(|m| (m.id.as_str(), m)).collect();
    listed
        .iter()
        .map(|mailbox| {
            let mut names = vec![mailbox.name.as_str()];
            let mut at = mailbox.parent_id.as_deref().and_then(|p| by_id.get(p));
            while let Some(parent) = at {
                names.push(parent.name.as_str());
                if names.len() > 32 {
                    break;
                }
                at = parent.parent_id.as_deref().and_then(|p| by_id.get(p));
            }
            names.reverse();
            (names.join("/").to_lowercase(), mailbox.id.clone())
        })
        .collect()
}

/// Each stored mailbox's JMAP id, by role first, then by path. A mailbox
/// the server has no match for is left out; a JMAP mailbox is taken once.
pub fn match_mailboxes(stored: &[StoredMailbox], listed: &[Mailbox]) -> Vec<(String, String)> {
    let by_path = paths(listed);
    let mut taken = BTreeSet::new();
    let mut out = Vec::new();
    for mailbox in stored {
        let by_role = mailbox
            .role
            .as_deref()
            .and_then(|role| listed.iter().find(|m| m.role.as_deref() == Some(role)).map(|m| m.id.clone()));
        let found = by_role.or_else(|| by_path.get(&normal(&mailbox.id)).or_else(|| by_path.get(&normal(&mailbox.name))).cloned());
        if let Some(id) = found
            && taken.insert(id.clone())
        {
            out.push((mailbox.id.clone(), id));
        }
    }
    out
}

/// The JMAP ids of the messages with these Message-IDs. Pages by position
/// until a page comes back empty: a server that clamps a page to its own
/// limit answers fewer hits than asked while more remain.
async fn found_with<J: JmapApi>(api: &J, message_ids: &[&str]) -> Result<Vec<String>, BackendError> {
    let conditions: Vec<_> = message_ids.iter().map(|id| has_message_id(id)).collect();
    let filter = json!({ "operator": "OR", "conditions": conditions });
    let (mut ids, mut position) = (Vec::new(), 0u64);
    loop {
        let query = EmailQuery { filter: Some(filter.clone()), position, limit: PAGE, total: false };
        let hits = api.query(&query).await.map_err(backend)?.hits;
        if hits.is_empty() {
            return Ok(ids);
        }
        position += hits.len() as u64;
        ids.extend(hits.into_iter().map(|h| h.id));
    }
}

/// Each stored message's JMAP id and thread, by Message-ID and the
/// nearest `receivedAt` within a day. A JMAP id goes to one stored message
/// only, so two stored copies of one Message-ID never share it; the second
/// copy stays unmatched when the server holds just one. A message without a
/// Message-ID stays unmatched.
pub async fn match_messages<J: JmapApi>(api: &J, stored: &[StoredMessage]) -> Result<Vec<Moved>, BackendError> {
    let with_id: Vec<&StoredMessage> = stored.iter().filter(|m| m.message_id.as_deref().is_some_and(|i| !bare_message_id(i).is_empty())).collect();
    let mut taken = BTreeSet::new();
    let mut out = Vec::new();
    for chunk in with_id.chunks(IDS_PER_QUERY) {
        let wanted: Vec<&str> = chunk.iter().filter_map(|m| m.message_id.as_deref()).collect();
        let ids = found_with(api, &wanted).await?;
        // Only the ids and dates of this chunk's hits are held.
        let mut by_message_id: HashMap<String, Vec<(String, String, i64)>> = HashMap::new();
        for ids in ids.chunks(IDS_PER_QUERY) {
            let got = api.emails(ids, &["id", "threadId", "messageId", "receivedAt"]).await.map_err(backend)?;
            for email in got.list {
                let received = received_millis(&email);
                for id in email.message_id.iter().flatten() {
                    by_message_id.entry(bare_message_id(id).to_string()).or_default().push((email.id.clone(), email.thread_id.clone(), received));
                }
            }
        }
        for message in chunk {
            let Some(wanted) = message.message_id.as_deref().map(bare_message_id) else { continue };
            let best = by_message_id
                .get(wanted)
                .into_iter()
                .flatten()
                .filter(|(id, _, received)| !taken.contains(id) && (received - message.date).abs() <= SAME_MESSAGE_WITHIN)
                .min_by_key(|(_, _, received)| (received - message.date).abs());
            if let Some((id, thread, _)) = best {
                taken.insert(id.clone());
                out.push(Moved { from: message.id.clone(), to: id.clone(), thread: thread.clone() });
            }
        }
    }
    Ok(out)
}
