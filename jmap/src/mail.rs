//! The mail calls the adapter makes (RFC 8621), each in one request: a
//! listing with its back-referenced fetch, the change feed with the
//! changed messages' places and marks, patches, import, and sending with
//! the sent copy filed by the same request.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::method_error;
use crate::identity::EmailAddress;
use crate::session::{MAIL, SUBMISSION};
use crate::{JmapClient, JmapError, Request, Session};

/// What a message's row in a list needs.
pub const META: &[&str] = &[
    "id", "blobId", "threadId", "mailboxIds", "keywords", "size", "receivedAt", "messageId", "from", "to", "cc",
    "subject", "preview", "hasAttachment", "header:List-Unsubscribe:asRaw", "header:List-Unsubscribe-Post:asRaw",
];

/// Where a message sits and what it carries, for the change feed.
pub const PLACES: &[&str] = &["id", "threadId", "mailboxIds", "keywords"];

/// A message's parts, for one too large to fetch whole.
pub const BODY: &[&str] = &["id", "blobId", "size", "headers", "bodyStructure"];

/// The part properties `BODY` asks for.
const PART_PROPERTIES: &[&str] = &["partId", "blobId", "size", "type", "charset", "disposition", "cid", "name", "subParts"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mailbox {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub sort_order: u32,
    #[serde(default)]
    pub total_emails: u64,
    #[serde(default)]
    pub unread_emails: u64,
    #[serde(default)]
    pub total_threads: u64,
    #[serde(default)]
    pub unread_threads: u64,
    #[serde(default)]
    pub is_subscribed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyPart {
    #[serde(default)]
    pub part_id: Option<String>,
    #[serde(default)]
    pub blob_id: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default, rename = "type")]
    pub mime_type: String,
    #[serde(default)]
    pub charset: Option<String>,
    #[serde(default)]
    pub disposition: Option<String>,
    #[serde(default)]
    pub cid: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub sub_parts: Option<Vec<BodyPart>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Email {
    pub id: String,
    #[serde(default)]
    pub blob_id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub mailbox_ids: BTreeMap<String, bool>,
    #[serde(default)]
    pub keywords: BTreeMap<String, bool>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default)]
    pub message_id: Option<Vec<String>>,
    #[serde(default)]
    pub from: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub to: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub cc: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    pub has_attachment: bool,
    #[serde(default, rename = "header:List-Unsubscribe:asRaw")]
    pub list_unsubscribe: Option<String>,
    #[serde(default, rename = "header:List-Unsubscribe-Post:asRaw")]
    pub list_unsubscribe_post: Option<String>,
    #[serde(default)]
    pub headers: Option<Vec<Header>>,
    #[serde(default)]
    pub body_structure: Option<BodyPart>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub email_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Listed<T> {
    #[serde(default)]
    pub state: String,
    pub list: Vec<T>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Got<T> {
    #[serde(default)]
    pub state: String,
    pub list: Vec<T>,
    #[serde(default)]
    pub not_found: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesResponse {
    #[serde(default)]
    pub old_state: String,
    pub new_state: String,
    #[serde(default)]
    pub has_more_changes: bool,
    #[serde(default)]
    pub created: Vec<String>,
    #[serde(default)]
    pub updated: Vec<String>,
    #[serde(default)]
    pub destroyed: Vec<String>,
    /// Mailbox/changes only: the properties that changed, when the server
    /// says only counts did.
    #[serde(default)]
    pub updated_properties: Option<Vec<String>>,
}

/// One page of the change feed: the Email and Mailbox changes, and where
/// each created or updated message now sits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailChanges {
    pub email: ChangesResponse,
    pub mailbox: ChangesResponse,
    pub changed: Vec<Email>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmailQuery {
    pub filter: Option<Value>,
    pub position: u64,
    pub limit: usize,
    /// Ask the server to count every match, which some servers find dear.
    pub total: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub id: String,
    pub thread_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queried {
    pub hits: Vec<Hit>,
    pub position: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Threads {
    pub threads: Vec<Thread>,
    pub emails: Vec<Email>,
    pub not_found: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SetError {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub description: Option<String>,
}

impl SetError {
    pub fn error(&self) -> JmapError {
        method_error(&self.kind, self.description.as_deref())
    }
}

/// A `/set` answer. Each map is null when it holds nothing (RFC 8620
/// section 5.3).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetResponse {
    #[serde(default)]
    pub new_state: Option<String>,
    #[serde(default)]
    pub created: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    pub updated: Option<BTreeMap<String, Option<Value>>>,
    #[serde(default)]
    pub destroyed: Option<Vec<String>>,
    #[serde(default)]
    pub not_created: Option<BTreeMap<String, SetError>>,
    #[serde(default)]
    pub not_updated: Option<BTreeMap<String, SetError>>,
    #[serde(default)]
    pub not_destroyed: Option<BTreeMap<String, SetError>>,
}

impl SetResponse {
    /// Why the server refused the update or the destroy of `id`.
    pub fn refusal(&self, id: &str) -> Option<&SetError> {
        self.not_updated.as_ref().and_then(|n| n.get(id)).or_else(|| self.not_destroyed.as_ref().and_then(|n| n.get(id)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxWrite {
    Create { name: String, parent_id: Option<String> },
    Update { id: String, name: String, parent_id: Option<String> },
    Destroy { id: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Imported {
    pub id: String,
    #[serde(default)]
    pub thread_id: String,
}

/// A saved message. `leftover` is why the older draft it replaces could not
/// be destroyed, when that failed; the old draft is then still on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub email: Imported,
    pub leftover: Option<JmapError>,
}

/// A message to send: its blob, who it goes from and to, and where its
/// copy waits and is filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub blob_id: String,
    pub identity_id: String,
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
    /// The copy waits here with `$draft` until the submission is made.
    pub drafts: Option<String>,
    /// The copy moves here once the submission is made.
    pub sent: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportAnswer {
    #[serde(default)]
    created: Option<BTreeMap<String, Imported>>,
    #[serde(default)]
    not_created: Option<BTreeMap<String, ImportRefusal>>,
}

/// What one import did.
enum Outcome {
    Made(Imported),
    /// The server keeps one copy of a message per account and already has
    /// these bytes, under this id (RFC 8621 section 4.8, `alreadyExists`).
    /// Cyrus, and so Fastmail, answers this.
    Exists(String),
}

impl ImportAnswer {
    fn one(mut self, key: &str) -> Result<Outcome, JmapError> {
        if let Some(refused) = self.not_created.as_mut().and_then(|n| n.remove(key)) {
            return match (refused.kind.as_str(), refused.existing_id) {
                ("alreadyExists", Some(existing)) => Ok(Outcome::Exists(existing)),
                _ => Err(method_error(&refused.kind, refused.description.as_deref())),
            };
        }
        self.created
            .and_then(|mut c| c.remove(key))
            .map(Outcome::Made)
            .ok_or_else(|| JmapError::Decode("the import created nothing".into()))
    }
}

/// A `notCreated` entry of an import, which may name the copy the server
/// already has.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportRefusal {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    existing_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryAnswer {
    #[serde(default)]
    position: u64,
    #[serde(default)]
    total: Option<u64>,
    ids: Vec<String>,
}

/// `segment` as one JSON pointer segment of a patch path (RFC 6901).
pub fn pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn account(session: &Session) -> Result<String, JmapError> {
    session.account_for(MAIL).map(str::to_string).ok_or_else(|| JmapError::UnknownCapability(MAIL.into()))
}

impl JmapClient {
    /// The account that holds the mail.
    pub async fn mail_account(&self) -> Result<String, JmapError> {
        account(&*self.session().await?)
    }

    pub async fn mailboxes(&self) -> Result<Listed<Mailbox>, JmapError> {
        let account = self.mail_account().await?;
        let mut request = Request::new();
        let id = request.call(MAIL, "Mailbox/get", json!({ "accountId": account, "ids": null }));
        self.send(&request).await?.take(&id, "Mailbox/get")
    }

    /// The Email state now, with no message.
    pub async fn email_state(&self) -> Result<String, JmapError> {
        let account = self.mail_account().await?;
        let mut request = Request::new();
        let id = request.call(MAIL, "Email/get", json!({ "accountId": account, "ids": [], "properties": ["id"] }));
        let got: Got<Email> = self.send(&request).await?.take(&id, "Email/get")?;
        Ok(got.state)
    }

    /// The changes since `email_since` and `mailbox_since`, at most `max`
    /// messages, with where each created or updated message now sits.
    pub async fn mail_changes(&self, email_since: &str, mailbox_since: &str, max: usize) -> Result<MailChanges, JmapError> {
        let account = self.mail_account().await?;
        let mut request = Request::new();
        let changes = request.call(MAIL, "Email/changes", json!({ "accountId": account, "sinceState": email_since, "maxChanges": max }));
        let created = request.call(MAIL, "Email/get", json!({
            "accountId": account, "#ids": changes.result_of("Email/changes", "/created"), "properties": PLACES
        }));
        let updated = request.call(MAIL, "Email/get", json!({
            "accountId": account, "#ids": changes.result_of("Email/changes", "/updated"), "properties": PLACES
        }));
        let mailboxes = request.call(MAIL, "Mailbox/changes", json!({ "accountId": account, "sinceState": mailbox_since }));
        let mut answer = self.send(&request).await?;
        let email: ChangesResponse = answer.take(&changes, "Email/changes")?;
        let mut changed = answer.take::<Got<Email>>(&created, "Email/get")?.list;
        changed.extend(answer.take::<Got<Email>>(&updated, "Email/get")?.list);
        let mailbox: ChangesResponse = answer.take(&mailboxes, "Mailbox/changes")?;
        Ok(MailChanges { email, mailbox, changed })
    }

    /// One page of a query, newest first, each hit with its thread.
    pub async fn query(&self, query: &EmailQuery) -> Result<Queried, JmapError> {
        let account = self.mail_account().await?;
        let mut args = json!({
            "accountId": account,
            "sort": [{ "property": "receivedAt", "isAscending": false }],
            "position": query.position,
            "limit": query.limit,
            "calculateTotal": query.total,
        });
        if let Some(filter) = &query.filter {
            args["filter"] = filter.clone();
        }
        let mut request = Request::new();
        let found = request.call(MAIL, "Email/query", args);
        let threads = request.call(MAIL, "Email/get", json!({
            "accountId": account, "#ids": found.result_of("Email/query", "/ids"), "properties": ["id", "threadId"]
        }));
        let mut answer = self.send(&request).await?;
        let listed: QueryAnswer = answer.take(&found, "Email/query")?;
        let got: Got<Email> = answer.take(&threads, "Email/get")?;
        let thread_of: BTreeMap<String, String> = got.list.into_iter().map(|e| (e.id, e.thread_id)).collect();
        let hits = listed
            .ids
            .into_iter()
            .map(|id| Hit { thread_id: thread_of.get(&id).cloned().unwrap_or_else(|| id.clone()), id })
            .collect();
        Ok(Queried { hits, position: listed.position, total: listed.total })
    }

    /// `ids` with `properties`, a request per `maxObjectsInGet`.
    pub async fn emails(&self, ids: &[String], properties: &[&str]) -> Result<Got<Email>, JmapError> {
        let session = self.session().await?;
        let account = account(&session)?;
        let step = session.limits().max_objects_in_get.clamp(1, 500);
        let mut all = Got::default();
        for chunk in ids.chunks(step) {
            let mut args = json!({ "accountId": account, "ids": chunk, "properties": properties });
            if properties.contains(&"bodyStructure") {
                args["bodyProperties"] = json!(PART_PROPERTIES);
            }
            let mut request = Request::new();
            let id = request.call(MAIL, "Email/get", args);
            let got: Got<Email> = self.send(&request).await?.take(&id, "Email/get")?;
            all.state = got.state;
            all.list.extend(got.list);
            all.not_found.extend(got.not_found);
        }
        Ok(all)
    }

    /// The threads `ids` and every message in them, with `META`.
    pub async fn threads(&self, ids: &[String]) -> Result<Threads, JmapError> {
        let account = self.mail_account().await?;
        let mut request = Request::new();
        let threads = request.call(MAIL, "Thread/get", json!({ "accountId": account, "ids": ids }));
        let emails = request.call(MAIL, "Email/get", json!({
            "accountId": account, "#ids": threads.result_of("Thread/get", "/list/*/emailIds"), "properties": META
        }));
        let mut answer = self.send(&request).await?;
        let listed: Got<Thread> = answer.take(&threads, "Thread/get")?;
        let got: Got<Email> = answer.take(&emails, "Email/get")?;
        Ok(Threads { threads: listed.list, emails: got.list, not_found: listed.not_found })
    }

    /// Patches `update` (id to patch object) and destroys `destroy`.
    pub async fn set_emails(&self, update: &Map<String, Value>, destroy: &[String]) -> Result<SetResponse, JmapError> {
        let account = self.mail_account().await?;
        let mut args = json!({ "accountId": account });
        if !update.is_empty() {
            args["update"] = Value::Object(update.clone());
        }
        if !destroy.is_empty() {
            args["destroy"] = json!(destroy);
        }
        let mut request = Request::new();
        let id = request.call(MAIL, "Email/set", args);
        self.send(&request).await?.take(&id, "Email/set")
    }

    /// Makes, renames or deletes a mailbox, and answers its id.
    pub async fn set_mailbox(&self, write: &MailboxWrite) -> Result<String, JmapError> {
        let account = self.mail_account().await?;
        let mut args = json!({ "accountId": account });
        let key = match write {
            MailboxWrite::Create { name, parent_id } => {
                args["create"] = json!({ "b": { "name": name, "parentId": parent_id } });
                "b".to_string()
            }
            MailboxWrite::Update { id, name, parent_id } => {
                args["update"] = json!({ id.clone(): { "name": name, "parentId": parent_id } });
                id.clone()
            }
            MailboxWrite::Destroy { id } => {
                args["destroy"] = json!([id]);
                args["onDestroyRemoveEmails"] = json!(false);
                id.clone()
            }
        };
        let mut request = Request::new();
        let call = request.call(MAIL, "Mailbox/set", args);
        let set: SetResponse = self.send(&request).await?.take(&call, "Mailbox/set")?;
        if let Some(refused) = set.not_created.as_ref().and_then(|n| n.get(&key)).or_else(|| set.refusal(&key)) {
            return Err(refused.error());
        }
        match write {
            MailboxWrite::Create { .. } => set
                .created
                .and_then(|c| c.get("b").and_then(|b| b["id"].as_str()).map(str::to_string))
                .ok_or_else(|| JmapError::Decode("the mailbox came back without an id".into())),
            _ => Ok(key),
        }
    }

    /// Imports a blob as a message in `mailbox_ids` with `keywords`. When
    /// `destroy` names an older draft, it is destroyed in a second request,
    /// only after the import is known to have created the new message, so a
    /// refused import never costs the old draft. A destroy the server
    /// refuses leaves the old draft in place; the new message is still
    /// returned and the refusal is in `Saved::leftover`.
    pub async fn import(
        &self,
        blob_id: &str,
        mailbox_ids: &[String],
        keywords: &[&str],
        destroy: Option<&str>,
    ) -> Result<Saved, JmapError> {
        let account = self.mail_account().await?;
        let mailboxes: Map<String, Value> = mailbox_ids.iter().map(|m| (m.clone(), json!(true))).collect();
        let marks: Map<String, Value> = keywords.iter().map(|k| (k.to_string(), json!(true))).collect();
        let mut request = Request::new();
        let import = request.call(MAIL, "Email/import", json!({
            "accountId": account, "emails": { "m": { "blobId": blob_id, "mailboxIds": mailboxes, "keywords": marks } }
        }));
        let email = match self.send(&request).await?.take::<ImportAnswer>(&import, "Email/import")?.one("m")? {
            Outcome::Made(email) => email,
            Outcome::Exists(existing) => self.file_existing(&account, &existing, mailbox_ids, keywords, destroy).await?,
        };
        // The server's copy of unchanged bytes is the old draft itself.
        let leftover = match destroy.filter(|old| *old != email.id) {
            Some(old) => self.destroy_email(old).await.err(),
            None => None,
        };
        Ok(Saved { email, leftover })
    }

    /// Adds `mailbox_ids` and `keywords` to the copy the server already
    /// has, as an import of the same bytes would have set them, and answers
    /// it with its thread. Only a draft is taken over this way: the draft
    /// being replaced, or another draft. Any other message that holds the
    /// same bytes is refused as `alreadyExists` and left as it is.
    async fn file_existing(
        &self,
        account: &str,
        id: &str,
        mailbox_ids: &[String],
        keywords: &[&str],
        replacing: Option<&str>,
    ) -> Result<Imported, JmapError> {
        let mut request = Request::new();
        let got = request.call(MAIL, "Email/get", json!({ "accountId": account, "ids": [id], "properties": ["id", "threadId", "keywords"] }));
        let found: Got<Email> = self.send(&request).await?.take(&got, "Email/get")?;
        let email = found.list.into_iter().next().ok_or(JmapError::NotFound)?;
        let draft = email.keywords.get("$draft").copied().unwrap_or(false);
        if replacing != Some(id) && !draft {
            return Err(JmapError::Method { kind: "alreadyExists".into(), description: String::new() });
        }
        let mut patch = Map::new();
        for mailbox in mailbox_ids {
            patch.insert(format!("mailboxIds/{}", pointer(mailbox)), json!(true));
        }
        for keyword in keywords {
            patch.insert(format!("keywords/{}", pointer(keyword)), json!(true));
        }
        let mut update = Map::new();
        update.insert(id.to_string(), Value::Object(patch));
        let set = self.set_emails(&update, &[]).await?;
        if let Some(refused) = set.refusal(id) {
            return Err(refused.error());
        }
        Ok(Imported { id: email.id, thread_id: email.thread_id })
    }

    /// The patch that files a sent copy: out of Drafts and into Sent when
    /// the submission names both, into `sent` alone otherwise, and never a
    /// draft again.
    fn filed(submission: &Submission) -> Map<String, Value> {
        let mut moved = Map::new();
        if let (Some(drafts), Some(sent)) = (&submission.drafts, &submission.sent)
            && drafts != sent
        {
            moved.insert(format!("mailboxIds/{}", pointer(drafts)), Value::Null);
        }
        if let Some(sent) = &submission.sent {
            moved.insert(format!("mailboxIds/{}", pointer(sent)), json!(true));
        }
        moved.insert("keywords/$draft".into(), Value::Null);
        moved
    }

    /// The copy the server already has as `id`, filed as sent, with its
    /// thread: for a server that took a submission of it at once.
    async fn file_sent_copy(&self, account: &str, id: &str, submission: &Submission) -> Result<Imported, JmapError> {
        let mut request = Request::new();
        request.call(MAIL, "Email/set", json!({ "accountId": account, "update": { id: Self::filed(submission) } }));
        let got = request.call(MAIL, "Email/get", json!({ "accountId": account, "ids": [id], "properties": ["id", "threadId"] }));
        // The message went out; a filing the server refuses does not undo that.
        self.found_copy(self.send(&request).await?.take(&got, "Email/get")?)
    }

    /// The one message an `Email/get` of an existing copy answers.
    fn found_copy(&self, got: Got<Email>) -> Result<Imported, JmapError> {
        let email = got.list.into_iter().next().ok_or(JmapError::NotFound)?;
        Ok(Imported { id: email.id, thread_id: email.thread_id })
    }

    /// Destroys one message, and says why when the server refuses.
    async fn destroy_email(&self, id: &str) -> Result<(), JmapError> {
        let set = self.set_emails(&Map::new(), &[id.to_string()]).await?;
        match set.refusal(id) {
            Some(refused) => Err(refused.error()),
            None => Ok(()),
        }
    }

    /// Imports the message, submits it, and moves the copy from Drafts to
    /// Sent without `$draft` once the submission is made, in one request
    /// (RFC 8621 section 7.5). When the submission is refused, by a `notCreated`
    /// entry or a method-level error, the imported copy is destroyed in a
    /// second request and the submission's error is returned; if that
    /// destroy fails too, the copy stays in Drafts and the error is the same.
    pub async fn submit(&self, submission: &Submission) -> Result<Imported, JmapError> {
        let session = self.session().await?;
        let account = account(&session)?;
        let sender = session.account_for(SUBMISSION).ok_or_else(|| JmapError::UnknownCapability(SUBMISSION.into()))?.to_string();
        let home = submission
            .drafts
            .as_ref()
            .or(submission.sent.as_ref())
            .ok_or_else(|| JmapError::Forbidden("the account has neither Drafts nor Sent".into()))?;
        let mut keywords = Map::new();
        keywords.insert("$seen".into(), json!(true));
        if submission.drafts.is_some() {
            keywords.insert("$draft".into(), json!(true));
        }
        let mut request = Request::new();
        let import = request.call(MAIL, "Email/import", json!({
            "accountId": account,
            "emails": { "m": { "blobId": submission.blob_id, "mailboxIds": { home.clone(): true }, "keywords": keywords } }
        }));
        let rcpt: Vec<Value> = submission.rcpt_to.iter().map(|a| json!({ "email": a })).collect();
        let mut args = json!({
            "accountId": sender,
            "create": { "s": {
                "identityId": submission.identity_id,
                "emailId": "#m",
                "envelope": { "mailFrom": { "email": submission.mail_from }, "rcptTo": rcpt }
            } }
        });
        if let (Some(drafts), Some(sent)) = (&submission.drafts, &submission.sent) {
            let mut moved = Map::new();
            moved.insert(format!("mailboxIds/{}", pointer(drafts)), Value::Null);
            moved.insert(format!("mailboxIds/{}", pointer(sent)), json!(true));
            moved.insert("keywords/$draft".into(), Value::Null);
            args["onSuccessUpdateEmail"] = json!({ "#s": moved });
        }
        let submitted = request.call(SUBMISSION, "EmailSubmission/set", args.clone());
        let mut answer = self.send(&request).await?;
        let email = match answer.take::<ImportAnswer>(&import, "Email/import")?.one("m")? {
            Outcome::Made(email) => email,
            // The submission named a copy that was never made, so nothing
            // went out. Send the server's own copy instead, and leave it be
            // if that is refused: this send did not make it.
            Outcome::Exists(existing) => {
                // A server may resolve `#m` to the copy it has and send it
                // at once; only a submission it did not make is sent again.
                let first = answer.take::<SetResponse>(&submitted, "EmailSubmission/set");
                let made = first.is_ok_and(|set| set.created.is_some_and(|c| c.contains_key("s")));
                return match made {
                    true => self.file_sent_copy(&account, &existing, submission).await,
                    false => self.submit_existing(&account, &existing, args, submission).await,
                };
            }
        };
        let refused = match answer.take::<SetResponse>(&submitted, "EmailSubmission/set") {
            Ok(set) => set.not_created.as_ref().and_then(|n| n.get("s")).map(SetError::error),
            Err(error) => Some(error),
        };
        if let Some(error) = refused {
            // Nothing went out, and the copy would wait in Drafts. If this
            // cleanup fails too, the submission's error still wins and the
            // copy stays; the log says so without naming it.
            if self.destroy_email(&email.id).await.is_err() {
                tracing::warn!("a refused submission left its imported copy on the server");
            }
            return Err(error);
        }
        Ok(email)
    }

    /// Submits the message the server already has as `id`, with the same
    /// envelope as `args` asked for a new copy, and files it as sent.
    async fn submit_existing(&self, account: &str, id: &str, mut args: Value, submission: &Submission) -> Result<Imported, JmapError> {
        args["create"]["s"]["emailId"] = json!(id);
        args["onSuccessUpdateEmail"] = json!({ "#s": Self::filed(submission) });
        let mut request = Request::new();
        let got = request.call(MAIL, "Email/get", json!({ "accountId": account, "ids": [id], "properties": ["id", "threadId"] }));
        let submitted = request.call(SUBMISSION, "EmailSubmission/set", args);
        let mut answer = self.send(&request).await?;
        let email = self.found_copy(answer.take(&got, "Email/get")?)?;
        let set: SetResponse = answer.take(&submitted, "EmailSubmission/set")?;
        if let Some(refused) = set.not_created.as_ref().and_then(|n| n.get("s")) {
            return Err(refused.error());
        }
        Ok(email)
    }
}
