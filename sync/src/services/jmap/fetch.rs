//! The window, ids, metadata and whole threads. K6 fills it.

use mailrs_domain::MessageMeta;

use super::{Jmap, JmapApi};
use crate::BackendError;
use crate::services::{Backfill, Found, RemoteRef, Want};

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn backfill_page(&self, days: i64, cursor: Option<&str>) -> Result<Backfill, BackendError> {
        let _ = (days, cursor);
        Err(BackendError::Unsupported)
    }
    pub(super) async fn ids_in(&self, days: Option<i64>, mailbox: Option<&str>) -> Result<Vec<RemoteRef>, BackendError> {
        let _ = (days, mailbox);
        Err(BackendError::Unsupported)
    }
    pub(super) async fn inbox_listing(&self) -> Result<Vec<RemoteRef>, BackendError> {
        Err(BackendError::Unsupported)
    }
    pub(super) async fn fetch_metas(&self, wants: Vec<Want>) -> Result<Found, BackendError> {
        let _ = wants;
        Err(BackendError::Unsupported)
    }
    pub(super) async fn fetch_threads(&self, threads: Vec<String>) -> Result<Found, BackendError> {
        let _ = threads;
        Err(BackendError::Unsupported)
    }
    pub(super) async fn sent_with(&self, message_id: &str) -> Result<Option<String>, BackendError> {
        let _ = message_id;
        Err(BackendError::Unsupported)
    }
    #[allow(dead_code)] // Used once the window is fetched.
    pub(super) fn meta_of(&self, email: &mailrs_jmap::Email) -> MessageMeta {
        MessageMeta {
            account_id: 0,
            id: email.id.clone(),
            thread_id: email.thread_id.clone(),
            rfc822_msgid: None,
            from: None,
            to: Vec::new(),
            cc: Vec::new(),
            subject: String::new(),
            date: 0,
            snippet: String::new(),
            size: 0,
            has_attachments: false,
            held: mailrs_domain::Memberships::default(),
            roles: Vec::new(),
            list_unsubscribe: None,
            one_click: false,
        }
    }
}
