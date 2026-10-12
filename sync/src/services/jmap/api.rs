//! JMAP as the JMAP adapter uses it. A trait, so the adapter runs over the
//! real client or over `FakeJmap` alike, as the Microsoft adapter runs over
//! `GraphApi`. Each method is one `mailrs_jmap::JmapClient` call, so each
//! is one request on the wire.

use std::sync::Arc;
use std::time::Duration;

use mailrs_jmap::{
    Email, EmailQuery, Got, Identity, Imported, JmapClient, JmapError, Listed, MailChanges, Mailbox, MailboxWrite,
    Pushed, Queried, Saved, Session, SetResponse, Submission, Threads,
};
use serde_json::{Map, Value};

pub(crate) type Answer<T> = Result<T, JmapError>;

/// The push stream gives up after twice the ping it asks for.
const PUSH_IDLE: Duration = Duration::from_secs(2 * mailrs_jmap::push::PING_SECONDS as u64);

pub trait JmapApi: Send + Sync + 'static {
    fn session(&self) -> impl Future<Output = Answer<Arc<Session>>> + Send;
    fn mailboxes(&self) -> impl Future<Output = Answer<Listed<Mailbox>>> + Send;
    fn mail_changes(&self, email: &str, mailbox: &str, max: usize) -> impl Future<Output = Answer<MailChanges>> + Send;
    fn email_state(&self) -> impl Future<Output = Answer<String>> + Send;
    fn query(&self, query: &EmailQuery) -> impl Future<Output = Answer<Queried>> + Send;
    fn query_after(&self, query: &EmailQuery, anchor: &str) -> impl Future<Output = Answer<Queried>> + Send;
    fn emails(&self, ids: &[String], properties: &[&str]) -> impl Future<Output = Answer<Got<Email>>> + Send;
    fn threads(&self, ids: &[String]) -> impl Future<Output = Answer<Threads>> + Send;
    fn set_emails(&self, update: &Map<String, Value>, destroy: &[String]) -> impl Future<Output = Answer<SetResponse>> + Send;
    fn set_mailbox(&self, write: &MailboxWrite) -> impl Future<Output = Answer<String>> + Send;
    fn download(&self, blob_id: &str, limit: usize) -> impl Future<Output = Answer<Vec<u8>>> + Send;
    fn upload(&self, bytes: Vec<u8>) -> impl Future<Output = Answer<String>> + Send;
    fn import(&self, blob_id: &str, mailbox_ids: &[String], keywords: &[&str], destroy: Option<&str>) -> impl Future<Output = Answer<Saved>> + Send;
    fn submit(&self, submission: &Submission) -> impl Future<Output = Answer<Imported>> + Send;
    fn identities(&self) -> impl Future<Output = Answer<Vec<Identity>>> + Send;
    fn push(&self) -> impl Future<Output = Answer<Pushed>> + Send;
    /// When push comes back, while a stalled call has paused it.
    fn push_paused(&self) -> Option<tokio::time::Instant>;
}

impl JmapApi for JmapClient {
    async fn session(&self) -> Answer<Arc<Session>> {
        JmapClient::session(self).await
    }
    async fn mailboxes(&self) -> Answer<Listed<Mailbox>> {
        JmapClient::mailboxes(self).await
    }
    async fn mail_changes(&self, email: &str, mailbox: &str, max: usize) -> Answer<MailChanges> {
        JmapClient::mail_changes(self, email, mailbox, max).await
    }
    async fn email_state(&self) -> Answer<String> {
        JmapClient::email_state(self).await
    }
    async fn query(&self, query: &EmailQuery) -> Answer<Queried> {
        JmapClient::query(self, query).await
    }
    async fn query_after(&self, query: &EmailQuery, anchor: &str) -> Answer<Queried> {
        JmapClient::query_after(self, query, anchor).await
    }
    async fn emails(&self, ids: &[String], properties: &[&str]) -> Answer<Got<Email>> {
        JmapClient::emails(self, ids, properties).await
    }
    async fn threads(&self, ids: &[String]) -> Answer<Threads> {
        JmapClient::threads(self, ids).await
    }
    async fn set_emails(&self, update: &Map<String, Value>, destroy: &[String]) -> Answer<SetResponse> {
        JmapClient::set_emails(self, update, destroy).await
    }
    async fn set_mailbox(&self, write: &MailboxWrite) -> Answer<String> {
        JmapClient::set_mailbox(self, write).await
    }
    async fn download(&self, blob_id: &str, limit: usize) -> Answer<Vec<u8>> {
        let account = self.mail_account().await?;
        JmapClient::download(self, &account, blob_id, "message.eml", "application/octet-stream", limit).await
    }
    async fn upload(&self, bytes: Vec<u8>) -> Answer<String> {
        let account = self.mail_account().await?;
        Ok(JmapClient::upload(self, &account, "message/rfc822", bytes).await?.blob_id)
    }
    async fn import(&self, blob_id: &str, mailbox_ids: &[String], keywords: &[&str], destroy: Option<&str>) -> Answer<Saved> {
        JmapClient::import(self, blob_id, mailbox_ids, keywords, destroy).await
    }
    async fn submit(&self, submission: &Submission) -> Answer<Imported> {
        JmapClient::submit(self, submission).await
    }
    async fn identities(&self) -> Answer<Vec<Identity>> {
        mailrs_jmap::identity::identities(self).await
    }
    async fn push(&self) -> Answer<Pushed> {
        JmapClient::push(self, PUSH_IDLE).await
    }
    fn push_paused(&self) -> Option<tokio::time::Instant> {
        JmapClient::push_paused(self)
    }
}
