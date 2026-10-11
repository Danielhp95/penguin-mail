//! Drafts and sending. K10 fills it.

use super::{Jmap, JmapApi};
use crate::BackendError;
use crate::api::{DraftRef, SavedDraft};

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn send_raw(&self, raw: &[u8]) -> Result<String, BackendError> {
        let _ = raw;
        Err(BackendError::Unsupported)
    }
    pub(super) async fn save(&self, draft_id: Option<&str>, raw: &[u8]) -> Result<SavedDraft, BackendError> {
        let _ = (draft_id, raw);
        Err(BackendError::Unsupported)
    }
    pub(super) async fn send_saved(&self, draft_id: &str) -> Result<String, BackendError> {
        let _ = draft_id;
        Err(BackendError::Unsupported)
    }
    pub(super) async fn drop_draft(&self, draft_id: &str) -> Result<(), BackendError> {
        let _ = draft_id;
        Err(BackendError::Unsupported)
    }
    pub(super) async fn drafts(&self) -> Result<Vec<DraftRef>, BackendError> {
        Err(BackendError::Unsupported)
    }
}
