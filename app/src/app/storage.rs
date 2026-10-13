//! How full each account's space on its server is. Read ten seconds after
//! start, again for each account as it connects, and then once an hour:
//! the numbers move slowly, and each read is one small call per account.

use std::collections::HashMap;
use std::rc::Rc;

use gtk::glib;
use mailrs_domain::AccountId;
use mailrs_sync::Storage;

use super::App;
use crate::ui::window::Notice;

const FIRST_LOOK_SECONDS: u32 = 10;
/// The demo's accounts connect at once, so its banner can go up before
/// the list lays out its rows rather than pushing them down under the
/// pointer.
const DEMO_FIRST_LOOK_SECONDS: u32 = 1;
const EVERY_SECONDS: u32 = 60 * 60;
/// How long one account's read may take before it counts as failed.
const READ_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

impl App {
    pub(super) fn start_storage_checks(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let first = if self.core.demo { DEMO_FIRST_LOOK_SECONDS } else { FIRST_LOOK_SECONDS };
        glib::timeout_add_seconds_local_once(first, move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            app.read_storage();
            let weak = Rc::downgrade(&app);
            glib::timeout_add_seconds_local(EVERY_SECONDS, move || {
                let Some(app) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                app.read_storage();
                glib::ControlFlow::Continue
            });
        });
    }

    /// Reads every account at once, so one slow server holds up none of
    /// the others, and puts the answers on the numbers as they are when
    /// the last one is in.
    pub(super) fn read_storage(self: &Rc<Self>) {
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let ids: Vec<AccountId> = this.accounts().iter().map(|a| a.id).collect();
            let reads = futures::future::join_all(ids.iter().map(|&id| {
                let this = Rc::clone(&this);
                async move { (id, this.read_one(id).await) }
            }))
            .await;
            this.show_storage(merged(this.storage(), &ids, reads));
        });
    }

    /// Reads `account_id`'s space as the account connects, so a full
    /// account's banner does not wait for the hourly read.
    pub fn read_storage_of(self: &Rc<Self>, account_id: AccountId) {
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let read = this.read_one(account_id).await;
            let mut found = this.storage();
            after_read(&mut found, account_id, read);
            this.show_storage(found);
        });
    }

    /// One account's read. `Err` when it failed, or the account is not
    /// connected, which keeps the last answer.
    async fn read_one(&self, account_id: AccountId) -> Result<Option<Storage>, ()> {
        let sync = self.core.account(account_id).ok_or(())?;
        let read = self.core.call(async move {
            match tokio::time::timeout(READ_WAIT, sync.storage()).await {
                Ok(read) => read.map_err(anyhow::Error::from),
                Err(_) => Err(anyhow::anyhow!("no answer in {} seconds", READ_WAIT.as_secs())),
            }
        });
        read.await.map_err(|err| {
            tracing::info!(account = account_id, error = %err, "could not read the account's storage");
        })
    }

    fn show_storage(&self, found: HashMap<AccountId, Storage>) {
        if *self.storage.borrow() != found {
            *self.storage.borrow_mut() = found;
            self.tell_window(Notice::StorageChanged);
        }
    }

    /// How full each account's server said it was at the last read.
    pub fn storage(&self) -> HashMap<AccountId, Storage> {
        self.storage.borrow().clone()
    }
}

/// `now` with the hourly `reads` put on it, for the accounts in
/// `accounts` alone. It starts from the numbers as they are when the reads
/// end, since an account that connected meanwhile read its own.
fn merged(
    mut now: HashMap<AccountId, Storage>,
    accounts: &[AccountId],
    reads: Vec<(AccountId, Result<Option<Storage>, ()>)>,
) -> HashMap<AccountId, Storage> {
    now.retain(|id, _| accounts.contains(id));
    for (account_id, read) in reads {
        after_read(&mut now, account_id, read);
    }
    now
}

/// `found` after `account_id`'s read: its numbers, none when the server
/// stopped saying, and the last answer when the read failed, so a moment
/// offline takes no banner down.
fn after_read(found: &mut HashMap<AccountId, Storage>, account_id: AccountId, read: Result<Option<Storage>, ()>) {
    match read {
        Ok(Some(storage)) => {
            found.insert(account_id, storage);
        }
        Ok(None) => {
            found.remove(&account_id);
        }
        Err(()) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use mailrs_sync::Storage;

    use super::{after_read, merged};

    fn used(n: u64) -> Storage {
        Storage::new(n, 100, None).unwrap()
    }

    #[test]
    fn a_read_replaces_the_account_s_numbers_and_leaves_the_others() {
        let mut found = HashMap::from([(1, used(10)), (2, used(20))]);
        after_read(&mut found, 1, Ok(Some(used(95))));
        assert_eq!(found, HashMap::from([(1, used(95)), (2, used(20))]));
        after_read(&mut found, 2, Ok(None));
        assert_eq!(found, HashMap::from([(1, used(95))]), "a server that stopped saying has no row");
    }

    /// The hourly read waits on every account. An account that connected
    /// meanwhile read its own numbers, and a failed hourly read must not
    /// put back the older ones it started from. An account that went is
    /// dropped.
    #[test]
    fn the_hourly_reads_land_on_the_numbers_as_they_are_now() {
        let now = HashMap::from([(1, used(10)), (2, used(96)), (3, used(50))]);
        let reads = vec![(1, Ok(Some(used(12)))), (2, Err(()))];
        assert_eq!(merged(now, &[1, 2], reads), HashMap::from([(1, used(12)), (2, used(96))]));
    }

    #[test]
    fn a_failed_read_keeps_the_last_answer() {
        let mut found = HashMap::from([(1, used(95))]);
        after_read(&mut found, 1, Err(()));
        assert_eq!(found, HashMap::from([(1, used(95))]), "a moment offline takes no banner down");
    }
}
