//! How full each account's space on its server is. Read ten seconds after
//! start, once the accounts are connected, and then once an hour: the
//! numbers move slowly, and each read is one small call per account.

use std::collections::HashMap;
use std::rc::Rc;

use gtk::glib;
use mailrs_domain::AccountId;
use mailrs_sync::Storage;

use super::App;
use crate::ui::window::Notice;

const FIRST_LOOK_SECONDS: u32 = 10;
const EVERY_SECONDS: u32 = 60 * 60;

impl App {
    pub(super) fn start_storage_checks(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(FIRST_LOOK_SECONDS, move || {
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
                let Some(sync) = this.core.account(account.id) else {
                    continue;
                };
                match this.core.call(async move { sync.storage().await }).await {
                    Ok(Some(storage)) => {
                        found.insert(account.id, storage);
                    }
                    Ok(None) => {}
                    // A read that failed keeps the last answer, so a
                    // moment offline takes no banner down.
                    Err(err) => {
                        tracing::info!(
                            account = account.id,
                            error = %err,
                            "could not read the account's storage"
                        );
                        if let Some(last) = this.storage.borrow().get(&account.id) {
                            found.insert(account.id, *last);
                        }
                    }
                }
            }
            if *this.storage.borrow() != found {
                *this.storage.borrow_mut() = found;
                this.tell_window(Notice::StorageChanged);
            }
        });
    }

    /// How full each account's server said it was at the last read.
    pub fn storage(&self) -> HashMap<AccountId, Storage> {
        self.storage.borrow().clone()
    }
}
