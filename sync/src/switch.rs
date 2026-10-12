//! Switching an IMAP account to JMAP on the same server. The stored mail
//! is matched to its JMAP ids by Message-ID and date, the mailboxes by
//! role and then by path, and the store rewrites the account in one
//! transaction (`mailrs_store::switch`). OBJECTID would name each
//! message's Email id directly, but the IMAP client cannot read `EMAILID`
//! yet; the Message-ID match works on every server.
//!
//! Nothing in the store changes before the last step, and a failure before
//! it leaves the IMAP account as it was. Drafts the JMAP server lacks go
//! up to it first, from the bytes kept here, so the switch loses none.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use mailrs_domain::translate::gettext;
use mailrs_domain::{Account, AccountId, Provider};
use mailrs_jmap::{Auth, EmailQuery, JmapClient, JmapError, Mailbox};
use mailrs_store::Db;
use mailrs_store::jmap::{JmapAuth, JmapSaved};
use mailrs_store::services::ServiceKind;
use mailrs_store::switch::{Adoption, Moved, StoredMailbox, StoredMessage, adopt_jmap, stored_mailboxes, stored_messages};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::passwords::{PasswordError, PasswordStore, Secrets};
use crate::services::JmapApi;
use crate::services::jmap::{backend, bare_message_id, has_message_id, received_millis};
use crate::{AnyJmap, BackendError, SyncError};

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

/// What the server holds under these Message-IDs: each bare Message-ID's
/// JMAP ids, threads and `receivedAt`. Only one chunk's hits are held.
async fn hits<J: JmapApi>(api: &J, message_ids: &[&str]) -> Result<HashMap<String, Vec<(String, String, i64)>>, BackendError> {
    let ids = found_with(api, message_ids).await?;
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
    Ok(by_message_id)
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
        let by_message_id = hits(api, &wanted).await?;
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

/// The ids of the stored drafts: what sits in Drafts, and what a waiting
/// outbox row names as its draft.
fn stored_drafts(conn: &Connection, account_id: AccountId) -> mailrs_store::Result<BTreeSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT m.id FROM messages m WHERE m.account_id = ?1 AND (\
         EXISTS (SELECT 1 FROM message_mailboxes s CROSS JOIN mailboxes b ON b.key = s.mailbox \
             WHERE s.account_id = m.account_id AND s.message_id = m.id AND b.role = 'drafts') \
         OR m.id IN (SELECT draft_id FROM outbox WHERE account_id = ?1 AND draft_id IS NOT NULL))",
    )?;
    let ids = stmt.query_map(params![account_id], |row| row.get(0))?;
    Ok(ids.collect::<rusqlite::Result<_>>()?)
}

/// A draft's bytes as kept here: its local copy, or the bytes an outbox
/// row waits to send it from. An IMAP draft usually has neither, since
/// its bytes live on the server.
fn draft_bytes(conn: &Connection, account_id: AccountId, id: &str) -> mailrs_store::Result<Option<Vec<u8>>> {
    if let Some(raw) = mailrs_store::local_messages::get(conn, account_id, id)? {
        return Ok(Some(raw));
    }
    Ok(conn
        .query_row(
            "SELECT raw FROM outbox WHERE account_id = ?1 AND draft_id = ?2 AND raw IS NOT NULL LIMIT 1",
            params![account_id, id],
            |row| row.get(0),
        )
        .optional()?)
}

/// Pairs each stored draft the date match left out with a JMAP copy:
/// one the server holds under its Message-ID at any date, or one imported
/// now from the bytes kept here. An import dates its copy on arrival, so a
/// copy an earlier, failed switch put there never matches by date; going
/// by Message-ID alone keeps a second switch from importing it again. A
/// draft with no bytes here and no copy there has nothing left to carry.
async fn carry_drafts<J: JmapApi>(
    db: &Db,
    account_id: AccountId,
    api: &J,
    listed: &[Mailbox],
    stored: &[StoredMessage],
    drafts: &BTreeSet<String>,
    moved: &mut Vec<Moved>,
) -> Result<(), SwitchError> {
    let matched: BTreeSet<&str> = moved.iter().map(|m| m.from.as_str()).collect();
    let left: Vec<&StoredMessage> = stored.iter().filter(|m| drafts.contains(&m.id) && !matched.contains(m.id.as_str())).collect();
    if left.is_empty() {
        return Ok(());
    }
    let mut taken: BTreeSet<String> = moved.iter().map(|m| m.to.clone()).collect();
    let wanted: Vec<&str> = left.iter().filter_map(|m| m.message_id.as_deref()).filter(|i| !bare_message_id(i).is_empty()).collect();
    let mut held: HashMap<String, Vec<(String, String, i64)>> = HashMap::new();
    for chunk in wanted.chunks(IDS_PER_QUERY) {
        for (message_id, found) in hits(api, chunk).await? {
            held.entry(message_id).or_default().extend(found);
        }
    }
    let drafts_mailbox = listed.iter().find(|m| m.role.as_deref() == Some("drafts")).map(|m| m.id.clone());
    for draft in left {
        let copy = draft
            .message_id
            .as_deref()
            .and_then(|i| held.get(bare_message_id(i)))
            .into_iter()
            .flatten()
            .find(|(id, _, _)| !taken.contains(id));
        if let Some((to, thread, _)) = copy {
            taken.insert(to.clone());
            moved.push(Moved { from: draft.id.clone(), to: to.clone(), thread: thread.clone() });
            continue;
        }
        let from = draft.id.clone();
        let Some(raw) = db.read(move |c| draft_bytes(c, account_id, &from)).await? else {
            continue;
        };
        let mailbox = drafts_mailbox.clone().ok_or_else(|| BackendError::Refused(gettext("The server has no Drafts mailbox.")))?;
        let blob = api.upload(raw).await.map_err(backend)?;
        let made = api.import(&blob, &[mailbox], &["$draft", "$seen"], None).await.map_err(backend)?.email;
        let thread = match made.thread_id.is_empty() {
            false => made.thread_id,
            true => {
                let got = api.emails(std::slice::from_ref(&made.id), &["id", "threadId"]).await.map_err(backend)?;
                got.list.into_iter().next().map(|e| e.thread_id).ok_or(BackendError::NotFound)?
            }
        };
        taken.insert(made.id.clone());
        moved.push(Moved { from: draft.id.clone(), to: made.id, thread });
    }
    Ok(())
}

/// What the person typed for the JMAP sign-in.
pub struct SwitchTo {
    pub session_url: String,
    pub token: bool,
    /// The user name Basic sends.
    pub user: String,
    pub secret: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    /// The JMAP server refused the sign-in or did not answer it.
    #[error(transparent)]
    SignIn(JmapError),
    #[error("only an IMAP account can switch to JMAP")]
    NotImap,
    #[error(transparent)]
    Sync(#[from] SyncError),
}

#[derive(Debug, Clone)]
pub struct Switched {
    pub account: Account,
    pub kept: usize,
    pub dropped: usize,
}

impl From<mailrs_store::StoreError> for SwitchError {
    fn from(err: mailrs_store::StoreError) -> Self {
        SwitchError::Sync(err.into())
    }
}

impl From<PasswordError> for SwitchError {
    fn from(err: PasswordError) -> Self {
        SwitchError::Sync(err.into())
    }
}

impl From<BackendError> for SwitchError {
    fn from(err: BackendError) -> Self {
        SwitchError::Sync(err.into())
    }
}

async fn keyring<T: Send + 'static>(work: impl FnOnce() -> Result<T, PasswordError> + Send + 'static) -> Result<T, PasswordError> {
    tokio::task::spawn_blocking(work).await.map_err(|e| PasswordError::Keyring(e.to_string()))?
}

/// Signs in to the JMAP session with what the person typed, then switches.
pub async fn to_jmap(db: &Db, secrets: &Secrets, account: &Account, to: SwitchTo) -> Result<Switched, SwitchError> {
    if account.provider != Provider::Imap {
        return Err(SwitchError::NotImap);
    }
    let auth = match to.token {
        true => Auth::Bearer(to.secret.clone()),
        false => Auth::Basic { user: to.user.clone(), password: to.secret.clone() },
    };
    let client = JmapClient::new(&to.session_url, auth).map_err(SwitchError::SignIn)?;
    let checked = mailrs_jmap::check(&client).await.map_err(SwitchError::SignIn)?;
    let session = &checked.session;
    let saved = JmapSaved {
        session_url: to.session_url,
        auth: if to.token { JmapAuth::Bearer } else { JmapAuth::Basic },
        user_name: (!to.token && !to.user.eq_ignore_ascii_case(&account.email)).then_some(to.user),
        labels: session.mailboxes_per_email().is_none_or(|most| most > 1),
        capabilities: session.capabilities.keys().filter(|c| session.offers(c)).cloned().collect(),
    };
    to_jmap_over(db, secrets, account, &AnyJmap::from(client), saved, to.secret).await
}

/// The switch over `api`, already signed in. The JMAP secret goes into the
/// keyring just before the store's one transaction and out again when that
/// fails. The IMAP password stays when a CalDAV, CardDAV or ManageSieve
/// server the session does not replace signs in with it.
pub async fn to_jmap_over<J: JmapApi>(
    db: &Db,
    secrets: &Secrets,
    account: &Account,
    api: &J,
    saved: JmapSaved,
    secret: String,
) -> Result<Switched, SwitchError> {
    if account.provider != Provider::Imap {
        return Err(SwitchError::NotImap);
    }
    let id = account.id;
    let session = api.session().await.map_err(SwitchError::SignIn)?;
    if !session.offers(mailrs_jmap::MAIL) {
        return Err(SwitchError::SignIn(JmapError::UnknownCapability(mailrs_jmap::MAIL.to_string())));
    }
    let listed = api.mailboxes().await.map_err(backend)?.list;
    let (stored, boxes, drafts) =
        db.read(move |c| Ok((stored_messages(c, id)?, stored_mailboxes(c, id)?, stored_drafts(c, id)?))).await?;
    let mut messages = match_messages(api, &stored).await?;
    carry_drafts(db, id, api, &listed, &stored, &drafts, &mut messages).await?;
    drop(stored);
    let mailboxes = match_mailboxes(&boxes, &listed);
    let found = db.read(move |c| mailrs_store::services::load(c, id)).await?;
    let contacts_from_jmap = saved.offers(mailrs_jmap::CONTACTS);
    let needs_dav_password = found.iter().filter(|f| f.confirmed).any(|f| match f.kind {
        ServiceKind::CalDav => true,
        ServiceKind::CardDav => !contacts_from_jmap,
        ServiceKind::Sieve => !saved.offers(mailrs_jmap::SIEVE),
    });
    let jmap = Arc::clone(&secrets.jmap);
    keyring(move || jmap.save(id, &secret)).await?;
    let adoption = Adoption { messages, mailboxes, saved, contacts_from_jmap };
    let adopted = db
        .write(move |c| {
            let adopted = adopt_jmap(c, id, &adoption)?;
            // An outbox row keeps its bytes and still sends; it lets go of
            // a message or thread the switch dropped.
            c.execute(
                "UPDATE outbox SET message_id = NULL WHERE account_id = ?1 \
                 AND message_id NOT IN (SELECT id FROM messages WHERE account_id = ?1)",
                params![id],
            )?;
            c.execute(
                "UPDATE outbox SET thread_id = NULL WHERE account_id = ?1 \
                 AND thread_id NOT IN (SELECT thread_id FROM messages WHERE account_id = ?1)",
                params![id],
            )?;
            Ok(adopted)
        })
        .await;
    let adopted = match adopted {
        Ok(adopted) => adopted,
        Err(err) => {
            let jmap = Arc::clone(&secrets.jmap);
            if keyring(move || jmap.delete(id)).await.is_err() {
                tracing::warn!(account = id, "the switch failed, and the keyring kept the JMAP secret");
            }
            return Err(err.into());
        }
    };
    if !needs_dav_password {
        let passwords = Arc::clone(&secrets.passwords);
        keyring(move || {
            passwords.delete_outgoing(id)?;
            passwords.delete(id)
        })
        .await?;
    }
    let account = db.read(move |c| mailrs_store::accounts::account(c, id)).await?.ok_or(SwitchError::NotImap)?;
    Ok(Switched { account, kept: adopted.kept, dropped: adopted.dropped })
}
