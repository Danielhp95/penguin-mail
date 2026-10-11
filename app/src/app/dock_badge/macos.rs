//! The Dock keeps the All Inboxes unread count even without an open window.

use std::rc::Rc;

use gtk::glib;
use mailrs_sync::mailbox::{Mailbox, Standard, View};
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSString;

use super::super::App;

const INBOX: Mailbox = Mailbox::Unified(Standard::Inbox);

pub(in crate::app) struct Badge(tokio::sync::watch::Sender<()>);

impl Default for Badge {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(()).0)
    }
}

impl Badge {
    pub(in crate::app) fn recount(&self) {
        self.0.send_replace(());
    }

    pub(in crate::app) fn watch(&self, app: &Rc<App>) {
        let requested = self.0.subscribe();
        let remote = app.core.lists().counts_changed();
        let weak = Rc::downgrade(app);
        glib::spawn_future_local(recount(requested, remote, async move |refresh| {
            let Some(app) = weak.upgrade() else { return };
            // Include every account so the shared count cache keeps its server answers.
            let mut mailboxes: Vec<_> = app
                .accounts()
                .iter()
                .map(|account| Mailbox::Standard {
                    account_id: account.id,
                    which: Standard::Inbox,
                })
                .collect();
            mailboxes.push(INBOX);
            let lists = app.core.lists();
            let counted = app
                .core
                .call(async move {
                    if refresh {
                        lists.counts(&mailboxes, &INBOX, &View::default()).await
                    } else {
                        lists
                            .cached_counts(&mailboxes, &INBOX, &View::default())
                            .await
                    }
                })
                .await;
            match counted {
                Ok(counts) => {
                    let unread = counts.mailboxes.get(&INBOX).copied().unwrap_or(0);
                    let label = (unread > 0).then(|| NSString::from_str(&unread.to_string()));
                    let mtm = MainThreadMarker::new().expect("the Dock updates on the GTK thread");
                    NSApplication::sharedApplication(mtm)
                        .dockTile()
                        .setBadgeLabel(label.as_deref());
                }
                Err(error) => {
                    tracing::warn!(%error, "could not count unread mail for the Dock")
                }
            }
        }));
    }
}

// Remote answers and expiry wakes only read the cache. Local changes may refresh it.
async fn recount(
    mut requested: tokio::sync::watch::Receiver<()>,
    mut remote: tokio::sync::watch::Receiver<()>,
    mut count: impl AsyncFnMut(bool),
) {
    requested.mark_changed();
    loop {
        let refresh = tokio::select! {
            changed = requested.changed() => if changed.is_err() { break } else { true },
            changed = remote.changed() => if changed.is_err() { break } else { false },
        };
        glib::timeout_future(crate::tray::RECOUNT_AFTER).await;
        let refresh = refresh || requested.has_changed().unwrap_or(false);
        requested.borrow_and_update();
        remote.borrow_and_update();
        // Publish each completed count; changes during it remain queued for another pass.
        count(refresh).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_counts_without_events_and_changes_during_a_count_get_another_turn() {
        let (local, requested) = tokio::sync::watch::channel(());
        let (remote, answers) = tokio::sync::watch::channel(());
        let mut senders = Some((local, remote));
        let mut counted = Vec::new();
        glib::MainContext::new().block_on(recount(requested, answers, async |refresh| {
            match counted.len() {
                0 => {
                    senders.as_ref().unwrap().1.send_replace(());
                }
                1 => {
                    senders.as_ref().unwrap().0.send_replace(());
                    senders.as_ref().unwrap().1.send_replace(());
                }
                _ => {
                    senders.take();
                }
            }
            glib::timeout_future(std::time::Duration::from_millis(1)).await;
            counted.push(refresh);
        }));
        assert_eq!(counted, [true, false, true]);
    }
}
