//! An in-memory JMAP server for sync's tests and the demo: mailboxes with
//! roles, messages held as raw bytes with their places and marks, a
//! change log per type the way a server keeps states, blobs, identities
//! and submissions. A state older than `forget_changes` cannot be
//! calculated, as a server that kept too little history answers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use mailrs_jmap::{
    BodyPart, CORE, ChangesResponse, Email, EmailAddress, EmailQuery, Got, Header, Hit, Identity, Imported, JmapError, Listed, MAIL,
    MailChanges, Mailbox, MailboxWrite, Pushed, Queried, SUBMISSION, Saved, Session, SessionAccount, SetError, SetResponse,
    Submission, Thread, Threads,
};
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

use crate::services::jmap::JmapApi;

type Answer<T> = Result<T, JmapError>;

const ACCOUNT: &str = "A1";

/// Whether any part below `part` is a file.
fn has_file(part: &mailrs_mime::Part) -> bool {
    part.attachment || part.children.iter().any(has_file)
}

/// `part` as a server describes it in `bodyStructure`.
fn body_part(part: &mailrs_mime::Part, blob: &str) -> BodyPart {
    let leaf = part.children.is_empty();
    BodyPart {
        part_id: leaf.then(|| part.path.clone()),
        blob_id: leaf.then(|| format!("{blob}/{}", part.path)),
        size: u64::try_from(part.size).unwrap_or_default(),
        mime_type: part.mime_type.clone(),
        charset: part.charset.clone(),
        disposition: part.attachment.then(|| "attachment".to_string()),
        cid: part.content_id.clone(),
        name: part.filename.clone(),
        sub_parts: (!leaf).then(|| part.children.iter().map(|c| body_part(c, blob)).collect()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeMailbox {
    pub name: String,
    pub parent_id: Option<String>,
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeEmail {
    pub raw: Vec<u8>,
    pub blob_id: String,
    pub thread_id: String,
    pub mailbox_ids: BTreeSet<String>,
    pub keywords: BTreeSet<String>,
    /// Milliseconds since 1970.
    pub received_at: i64,
}

/// One submission the server took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub identity_id: String,
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Created,
    Updated,
    Destroyed,
}

pub struct JmapState {
    pub session: Session,
    pub mailboxes: BTreeMap<String, FakeMailbox>,
    pub emails: BTreeMap<String, FakeEmail>,
    pub blobs: HashMap<String, Vec<u8>>,
    pub identities: Vec<Identity>,
    pub sent: Vec<Sent>,
    /// Every method the adapter called, in order, for tests that count.
    pub calls: Vec<String>,
    /// Blobs that answer as larger than any cap, without holding the bytes.
    pub oversized: BTreeSet<String>,
    /// The most hits one `Email/query` answers, whatever limit it was given:
    /// RFC 8620 5.5 lets a server return fewer than asked.
    pub query_clamp: Option<usize>,
    /// A mailbox id and the message count it claims, for a mailbox too big
    /// to fill in a test.
    pub total_override: Option<(String, u64)>,
    log: Vec<(u64, &'static str, String, Op)>,
    state: u64,
    forgotten: u64,
    next: u64,
    push: bool,
}

impl JmapState {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    fn record(&mut self, kind: &'static str, id: &str, op: Op) {
        self.state += 1;
        self.log.push((self.state, kind, id.to_string(), op));
    }

    /// The id of the mailbox with `role`.
    pub fn role_id(&self, role: &str) -> Option<String> {
        self.mailboxes.iter().find(|(_, m)| m.role.as_deref() == Some(role)).map(|(id, _)| id.clone())
    }

    /// The most mailboxes one message may sit in, when the session says.
    fn mailbox_limit(&self) -> Option<u64> {
        let account = self.session.accounts.values().next()?;
        account.account_capabilities.get(MAIL)?.get("maxMailboxesPerEmail")?.as_u64()
    }

    /// Why a message may not sit in `places`, as a server refuses it.
    fn check_places(&self, places: &BTreeSet<String>) -> Result<(), SetError> {
        let invalid = |why: &str| SetError { kind: "invalidProperties".into(), description: Some(why.into()) };
        if places.is_empty() {
            return Err(invalid("an Email must be in a mailbox"));
        }
        if places.iter().any(|m| !self.mailboxes.contains_key(m)) {
            return Err(invalid("a mailbox does not exist"));
        }
        if self.mailbox_limit().is_some_and(|limit| places.len() as u64 > limit) {
            return Err(invalid("an Email is in too many mailboxes"));
        }
        Ok(())
    }

    /// Records a mailbox change for every mailbox whose counts a message's
    /// change moves: where it came to or left, and all of its mailboxes when
    /// it appears, goes, or is read or unread.
    fn settle(&mut self, before: Option<&FakeEmail>, after: Option<&FakeEmail>) {
        let seen = |e: Option<&FakeEmail>| e.is_some_and(|e| e.keywords.contains("$seen"));
        let places = |e: Option<&FakeEmail>| e.map(|e| e.mailbox_ids.clone()).unwrap_or_default();
        let (was, now) = (places(before), places(after));
        let touched: BTreeSet<String> = if before.is_none() || after.is_none() || seen(before) != seen(after) {
            was.union(&now).cloned().collect()
        } else {
            was.symmetric_difference(&now).cloned().collect()
        };
        let touched: Vec<String> = touched.into_iter().filter(|m| self.mailboxes.contains_key(m)).collect();
        for mailbox in touched {
            self.record("Mailbox", &mailbox, Op::Updated);
        }
    }

    /// A blob by id: a whole message, or one part of one as `blob/path`.
    fn blob(&self, id: &str) -> Option<Vec<u8>> {
        if let Some(whole) = self.blobs.get(id) {
            return Some(whole.clone());
        }
        let (blob, path) = id.split_once('/')?;
        let parts = mailrs_mime::parts(self.blobs.get(blob)?)?;
        parts.find(path)?.data.clone()
    }

    /// The Email as `Email/get` answers it. The structure and headers
    /// come only when `bodies` asks for them, as a server sends only the
    /// properties requested. Each leaf's blob id is the message's blob id,
    /// a slash and the part's section number.
    fn email(&self, id: &str, bodies: bool) -> Option<Email> {
        let held = self.emails.get(id)?;
        let parts = mailrs_mime::parts(&held.raw).unwrap_or_default();
        let addresses = |name: &str| {
            parts.header(name).map(|v| {
                mailrs_mime::address::parse_address_list(v)
                    .into_iter()
                    .map(|a| EmailAddress { name: a.name, email: a.email })
                    .collect::<Vec<_>>()
            })
        };
        let received = chrono::DateTime::from_timestamp_millis(held.received_at).unwrap_or_default();
        Some(Email {
            id: id.to_string(),
            blob_id: held.blob_id.clone(),
            thread_id: held.thread_id.clone(),
            mailbox_ids: held.mailbox_ids.iter().map(|m| (m.clone(), true)).collect(),
            keywords: held.keywords.iter().map(|k| (k.clone(), true)).collect(),
            size: held.raw.len() as u64,
            received_at: Some(received.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            message_id: parts.header("Message-ID").map(|m| vec![m.trim().trim_matches(['<', '>']).to_string()]),
            from: addresses("From"),
            to: addresses("To"),
            cc: addresses("Cc"),
            subject: parts.header("Subject").map(str::to_string),
            preview: Some(String::from_utf8_lossy(&held.raw).lines().last().unwrap_or_default().trim().to_string()),
            has_attachment: has_file(&parts.root),
            list_unsubscribe: parts.header("List-Unsubscribe").map(str::to_string),
            list_unsubscribe_post: parts.header("List-Unsubscribe-Post").map(str::to_string),
            headers: bodies.then(|| parts.headers.iter().map(|(name, value)| Header { name: name.clone(), value: value.clone() }).collect()),
            body_structure: bodies.then(|| body_part(&parts.root, &held.blob_id)),
        })
    }

    fn changes(&self, kind: &'static str, since: &str, max: usize) -> Answer<ChangesResponse> {
        let since: u64 = since.parse().map_err(|_| JmapError::CannotCalculateChanges)?;
        if since < self.forgotten || since > self.state {
            return Err(JmapError::CannotCalculateChanges);
        }
        let mut seen: BTreeMap<String, Op> = BTreeMap::new();
        let mut new_state = since;
        let mut more = false;
        for (at, _, id, op) in self.log.iter().filter(|(at, k, _, _)| *at > since && *k == kind) {
            if seen.len() >= max && !seen.contains_key(id) {
                more = true;
                break;
            }
            new_state = *at;
            let merged = match (seen.get(id), op) {
                (Some(Op::Created), Op::Updated) => Op::Created,
                (_, op) => *op,
            };
            seen.insert(id.clone(), merged);
        }
        if !more {
            new_state = self.state.max(since);
        }
        let pick = |want: Op| seen.iter().filter(|(_, op)| **op == want).map(|(id, _)| id.clone()).collect();
        Ok(ChangesResponse {
            old_state: since.to_string(),
            new_state: new_state.to_string(),
            has_more_changes: more,
            created: pick(Op::Created),
            updated: pick(Op::Updated),
            destroyed: pick(Op::Destroyed),
            updated_properties: None,
        })
    }

    fn matches(&self, email: &FakeEmail, filter: &Value) -> bool {
        if let Some(op) = filter.get("operator").and_then(Value::as_str) {
            let conditions: Vec<&Value> = filter["conditions"].as_array().map(|c| c.iter().collect()).unwrap_or_default();
            return match op {
                "AND" => conditions.iter().all(|c| self.matches(email, c)),
                "OR" => conditions.iter().any(|c| self.matches(email, c)),
                _ => !conditions.iter().any(|c| self.matches(email, c)),
            };
        }
        let text = String::from_utf8_lossy(&email.raw).to_lowercase();
        let parts = mailrs_mime::parts(&email.raw).unwrap_or_default();
        let header_has = |name: &str, needle: &Value| {
            needle.as_str().is_none_or(|n| parts.header(name).is_some_and(|v| v.to_lowercase().contains(&n.to_lowercase())))
        };
        let millis = |v: &Value| v.as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp_millis());
        filter.as_object().is_none_or(|conditions| {
            conditions.iter().all(|(key, value)| match key.as_str() {
                "inMailbox" => value.as_str().is_some_and(|m| email.mailbox_ids.contains(m)),
                "inMailboxOtherThan" => value.as_array().is_some_and(|ms| email.mailbox_ids.iter().any(|m| !ms.iter().any(|x| x == m))),
                "after" => millis(value).is_some_and(|at| email.received_at >= at),
                "before" => millis(value).is_some_and(|at| email.received_at < at),
                "hasKeyword" => value.as_str().is_some_and(|k| email.keywords.contains(k)),
                "notKeyword" => value.as_str().is_some_and(|k| !email.keywords.contains(k)),
                "from" => header_has("From", value),
                "to" => header_has("To", value),
                "subject" => header_has("Subject", value),
                "text" | "body" => value.as_str().is_some_and(|t| text.contains(&t.to_lowercase())),
                "hasAttachment" => value.as_bool() == Some(false),
                "minSize" => value.as_u64().is_some_and(|n| email.raw.len() as u64 >= n),
                "header" => {
                    let pair: Vec<&str> = value.as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    match pair.as_slice() {
                        [name] => parts.header(name).is_some(),
                        [name, wanted] => parts.header(name).is_some_and(|v| v.trim() == wanted.trim()),
                        _ => false,
                    }
                }
                _ => true,
            })
        })
    }
}

pub struct FakeJmap {
    state: Mutex<JmapState>,
    pushed: Notify,
}

fn session(with: &[&str], mailboxes_per_email: Option<u64>) -> Session {
    let capabilities: BTreeMap<String, Value> = std::iter::once((CORE.to_string(), json!({
        "maxCallsInRequest": 16, "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxSizeRequest": 10_000_000,
        "maxConcurrentRequests": 4, "maxSizeUpload": 50_000_000, "maxConcurrentUpload": 2
    })))
    .chain(with.iter().map(|c| (c.to_string(), json!({}))))
    .collect();
    let mut account_capabilities: BTreeMap<String, Value> = with.iter().map(|c| (c.to_string(), json!({}))).collect();
    account_capabilities.insert(MAIL.into(), json!({ "maxMailboxesPerEmail": mailboxes_per_email }));
    Session {
        capabilities,
        accounts: BTreeMap::from([(
            ACCOUNT.to_string(),
            SessionAccount { name: "me@example.com".into(), is_personal: true, is_read_only: false, account_capabilities },
        )]),
        primary_accounts: with.iter().map(|c| (c.to_string(), ACCOUNT.to_string())).collect(),
        username: "me@example.com".into(),
        api_url: "https://jmap.example.com/api/".into(),
        download_url: "https://jmap.example.com/download/{accountId}/{blobId}/{name}?type={type}".into(),
        upload_url: "https://jmap.example.com/upload/{accountId}/".into(),
        event_source_url: Some("https://jmap.example.com/events/".into()),
        state: "s1".into(),
    }
}

impl Default for FakeJmap {
    fn default() -> Self {
        FakeJmap::new()
    }
}

impl FakeJmap {
    pub fn new() -> FakeJmap {
        let mut state = JmapState {
            session: session(&[MAIL, SUBMISSION], Some(1000)),
            mailboxes: BTreeMap::new(),
            emails: BTreeMap::new(),
            blobs: HashMap::new(),
            identities: vec![Identity {
                id: "I1".into(),
                name: "Me".into(),
                email: "me@example.com".into(),
                reply_to: None,
                bcc: None,
                text_signature: String::new(),
                html_signature: String::new(),
                may_delete: false,
            }],
            sent: Vec::new(),
            calls: Vec::new(),
            oversized: BTreeSet::new(),
            query_clamp: None,
            total_override: None,
            log: Vec::new(),
            state: 0,
            forgotten: 0,
            next: 0,
            push: true,
        };
        for (name, role) in [("Inbox", "inbox"), ("Drafts", "drafts"), ("Sent", "sent"), ("Trash", "trash"), ("Junk Mail", "junk"), ("Archive", "archive")] {
            let id = format!("mb-{role}");
            state.mailboxes.insert(id, FakeMailbox { name: name.into(), parent_id: None, role: Some(role.into()) });
        }
        FakeJmap { state: Mutex::new(state), pushed: Notify::new() }
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut JmapState) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn called(&self, name: &str) {
        self.with(|s| s.calls.push(name.to_string()));
    }

    /// A server that answers at most `most` hits to a query.
    pub fn clamping_queries(self, most: usize) -> FakeJmap {
        self.with(|s| s.query_clamp = Some(most));
        self
    }

    pub fn without_push(self) -> FakeJmap {
        self.with(|s| {
            s.push = false;
            s.session.event_source_url = None;
        });
        self
    }

    pub fn with_capability(self, capability: &str) -> FakeJmap {
        self.with(|s| {
            s.session.capabilities.insert(capability.into(), json!({}));
            s.session.primary_accounts.insert(capability.into(), ACCOUNT.into());
            if let Some(account) = s.session.accounts.get_mut(ACCOUNT) {
                account.account_capabilities.insert(capability.into(), json!({}));
            }
        });
        self
    }

    pub fn without_capability(self, capability: &str) -> FakeJmap {
        self.with(|s| {
            s.session.capabilities.remove(capability);
            s.session.primary_accounts.remove(capability);
        });
        self
    }

    pub fn mailboxes_per_email(self, limit: Option<u64>) -> FakeJmap {
        self.with(|s| {
            if let Some(account) = s.session.accounts.get_mut(ACCOUNT) {
                account.account_capabilities.insert(MAIL.into(), json!({ "maxMailboxesPerEmail": limit }));
            }
        });
        self
    }

    pub fn mailbox(&self, role: &str) -> String {
        self.with(|s| s.role_id(role)).unwrap_or_else(|| panic!("the fake has no {role} mailbox"))
    }

    pub fn add_mailbox(&self, name: &str, parent: Option<&str>) -> String {
        self.with(|s| {
            let id = s.id("mb-");
            s.mailboxes.insert(id.clone(), FakeMailbox { name: name.into(), parent_id: parent.map(str::to_string), role: None });
            s.record("Mailbox", &id, Op::Created);
            id
        })
    }

    /// Puts `raw` in the mailbox with `role`, received at `received_at`
    /// milliseconds, threaded with the stored message its `In-Reply-To`
    /// names.
    pub fn deliver(&self, role: &str, raw: &[u8], received_at: i64) -> String {
        self.with(|s| {
            let mailbox = s.role_id(role).expect("the role's mailbox");
            let (id, blob) = (s.id("M"), s.id("G"));
            let parts = mailrs_mime::parts(raw).unwrap_or_default();
            let parent = parts.header("In-Reply-To").map(|v| v.trim().to_string());
            let thread = parent
                .and_then(|wanted| {
                    s.emails.iter().find_map(|(_, e)| {
                        let p = mailrs_mime::parts(&e.raw).unwrap_or_default();
                        (p.header("Message-ID").map(str::trim) == Some(wanted.as_str())).then(|| e.thread_id.clone())
                    })
                })
                .unwrap_or_else(|| format!("T{id}"));
            s.blobs.insert(blob.clone(), raw.to_vec());
            s.emails.insert(id.clone(), FakeEmail {
                raw: raw.to_vec(),
                blob_id: blob,
                thread_id: thread,
                mailbox_ids: BTreeSet::from([mailbox.clone()]),
                keywords: BTreeSet::new(),
                received_at,
            });
            s.record("Email", &id, Op::Created);
            let made = s.emails.get(&id).cloned();
            s.settle(None, made.as_ref());
            id
        })
    }

    pub fn set_keyword(&self, id: &str, keyword: &str, on: bool) {
        self.with(|s| {
            let email = s.emails.get_mut(id).expect("the message");
            let before = email.clone();
            match on {
                true => email.keywords.insert(keyword.to_string()),
                false => email.keywords.remove(keyword),
            };
            let after = email.clone();
            s.record("Email", id, Op::Updated);
            s.settle(Some(&before), Some(&after));
        });
    }

    pub fn move_to(&self, id: &str, mailbox: &str) {
        self.with(|s| {
            let email = s.emails.get_mut(id).expect("the message");
            let before = email.clone();
            email.mailbox_ids = BTreeSet::from([mailbox.to_string()]);
            let after = email.clone();
            s.record("Email", id, Op::Updated);
            s.settle(Some(&before), Some(&after));
        });
    }

    pub fn destroy(&self, id: &str) {
        self.with(|s| {
            let gone = s.emails.remove(id);
            s.record("Email", id, Op::Destroyed);
            s.settle(gone.as_ref(), None);
        });
    }

    /// Forgets every change before now, so an older state cannot be read
    /// while the current one reads as empty.
    pub fn forget_changes(&self) {
        self.with(|s| s.forgotten = s.state);
    }

    /// Tells a waiting push that something changed.
    pub fn poke(&self) {
        self.pushed.notify_one();
    }

    fn import_now(s: &mut JmapState, blob_id: &str, mailboxes: &[String], keywords: &[&str]) -> Answer<Imported> {
        let raw = s.blobs.get(blob_id).cloned().ok_or(JmapError::NotFound)?;
        let places: BTreeSet<String> = mailboxes.iter().cloned().collect();
        s.check_places(&places).map_err(|refused| refused.error())?;
        let id = s.id("M");
        let thread = format!("T{id}");
        s.emails.insert(id.clone(), FakeEmail {
            raw,
            blob_id: blob_id.to_string(),
            thread_id: thread.clone(),
            mailbox_ids: mailboxes.iter().cloned().collect(),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            received_at: chrono::Utc::now().timestamp_millis(),
        });
        s.record("Email", &id, Op::Created);
        let made = s.emails.get(&id).cloned();
        s.settle(None, made.as_ref());
        Ok(Imported { id, thread_id: thread })
    }
}

impl JmapApi for FakeJmap {
    async fn session(&self) -> Answer<Arc<Session>> {
        Ok(Arc::new(self.with(|s| s.session.clone())))
    }

    async fn mailboxes(&self) -> Answer<Listed<Mailbox>> {
        self.called("Mailbox/get");
        Ok(self.with(|s| {
            let list = s
                .mailboxes
                .iter()
                .map(|(id, m)| {
                    let inside: Vec<&FakeEmail> = s.emails.values().filter(|e| e.mailbox_ids.contains(id)).collect();
                    let unread = inside.iter().filter(|e| !e.keywords.contains("$seen")).count() as u64;
                    Mailbox {
                        id: id.clone(),
                        name: m.name.clone(),
                        parent_id: m.parent_id.clone(),
                        role: m.role.clone(),
                        total_emails: s.total_override.as_ref().filter(|(m, _)| m == id).map_or(inside.len() as u64, |(_, n)| *n),
                        unread_emails: unread,
                        total_threads: inside.iter().map(|e| &e.thread_id).collect::<BTreeSet<_>>().len() as u64,
                        unread_threads: unread,
                        is_subscribed: true,
                        ..Mailbox::default()
                    }
                })
                .collect();
            Listed { state: s.state.to_string(), list }
        }))
    }

    async fn mail_changes(&self, email: &str, mailbox: &str, max: usize) -> Answer<MailChanges> {
        self.called("Email/changes");
        self.with(|s| {
            let changes = s.changes("Email", email, max)?;
            let changed = changes.created.iter().chain(&changes.updated).filter_map(|id| s.email(id, false)).collect();
            let mailbox = s.changes("Mailbox", mailbox, 10_000)?;
            Ok(MailChanges { email: changes, mailbox, changed })
        })
    }

    async fn email_state(&self) -> Answer<String> {
        Ok(self.with(|s| s.state.to_string()))
    }

    async fn query(&self, query: &EmailQuery) -> Answer<Queried> {
        self.called("Email/query");
        Ok(self.with(|s| {
            let mut found: Vec<(&String, &FakeEmail)> = s
                .emails
                .iter()
                .filter(|(_, e)| query.filter.as_ref().is_none_or(|f| s.matches(e, f)))
                .collect();
            found.sort_by(|a, b| b.1.received_at.cmp(&a.1.received_at).then(b.0.cmp(a.0)));
            let total = found.len() as u64;
            let hits = found
                .into_iter()
                .skip(query.position as usize)
                .take(s.query_clamp.map_or(query.limit, |clamp| clamp.min(query.limit)))
                .map(|(id, e)| Hit { id: id.clone(), thread_id: e.thread_id.clone() })
                .collect();
            Queried { hits, position: query.position, total: query.total.then_some(total) }
        }))
    }

    async fn emails(&self, ids: &[String], properties: &[&str]) -> Answer<Got<Email>> {
        self.called("Email/get");
        Ok(self.with(|s| {
            let (mut list, mut not_found) = (Vec::new(), Vec::new());
            for id in ids {
                match s.email(id, properties.contains(&"bodyStructure")) {
                    Some(email) => list.push(email),
                    None => not_found.push(id.clone()),
                }
            }
            Got { state: s.state.to_string(), list, not_found }
        }))
    }

    async fn threads(&self, ids: &[String]) -> Answer<Threads> {
        self.called("Thread/get");
        Ok(self.with(|s| {
            let mut out = Threads::default();
            for thread in ids {
                let mut members: Vec<(&String, &FakeEmail)> = s.emails.iter().filter(|(_, e)| &e.thread_id == thread).collect();
                if members.is_empty() {
                    out.not_found.push(thread.clone());
                    continue;
                }
                members.sort_by_key(|(_, e)| e.received_at);
                out.threads.push(Thread { id: thread.clone(), email_ids: members.iter().map(|(id, _)| (*id).clone()).collect() });
                out.emails.extend(members.iter().filter_map(|(id, _)| s.email(id, false)));
            }
            out
        }))
    }

    async fn set_emails(&self, update: &Map<String, Value>, destroy: &[String]) -> Answer<SetResponse> {
        self.called("Email/set");
        Ok(self.with(|s| {
            let mut answer = SetResponse::default();
            let (mut updated, mut not_updated) = (BTreeMap::new(), BTreeMap::new());
            for (id, patch) in update {
                let Some(email) = s.emails.get(id).cloned() else {
                    not_updated.insert(id.clone(), SetError { kind: "notFound".into(), description: None });
                    continue;
                };
                let mut next = email.clone();
                let invalid = |why: &str| SetError { kind: "invalidProperties".into(), description: Some(why.into()) };
                let mut refusal = None;
                for (path, value) in patch.as_object().into_iter().flatten() {
                    let unescaped = |name: &str| name.replace("~1", "/").replace("~0", "~");
                    let flag = |v: &Value| match v {
                        Value::Bool(true) => Some(true),
                        Value::Null => Some(false),
                        _ => None,
                    };
                    if path == "mailboxIds" {
                        match value.as_object().filter(|m| m.values().all(|v| v == &Value::Bool(true))) {
                            Some(m) => next.mailbox_ids = m.keys().cloned().collect(),
                            None => refusal = Some(invalid("mailboxIds must map ids to true")),
                        }
                    } else if let Some(mailbox) = path.strip_prefix("mailboxIds/") {
                        match flag(value) {
                            Some(true) => drop(next.mailbox_ids.insert(unescaped(mailbox))),
                            Some(false) => drop(next.mailbox_ids.remove(&unescaped(mailbox))),
                            None => refusal = Some(invalid("a mailbox is set to true or removed with null")),
                        }
                    } else if let Some(keyword) = path.strip_prefix("keywords/") {
                        match flag(value) {
                            Some(true) => drop(next.keywords.insert(unescaped(keyword))),
                            Some(false) => drop(next.keywords.remove(&unescaped(keyword))),
                            None => refusal = Some(invalid("a keyword is set to true or removed with null")),
                        }
                    } else {
                        refusal = Some(invalid("an unknown property"));
                    }
                }
                if let Some(refused) = refusal.or_else(|| s.check_places(&next.mailbox_ids).err()) {
                    not_updated.insert(id.clone(), refused);
                    continue;
                }
                s.emails.insert(id.clone(), next.clone());
                s.record("Email", id, Op::Updated);
                s.settle(Some(&email), Some(&next));
                updated.insert(id.clone(), None);
            }
            let (mut destroyed, mut not_destroyed) = (Vec::new(), BTreeMap::new());
            for id in destroy {
                match s.emails.remove(id) {
                    Some(gone) => {
                        s.record("Email", id, Op::Destroyed);
                        s.settle(Some(&gone), None);
                        destroyed.push(id.clone());
                    }
                    None => {
                        not_destroyed.insert(id.clone(), SetError { kind: "notFound".into(), description: None });
                    }
                }
            }
            answer.not_destroyed = (!not_destroyed.is_empty()).then_some(not_destroyed);
            answer.updated = Some(updated);
            answer.not_updated = (!not_updated.is_empty()).then_some(not_updated);
            answer.destroyed = Some(destroyed);
            answer.new_state = Some(s.state.to_string());
            answer
        }))
    }

    async fn set_mailbox(&self, write: &MailboxWrite) -> Answer<String> {
        self.called("Mailbox/set");
        self.with(|s| match write {
            MailboxWrite::Create { name, parent_id } => {
                let id = s.id("mb-");
                s.mailboxes.insert(id.clone(), FakeMailbox { name: name.clone(), parent_id: parent_id.clone(), role: None });
                s.record("Mailbox", &id, Op::Created);
                Ok(id)
            }
            MailboxWrite::Update { id, name, parent_id } => {
                let mailbox = s.mailboxes.get_mut(id).ok_or(JmapError::NotFound)?;
                mailbox.name = name.clone();
                mailbox.parent_id = parent_id.clone();
                s.record("Mailbox", id, Op::Updated);
                Ok(id.clone())
            }
            MailboxWrite::Destroy { id } => {
                s.mailboxes.remove(id).ok_or(JmapError::NotFound)?;
                s.record("Mailbox", id, Op::Destroyed);
                Ok(id.clone())
            }
        })
    }

    async fn download(&self, blob_id: &str, limit: usize) -> Answer<Vec<u8>> {
        self.called("download");
        if self.with(|s| s.oversized.contains(blob_id)) {
            return Err(JmapError::AnswerTooLarge { limit });
        }
        let bytes = self.with(|s| s.blob(blob_id)).ok_or(JmapError::NotFound)?;
        match bytes.len() > limit {
            true => Err(JmapError::AnswerTooLarge { limit }),
            false => Ok(bytes),
        }
    }

    async fn upload(&self, bytes: Vec<u8>) -> Answer<String> {
        self.called("upload");
        Ok(self.with(|s| {
            let id = s.id("G");
            s.blobs.insert(id.clone(), bytes);
            id
        }))
    }

    async fn import(&self, blob_id: &str, mailbox_ids: &[String], keywords: &[&str], destroy: Option<&str>) -> Answer<Saved> {
        self.called("Email/import");
        self.with(|s| {
            let made = FakeJmap::import_now(s, blob_id, mailbox_ids, keywords)?;
            if let Some(old) = destroy
                && let Some(gone) = s.emails.remove(old)
            {
                s.record("Email", old, Op::Destroyed);
                s.settle(Some(&gone), None);
            }
            Ok(Saved { email: made, leftover: None })
        })
    }

    async fn submit(&self, submission: &Submission) -> Answer<Imported> {
        self.called("EmailSubmission/set");
        self.with(|s| {
            if !s.identities.iter().any(|i| i.id == submission.identity_id) {
                return Err(JmapError::Method { kind: "forbiddenFrom".into(), description: String::new() });
            }
            let home = submission.drafts.clone().or(submission.sent.clone()).ok_or(JmapError::NotFound)?;
            let made = FakeJmap::import_now(s, &submission.blob_id, &[home], &["$seen", "$draft"])?;
            let raw = s.blobs.get(&submission.blob_id).cloned().unwrap_or_default();
            s.sent.push(Sent {
                identity_id: submission.identity_id.clone(),
                mail_from: submission.mail_from.clone(),
                rcpt_to: submission.rcpt_to.clone(),
                raw,
            });
            if let Some(email) = s.emails.get_mut(&made.id) {
                let before = email.clone();
                email.keywords.remove("$draft");
                if let Some(sent) = &submission.sent {
                    email.mailbox_ids = BTreeSet::from([sent.clone()]);
                }
                let after = email.clone();
                s.settle(Some(&before), Some(&after));
            }
            s.record("Email", &made.id, Op::Updated);
            Ok(made)
        })
    }

    async fn identities(&self) -> Answer<Vec<Identity>> {
        Ok(self.with(|s| s.identities.clone()))
    }

    async fn push(&self) -> Answer<Pushed> {
        if !self.with(|s| s.push) {
            return Ok(Pushed::NoPush);
        }
        self.pushed.notified().await;
        Ok(Pushed::Changed)
    }
}

#[cfg(test)]
mod tests {
    use mailrs_jmap::{EmailQuery, JmapError, MailboxWrite, PLACES, Pushed};
    use serde_json::{Map, json};

    use super::*;
    use crate::services::jmap::JmapApi;

    const RAW: &[u8] = b"From: Ada <ada@example.com>\r\nTo: me@example.com\r\nSubject: Hi\r\nMessage-ID: <a1@example.com>\r\n\r\nHello\r\n";

    #[tokio::test]
    async fn delivered_mail_shows_in_the_changes_since_the_last_state() {
        let fake = FakeJmap::new();
        let (email, mailbox) = (fake.email_state().await.unwrap(), fake.mailboxes().await.unwrap().state);
        let id = fake.deliver("inbox", RAW, 1_000);
        let changes = fake.mail_changes(&email, &mailbox, 100).await.unwrap();
        assert_eq!(changes.email.created, std::slice::from_ref(&id));
        assert_eq!(changes.changed[0].mailbox_ids.keys().next(), Some(&fake.mailbox("inbox")));
        let again = fake.mail_changes(&changes.email.new_state, &mailbox, 100).await.unwrap();
        assert!(again.email.created.is_empty() && again.changed.is_empty());
    }

    #[tokio::test]
    async fn a_forgotten_state_cannot_be_calculated() {
        let fake = FakeJmap::new();
        let email = fake.email_state().await.unwrap();
        fake.deliver("inbox", RAW, 1_000);
        fake.forget_changes();
        assert_eq!(fake.mail_changes(&email, "0", 100).await.unwrap_err(), JmapError::CannotCalculateChanges);
    }

    #[tokio::test]
    async fn a_patch_that_empties_mailbox_ids_is_refused_like_a_server_would() {
        let fake = FakeJmap::new();
        let id = fake.deliver("inbox", RAW, 1_000);
        let mut update = Map::new();
        update.insert(id.clone(), json!({ format!("mailboxIds/{}", fake.mailbox("inbox")): null }));
        let set = fake.set_emails(&update, &[]).await.unwrap();
        assert_eq!(set.refusal(&id).unwrap().kind, "invalidProperties");
    }

    #[tokio::test]
    async fn a_query_filters_and_sorts_newest_first() {
        let fake = FakeJmap::new();
        let old = fake.deliver("inbox", RAW, 1_000);
        let new = fake.deliver("archive", RAW, 2_000);
        let all = fake.query(&EmailQuery { filter: None, position: 0, limit: 10, total: true }).await.unwrap();
        assert_eq!(all.hits.iter().map(|h| h.id.clone()).collect::<Vec<_>>(), [new, old.clone()]);
        let inbox = json!({ "inMailbox": fake.mailbox("inbox") });
        let only = fake.query(&EmailQuery { filter: Some(inbox), position: 0, limit: 10, total: false }).await.unwrap();
        assert_eq!(only.hits.len(), 1);
        let got = fake.emails(&[old], PLACES).await.unwrap();
        assert!(got.list[0].mailbox_ids.contains_key(&fake.mailbox("inbox")));
    }

    #[tokio::test]
    async fn a_submission_files_the_copy_in_sent_and_keeps_the_envelope() {
        let fake = FakeJmap::new();
        let blob = fake.upload(RAW.to_vec()).await.unwrap();
        let identity = fake.identities().await.unwrap()[0].id.clone();
        let submission = mailrs_jmap::Submission {
            blob_id: blob,
            identity_id: identity,
            mail_from: "me@example.com".into(),
            rcpt_to: vec!["ada@example.com".into()],
            drafts: Some(fake.mailbox("drafts")),
            sent: Some(fake.mailbox("sent")),
        };
        let sent = fake.submit(&submission).await.unwrap();
        fake.with(|s| {
            let email = &s.emails[&sent.id];
            assert!(email.mailbox_ids.contains(&s.role_id("sent").unwrap()));
            assert!(!email.keywords.contains("$draft"));
            assert_eq!(s.sent[0].rcpt_to, ["ada@example.com"]);
        });
    }

    #[tokio::test]
    async fn a_poke_wakes_push_and_no_push_says_so() {
        let fake = std::sync::Arc::new(FakeJmap::new());
        let waiting = tokio::spawn({
            let fake = std::sync::Arc::clone(&fake);
            async move { fake.push().await }
        });
        tokio::task::yield_now().await;
        fake.poke();
        assert_eq!(waiting.await.unwrap().unwrap(), Pushed::Changed);
        assert_eq!(FakeJmap::new().without_push().push().await.unwrap(), Pushed::NoPush);
    }

    #[tokio::test]
    async fn a_mailbox_made_under_a_parent_lists_with_it() {
        let fake = FakeJmap::new();
        let parent = fake.add_mailbox("Trips", None);
        let id = fake
            .set_mailbox(&MailboxWrite::Create { name: "2026".into(), parent_id: Some(parent.clone()) })
            .await
            .unwrap();
        let listed = fake.mailboxes().await.unwrap().list;
        assert!(listed.iter().any(|m| m.id == id && m.parent_id.as_deref() == Some(parent.as_str())));
    }

    async fn set(fake: &FakeJmap, id: &str, patch: Value) -> SetResponse {
        let mut update = Map::new();
        update.insert(id.to_string(), patch);
        fake.set_emails(&update, &[]).await.unwrap()
    }

    #[tokio::test]
    async fn after_forgetting_the_current_state_reads_as_empty() {
        let fake = FakeJmap::new();
        fake.deliver("inbox", RAW, 1_000);
        fake.forget_changes();
        let (email, mailbox) = (fake.email_state().await.unwrap(), fake.mailboxes().await.unwrap().state);
        let changes = fake.mail_changes(&email, &mailbox, 100).await.unwrap();
        assert!(changes.email.created.is_empty() && changes.changed.is_empty());
    }

    #[tokio::test]
    async fn a_state_from_the_future_cannot_be_calculated() {
        let fake = FakeJmap::new();
        assert_eq!(fake.mail_changes("99", "0", 100).await.unwrap_err(), JmapError::CannotCalculateChanges);
    }

    #[tokio::test]
    async fn every_write_that_moves_a_count_moves_the_mailbox_state() {
        let fake = FakeJmap::new();
        let id = fake.deliver("inbox", RAW, 1_000);
        let mut since = fake.mailboxes().await.unwrap().state;
        let step = |fake: &FakeJmap, since: &mut String| {
            let changes = fake.with(|s| s.changes("Mailbox", since, 100)).unwrap();
            *since = changes.new_state.clone();
            changes.updated
        };
        let inbox = fake.mailbox("inbox");
        fake.set_keyword(&id, "$seen", true);
        assert_eq!(step(&fake, &mut since), std::slice::from_ref(&inbox));
        fake.move_to(&id, &fake.mailbox("archive"));
        let mut both = step(&fake, &mut since);
        both.sort();
        let mut expected = [inbox.clone(), fake.mailbox("archive")];
        expected.sort();
        assert_eq!(both, expected);
        set(&fake, &id, json!({ "keywords/$seen": null })).await;
        assert_eq!(step(&fake, &mut since), [fake.mailbox("archive")]);
        let blob = fake.upload(RAW.to_vec()).await.unwrap();
        let made = fake.import(&blob, std::slice::from_ref(&inbox), &[], None).await.unwrap().email;
        assert_eq!(step(&fake, &mut since), std::slice::from_ref(&inbox));
        fake.destroy(&made.id);
        assert_eq!(step(&fake, &mut since), std::slice::from_ref(&inbox));
    }

    #[tokio::test]
    async fn a_patch_with_an_unknown_path_is_refused() {
        let fake = FakeJmap::new();
        let id = fake.deliver("inbox", RAW, 1_000);
        let set = set(&fake, &id, json!({ "subject": "New" })).await;
        assert_eq!(set.refusal(&id).unwrap().kind, "invalidProperties");
    }

    #[tokio::test]
    async fn a_keyword_set_to_anything_but_true_or_null_is_refused() {
        let fake = FakeJmap::new();
        let id = fake.deliver("inbox", RAW, 1_000);
        let set = set(&fake, &id, json!({ "keywords/$seen": false })).await;
        assert_eq!(set.refusal(&id).unwrap().kind, "invalidProperties");
    }

    #[tokio::test]
    async fn a_patch_into_a_mailbox_that_does_not_exist_is_refused() {
        let fake = FakeJmap::new();
        let id = fake.deliver("inbox", RAW, 1_000);
        let set = set(&fake, &id, json!({ "mailboxIds/nowhere": true })).await;
        assert_eq!(set.refusal(&id).unwrap().kind, "invalidProperties");
    }

    #[tokio::test]
    async fn a_patch_past_the_mailbox_limit_is_refused() {
        let fake = FakeJmap::new().mailboxes_per_email(Some(1));
        let id = fake.deliver("inbox", RAW, 1_000);
        let archive = fake.mailbox("archive");
        let set = set(&fake, &id, json!({ format!("mailboxIds/{archive}"): true })).await;
        assert_eq!(set.refusal(&id).unwrap().kind, "invalidProperties");
    }

    #[tokio::test]
    async fn an_import_into_no_mailbox_or_a_missing_one_is_refused() {
        let fake = FakeJmap::new();
        let blob = fake.upload(RAW.to_vec()).await.unwrap();
        let kind = |e: JmapError| match e {
            JmapError::Method { kind, .. } => kind,
            other => panic!("{other:?}"),
        };
        assert_eq!(kind(fake.import(&blob, &[], &[], None).await.unwrap_err()), "invalidProperties");
        assert_eq!(kind(fake.import(&blob, &["nowhere".into()], &[], None).await.unwrap_err()), "invalidProperties");
        assert!(fake.with(|s| s.emails.is_empty()));
    }

    #[tokio::test]
    async fn destroying_an_unknown_id_answers_not_found() {
        let fake = FakeJmap::new();
        let set = fake.set_emails(&Map::new(), &["gone".into()]).await.unwrap();
        assert_eq!(set.refusal("gone").unwrap().kind, "notFound");
    }

    #[tokio::test]
    async fn changes_page_by_max_and_say_there_is_more() {
        let fake = FakeJmap::new();
        let since = fake.email_state().await.unwrap();
        let ids: Vec<String> = (0..3).map(|n| fake.deliver("inbox", RAW, 1_000 + n)).collect();
        let first = fake.with(|s| s.changes("Email", &since, 2)).unwrap();
        assert!(first.has_more_changes);
        assert_eq!(first.created, ids[..2]);
        let rest = fake.with(|s| s.changes("Email", &first.new_state, 2)).unwrap();
        assert!(!rest.has_more_changes);
        assert_eq!(rest.created, ids[2..]);
    }
}
