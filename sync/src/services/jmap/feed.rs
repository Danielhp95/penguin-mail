//! The change feed. K5 fills it.

use super::{Jmap, JmapApi};
use crate::BackendError;
use crate::services::{Changes, SyncState};

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn feed(&self, since: Option<&SyncState>) -> Result<Changes, BackendError> {
        let _ = since;
        Err(BackendError::Unsupported)
    }
}
