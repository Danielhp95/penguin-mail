//! The Dock keeps the All Inboxes unread count even without an open window.

use std::rc::Rc;

use gtk::glib;
use mailrs_sync::mailbox::{Mailbox, Standard, View};
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSString;

use super::App;

impl App {
    pub(super) fn watch_dock_badge(self: &Rc<Self>) {
        let mut requested = self.dock_badge_changed.subscribe();
        let mut remote = self.core.lists().counts_changed();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            loop {
                tokio::select! {
                    changed = requested.changed() => if changed.is_err() { break },
                    changed = remote.changed() => if changed.is_err() { break },
                }
                glib::timeout_future(crate::tray::RECOUNT_AFTER).await;
                requested.borrow_and_update();
                remote.borrow_and_update();
                let Some(app) = weak.upgrade() else { break };
                let inbox = Mailbox::Unified(Standard::Inbox);
                // Include every account so the shared count cache keeps its server answers.
                let mut mailboxes: Vec<_> = app
                    .accounts()
                    .iter()
                    .map(|account| Mailbox::Standard {
                        account_id: account.id,
                        which: Standard::Inbox,
                    })
                    .collect();
                mailboxes.push(inbox.clone());
                let lists = app.core.lists();
                let counted = app
                    .core
                    .call(async move { lists.counts(&mailboxes, &inbox, &View::default()).await })
                    .await;
                // One count runs at a time; changes during it need a fresh answer.
                if requested.has_changed().unwrap_or(true) || remote.has_changed().unwrap_or(true) {
                    continue;
                }
                match counted {
                    Ok(counts) => {
                        let unread = counts
                            .mailboxes
                            .get(&Mailbox::Unified(Standard::Inbox))
                            .copied()
                            .unwrap_or(0);
                        let label = (unread > 0).then(|| NSString::from_str(&unread.to_string()));
                        let mtm =
                            MainThreadMarker::new().expect("the Dock updates on the GTK thread");
                        NSApplication::sharedApplication(mtm)
                            .dockTile()
                            .setBadgeLabel(label.as_deref());
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not count unread mail for the Dock")
                    }
                }
            }
        });
    }
}
