//! The JMAP adapter over `FakeJmap`, through the same engine a real
//! account runs.

mod bodies;
mod contacts;
mod feed;
mod fetch;
mod identities;
mod mailboxes;
mod outgoing;
mod rules;
mod search;
mod vacation;
mod watch;
mod writes;

use std::sync::Arc;

use mailrs_domain::{ChangeEvent, Memberships};
use mailrs_store::{Db, accounts, messages};

use crate::fake::FakeJmap;
use crate::{AccountServices, AccountSync, AnyMail};

pub(crate) struct Harness {
    pub fake: Arc<FakeJmap>,
    pub sync: Arc<AccountSync>,
    pub db: Db,
    pub events: async_channel::Receiver<ChangeEvent>,
    pub account_id: i64,
    _dir: tempfile::TempDir,
}

pub(crate) async fn fastmail() -> Harness {
    over(FakeJmap::new()).await
}

pub(crate) async fn over(fake: FakeJmap) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("mail.db")).unwrap();
    let account_id = db.write(|c| accounts::insert_account(c, "me@example.com", 0)).await.unwrap();
    let fake = Arc::new(fake);
    let (sender, events) = async_channel::unbounded();
    let sync = Arc::new(AccountSync::new(account_id, AccountServices::fake_jmap(Arc::clone(&fake)), db.clone(), sender));
    Harness { fake, sync, db, events, account_id, _dir: dir }
}

/// A message to the account, dated now.
pub(crate) fn raw(message_id: &str, subject: &str) -> Vec<u8> {
    format!(
        "From: Ada Lovelace <ada@example.com>\r\nTo: me@example.com\r\nSubject: {subject}\r\nMessage-ID: <{message_id}>\r\nDate: {}\r\n\r\n{subject} body\r\n",
        chrono::Utc::now().to_rfc2822()
    )
    .into_bytes()
}

impl Harness {
    pub fn mail(&self) -> AnyMail {
        self.sync.services().mail.clone()
    }

    pub async fn bootstrap_all(&self) {
        self.sync.bootstrap().await.unwrap();
        self.drain();
    }

    pub async fn look(&self) {
        self.sync.incremental().await.unwrap();
    }

    pub async fn stored(&self, id: &str) -> bool {
        let (account, wanted) = (self.account_id, vec![id.to_string()]);
        self.db.read(move |c| messages::existing_ids(c, account, &wanted)).await.unwrap().contains(id)
    }

    pub async fn held(&self, id: &str) -> Memberships {
        let (account, wanted) = (self.account_id, vec![id.to_string()]);
        self.db.read(move |c| messages::memberships_of(c, account, &wanted)).await.unwrap().remove(id).unwrap_or_default()
    }

    pub fn drain(&self) -> Vec<ChangeEvent> {
        std::iter::from_fn(|| self.events.try_recv().ok()).collect()
    }
}
