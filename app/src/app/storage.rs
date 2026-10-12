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

    pub(super) fn read_storage(self: &Rc<Self>) {
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let mut found = HashMap::new();
            for account in this.accounts() {
                if let Some(last) = this.storage.borrow().get(&account.id) {
                    found.insert(account.id, *last);
                }
                let read = this.read_one(account.id).await;
                after_read(&mut found, account.id, read);
            }
            this.show_storage(found);
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
        self.core.call(async move { sync.storage().await }).await.map_err(|err| {
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

    use super::after_read;

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

    #[test]
    fn a_failed_read_keeps_the_last_answer() {
        let mut found = HashMap::from([(1, used(95))]);
        after_read(&mut found, 1, Err(()));
        assert_eq!(found, HashMap::from([(1, used(95))]), "a moment offline takes no banner down");
    }
}
