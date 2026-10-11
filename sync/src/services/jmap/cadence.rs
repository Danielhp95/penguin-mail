//! How often the account looks, and push. K11 fills it.

use std::time::Duration;

use super::{Jmap, JmapApi};

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn watch_mail(&self) {
        std::future::pending::<()>().await;
    }
    pub(super) fn poll_every(&self) -> Duration {
        Duration::from_secs(60)
    }
}
