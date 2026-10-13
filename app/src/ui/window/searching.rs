//! The window behind the listing run. `listing` decides what the thread
//! list lists and when; this file is the adapter that answers its reads
//! from the window, makes its store and server calls through the core,
//! and hands its rows to the thread list. It also wires the search field
//! to the run: a pause in typing runs the search over the mail on this
//! computer, and from three letters on over the servers too.

use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use mailrs_domain::{AccountId, Category, Folder, Target, ThreadSummary};
use mailrs_sync::{Changed, Listing, Loaded, Scope, Stop, View};

use super::listing::{Answer, Lists, Pane, Work};
use super::on_screen::{Redraw, Revealed};
use super::{MainWindow, follows, load_failed};
use crate::core::Core;
use crate::ui::Mailbox;
use crate::ui::list_feed::{Named, Refresh};

impl MainWindow {
    /// Hands the search field's keys and the search bar closing to the
    /// listing run.
    pub(super) fn install_search_typing(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.list.search_entry.connect_changed(move |entry| {
            if let Some(win) = weak.upgrade() {
                win.listing.typed(&entry.text());
            }
        });
        let weak = Rc::downgrade(self);
        self.list.search_button.connect_toggled(move |button| {
            let Some(win) = weak.upgrade() else { return };
            if !button.is_active() {
                win.listing.search_bar_closed();
            }
        });
    }

    /// Enter in the search field, or a picked suggestion: runs `text` at
    /// once, as far as the servers.
    pub(super) fn search_entered(self: &Rc<Self>, text: String) {
        self.listing.entered(&text);
    }
}

/// The window, as the listing run reads and changes it.
pub(super) struct ListPane(pub(super) Weak<MainWindow>);

impl ListPane {
    fn window(&self) -> Option<Rc<MainWindow>> {
        self.0.upgrade()
    }
}

impl Pane for ListPane {
    fn scope(&self) -> Scope {
        self.window()
            .map_or_else(|| Scope::over(Vec::new()), |win| win.scope())
    }

    fn view(&self, mailbox: &Mailbox, category: Category) -> View {
        self.window()
            .map(|win| win.view_of(mailbox, category))
            .unwrap_or_default()
    }

    fn follows(&self, mailbox: &Mailbox) -> Option<(AccountId, String)> {
        let win = self.window()?;
        follows(mailbox, win.offers(mailbox.account()?))
    }

    fn loaded(&self) -> Loaded {
        self.window()
            .map(|win| win.list.loaded())
            .unwrap_or_default()
    }

    fn redraw(&self, redraw: &Redraw) {
        if let Some(win) = self.window() {
            win.redraw(redraw);
        }
    }

    fn refreshing(&self, refresh: &Refresh) {
        let Some(win) = self.window() else { return };
        win.refresh_counts();
        // A conversation the events did not name has nothing new.
        win.refresh_open_threads(|account_id, thread_id| refresh.names(account_id, thread_id));
    }

    fn set_rows(&self, listing: Listing) {
        let Some(win) = self.window() else { return };
        for notice in &listing.notices {
            win.toast(notice);
        }
        let rows = listing.rows.into_iter().map(Rc::new).collect();
        win.list.set_empty_description(&listing.empty.description);
        win.list
            .set_rows(rows, &listing.empty.title, listing.empty.icon);
        win.follow_selection();
        win.list.set_title(&listing.title, &listing.subtitle);
    }

    fn append(&self, rows: Vec<ThreadSummary>) {
        if let Some(win) = self.window() {
            win.list.append(rows.into_iter().map(Rc::new).collect());
        }
    }

    fn replace(&self, changed: &[Named], rows: Vec<ThreadSummary>, title: &str, subtitle: &str) {
        let Some(win) = self.window() else { return };
        win.list.replace_threads(changed, rows);
        win.follow_selection();
        win.list.set_title(title, subtitle);
    }

    fn drop_rows(&self, gone: &[Target]) {
        let Some(win) = self.window() else { return };
        win.list.retain(|row| !gone.contains(&Target::from_row(row)));
        if win.conversation.read(|o| o.among(gone)) == Some(true) {
            win.conversation.clear();
        }
    }

    fn loading(&self) {
        if let Some(win) = self.window() {
            win.list.show_loading();
        }
    }

    fn select(&self, (account_id, thread_id, then): Revealed) {
        if let Some(win) = self.window() {
            win.select_revealed(account_id, thread_id, then);
        }
    }

    fn failed(&self, err: &anyhow::Error) {
        if let Some(win) = self.window() {
            win.toast(&load_failed(err));
        }
    }
}

/// The store, the servers and the main loop, as the listing run asks them.
pub(super) struct CoreLists(pub(super) Rc<Core>);

impl Lists for CoreLists {
    fn list(&self, mailbox: Mailbox, scope: Scope, view: View, held: Loaded) -> Answer<'_, anyhow::Result<Listing>> {
        let lists = self.0.lists();
        Box::pin(
            self.0
                .call(async move { lists.list(&mailbox, &scope, &view, held).await }),
        )
    }

    fn stored_search(
        &self,
        query: String,
        only: Option<AccountId>,
        scope: Scope,
        view: View,
    ) -> Answer<'_, anyhow::Result<Listing>> {
        let lists = self.0.lists();
        Box::pin(
            self.0
                .call(async move { lists.stored_search(&query, only, &scope, &view).await }),
        )
    }

    fn server_search(
        &self,
        mailbox: Mailbox,
        scope: Scope,
        view: View,
        stop: Stop,
    ) -> Answer<'_, anyhow::Result<Listing>> {
        let lists = self.0.lists();
        Box::pin(self.0.call(async move {
            lists
                .list_until(&mailbox, &scope, &view, Loaded::nothing(), &stop)
                .await
        }))
    }

    fn changed(&self, mailbox: Mailbox, named: Vec<Named>, view: View) -> Answer<'_, Option<Changed>> {
        let lists = self.0.lists();
        let call = self
            .0
            .call(async move { lists.changed(&mailbox, &named, &view).await });
        Box::pin(async move { call.await.unwrap_or(None) })
    }

    fn gone_from(&self, folder: Folder, targets: Vec<Target>) -> Answer<'_, Vec<Target>> {
        let actions = self.0.actions();
        let call = self
            .0
            .call(async move { actions.gone_from(folder, &targets).await });
        Box::pin(async move { call.await.unwrap_or_default() })
    }

    /// This never touches the window: a failure only logs, and the
    /// listing on screen, which came from the store already, stays as it
    /// is.
    fn follow(&self, account_id: AccountId, server_mailbox: String) {
        let Some(account) = self.0.account(account_id) else {
            return;
        };
        self.0.spawn(async move {
            if let Err(err) = account.follow_mailbox(&server_mailbox).await {
                tracing::warn!(
                    account = account_id,
                    mailbox = %server_mailbox,
                    error = %err,
                    "could not follow the folder opened on screen"
                );
            }
        });
    }

    fn sleep(&self, time: Duration) -> Answer<'_, ()> {
        Box::pin(glib::timeout_future(time))
    }

    fn spawn(&self, work: Work) {
        glib::spawn_future_local(work);
    }
}
