//! Moves, marks and Delete Forever. K8 fills it.

use super::{Jmap, JmapApi};
use crate::services::{Relocated, Unapplied};
use crate::{BackendError, MailOp};

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn write(&self, messages: &[String], ops: &[MailOp]) -> Result<Vec<Relocated>, Unapplied> {
        let _ = (messages, ops);
        Err(Unapplied { taken: 0, error: BackendError::Unsupported, relocated: Vec::new() })
    }
}
