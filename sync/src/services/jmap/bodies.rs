//! Raw messages, structures and parts. K7 fills it.

use mailrs_mime::Parts;

use super::{Jmap, JmapApi};
use crate::BackendError;
use crate::services::RawMessage;

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn raws(&self, ids: &[String]) -> Result<Vec<RawMessage>, BackendError> {
        let _ = ids;
        Err(BackendError::Unsupported)
    }
    pub(super) async fn structure(&self, id: &str) -> Result<Parts, BackendError> {
        let _ = id;
        Err(BackendError::Unsupported)
    }
    pub(super) async fn part(&self, id: &str, path: &str) -> Result<Vec<u8>, BackendError> {
        let _ = (id, path);
        Err(BackendError::Unsupported)
    }
}
