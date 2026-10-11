//! Search. K9 fills it.

use super::{Jmap, JmapApi};
use crate::BackendError;
use crate::services::{RemoteRef, SearchQuery};

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn find(&self, query: &SearchQuery, limit: usize) -> Result<Vec<RemoteRef>, BackendError> {
        let _ = (query, limit);
        Err(BackendError::Unsupported)
    }
}
