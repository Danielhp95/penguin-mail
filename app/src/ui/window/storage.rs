//! The banner above the message list for an account whose server is
//! nearly full. Its one button opens Preferences on the Sync page, where
//! Space on the Server shows every account, and takes the banner down for
//! the run. One dismissed while nearly full comes back once the account
//! is full.

use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use mailrs_domain::AccountId;
use mailrs_domain::translate::{fill, gettext};
use mailrs_sync::Storage;

use super::MainWindow;

/// What the banner says about `address`, or `None` while it has room.
pub(super) fn banner_line(address: &str, storage: Storage) -> Option<String> {
    if !storage.past_warning() {
        return None;
    }
    Some(if storage.full() {
        fill(&gettext("{address} is full and may refuse new mail"), &[("address", address)])
    } else {
        fill(
            &gettext("{address} is {percent}% full"),
            &[("address", address), ("percent", &storage.percent().to_string())],
        )
    })
}

/// Whether a banner the person took down stays down. `when_full` says
/// whether the account was full then: one taken down while nearly full
/// comes back when the account fills, since that is new news.
pub(super) fn stays_dismissed(when_full: bool, storage: Storage) -> bool {
    when_full || !storage.full()
}

impl MainWindow {
    /// Puts up a banner for each account past its warning that the person
    /// has not taken down, beside the Grant Access banners, and takes down
    /// the rest. A banner already up keeps its widget and changes its words.
    pub(super) fn rebuild_storage_banners(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let storage = app.storage();
        let wanted: Vec<(AccountId, bool, String)> = {
            let dismissed = self.storage_dismissed.borrow();
            app.accounts()
                .iter()
                .filter_map(|account| {
                    let space = *storage.get(&account.id)?;
                    if dismissed.get(&account.id).is_some_and(|&full| stays_dismissed(full, space)) {
                        return None;
                    }
                    Some((account.id, space.full(), banner_line(&account.email, space)?))
                })
                .collect()
        };
        let ids: HashSet<AccountId> = wanted.iter().map(|(id, _, _)| *id).collect();
        let mut widgets = self.storage_banner_widgets.borrow_mut();
        widgets.retain(|id, (banner, _)| {
            let keep = ids.contains(id);
            if !keep {
                self.grant_banners.remove(banner);
            }
            keep
        });
        for (id, full, line) in wanted {
            if let Some((banner, was_full)) = widgets.get_mut(&id) {
                banner.set_title(&line);
                *was_full = full;
                continue;
            }
            // The title holds the address, which may carry an "&".
            let banner = adw::Banner::builder()
                .use_markup(false)
                .title(line)
                .button_label(gettext("Review Storage"))
                .css_classes(["storage-banner"])
                .revealed(true)
                .build();
            let weak = Rc::downgrade(self);
            // Runs after this rebuild returned and dropped its borrows, so
            // the borrows below meet no other (gtk-rs-refcell-reentrancy).
            banner.connect_button_clicked(move |_| {
                let Some(window) = weak.upgrade() else {
                    return;
                };
                let full = window.storage_banner_widgets.borrow().get(&id).is_some_and(|(_, full)| *full);
                window.storage_dismissed.borrow_mut().insert(id, full);
                window.rebuild_storage_banners();
                window.show_preferences_page("sync");
            });
            self.grant_banners.append(&banner);
            widgets.insert(id, (banner, full));
        }
    }
}

#[cfg(test)]
mod tests {
    use mailrs_sync::Storage;

    use super::{banner_line, stays_dismissed};

    fn used(used: u64) -> Storage {
        Storage::new(used, 100, None).unwrap()
    }

    #[test]
    fn the_banner_waits_for_the_warning() {
        assert_eq!(banner_line("dana@fastmail.example", used(50)), None);
    }

    #[test]
    fn the_banner_says_how_full_past_the_warning() {
        assert_eq!(
            banner_line("dana.reyes@work.example", used(95)).as_deref(),
            Some("dana.reyes@work.example is 95% full")
        );
    }

    #[test]
    fn the_banner_takes_the_servers_own_warning_mark() {
        let storage = Storage::new(85, 100, Some(80)).unwrap();
        assert_eq!(
            banner_line("dana@fastmail.example", storage).as_deref(),
            Some("dana@fastmail.example is 85% full")
        );
    }

    #[test]
    fn the_banner_says_a_full_account_may_refuse_mail() {
        assert_eq!(
            banner_line("dana@home.example", used(100)).as_deref(),
            Some("dana@home.example is full and may refuse new mail")
        );
    }

    #[test]
    fn a_banner_dismissed_while_nearly_full_comes_back_once_full() {
        assert!(stays_dismissed(false, used(97)));
        assert!(!stays_dismissed(false, used(100)));
    }

    #[test]
    fn a_banner_dismissed_while_full_stays_down() {
        assert!(stays_dismissed(true, used(100)));
    }
}
