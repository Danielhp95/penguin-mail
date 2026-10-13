//! The life of the thread list, through two ports: [`Pane`] for the window
//! and its list of rows, and [`Lists`] for the store, the servers and the
//! main loop.
//!
//! The run owns the mailbox on screen with its list feed, the search
//! field's typing, the server search still out and what stops it, and the
//! folders each account was asked to follow. The window hands it events:
//! a mailbox chosen, a change event, the list scrolled to its end, a key in
//! the search field. The run decides what to list, when, and whether an
//! answer still belongs on screen once it arrives, so the window never
//! holds a ticket or a stop of its own.
//!
//! Every answer passes through the feed's ticket on its way to the list,
//! the way the thread run's answers pass through `Wanted`: an answer for a
//! mailbox that has since left the screen is dropped here.
//!
//! Nothing here touches GTK. The window is one adapter behind the ports
//! and the tests are another.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use mailrs_domain::{AccountId, Category, Folder, Target, ThreadSummary};
use mailrs_sync::{Changed, Listing, Loaded, Scope, Stop, View};

use super::on_screen::{OnScreen, Redraw, Revealed};
use crate::search::typing::{self, Reach, Run, Typed, Typing};
use crate::ui::Mailbox;
use crate::ui::list_feed::{Named, Refresh, Splice, Ticket};
pub(super) use crate::wanted::Answer;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

/// Work the run starts on the main loop and does not wait for.
pub(super) type Work = Pin<Box<dyn Future<Output = ()>>>;

/// How long a burst of change events gets to end before the list catches
/// up with it.
pub(super) const COALESCE: Duration = Duration::from_millis(150);

/// What the run reads from the window and changes in it. Every read
/// answers from what the window already holds.
pub(super) trait Pane {
    /// The accounts a listing may read, in sidebar order.
    fn scope(&self) -> Scope;
    /// The settings that change what `mailbox` lists, with `category`
    /// chosen in the inbox.
    fn view(&self, mailbox: &Mailbox, category: Category) -> View;
    /// The account and server mailbox to keep in step once `mailbox` is
    /// listed, if any.
    fn follows(&self, mailbox: &Mailbox) -> Option<(AccountId, String)>;
    /// The rows the list holds, for asking for the page after them.
    fn loaded(&self) -> Loaded;
    /// Redraws what a change to the mailbox on screen left stale, apart
    /// from the list, which the run lists itself.
    fn redraw(&self, redraw: &Redraw);
    /// A refresh is about to run: the counts and the open conversations
    /// it names catch up too.
    fn refreshing(&self, refresh: &Refresh);
    /// Puts a first page on screen in place of the rows there.
    fn set_rows(&self, listing: Listing);
    /// Adds a page of older rows at the end.
    fn append(&self, rows: Vec<ThreadSummary>);
    /// Puts `rows` in place of the rows of the `changed` threads, and
    /// `title` and `subtitle` over the list.
    fn replace(&self, changed: &[Named], rows: Vec<ThreadSummary>, title: &str, subtitle: &str);
    /// Drops the rows of these targets, which left the folder on screen.
    fn drop_rows(&self, gone: &[Target]);
    /// Shows the spinner in place of the rows.
    fn loading(&self);
    /// Selects a thread a reveal waited on, now that its row is listed.
    fn select(&self, revealed: Revealed);
    /// Says that listing failed.
    fn failed(&self, err: &anyhow::Error);
}

/// What the run asks of the store, the servers and the main loop.
pub(super) trait Lists {
    /// A page of `mailbox`, after the rows in `held`.
    fn list(&self, mailbox: Mailbox, scope: Scope, view: View, held: Loaded)
    -> Answer<'_, anyhow::Result<Listing>>;
    /// The stored mail that matches a search.
    fn stored_search(
        &self,
        query: String,
        only: Option<AccountId>,
        scope: Scope,
        view: View,
    ) -> Answer<'_, anyhow::Result<Listing>>;
    /// The first page of a server search, which `stop` cuts short.
    fn server_search(
        &self,
        mailbox: Mailbox,
        scope: Scope,
        view: View,
        stop: Stop,
    ) -> Answer<'_, anyhow::Result<Listing>>;
    /// The rows of the named threads as they are now, or nothing when the
    /// mailbox has to be listed again.
    fn changed(&self, mailbox: Mailbox, named: Vec<Named>, view: View) -> Answer<'_, Option<Changed>>;
    /// Which of `targets` left `folder`.
    fn gone_from(&self, folder: Folder, targets: Vec<Target>) -> Answer<'_, Vec<Target>>;
    /// Keeps a server mailbox of an account in step from now on.
    fn follow(&self, account_id: AccountId, server_mailbox: String);
    fn sleep(&self, time: Duration) -> Answer<'_, ()>;
    /// Runs `work` on the main loop, beside whatever else runs.
    fn spawn(&self, work: Work);
}

/// What the list shows while the first page is on its way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waiting {
    /// A mailbox just put on screen: its rows are not there yet.
    Spinner,
    /// The mailbox on screen listed again after a change: its rows stay
    /// until the new ones replace them, rather than blinking out after
    /// every action.
    KeepRows,
}

struct State {
    screen: OnScreen,
    /// The search field between keys.
    typing: Typing,
    /// The search the field last ran, which says how far the listing of
    /// that search reaches.
    searched: Option<Run>,
    /// The server search in flight and the ticket it lists under, stopped
    /// once a key or a newer search makes its answer stale.
    stop: Option<(Ticket, Stop)>,
    /// The server mailboxes each account was already asked to follow, so
    /// a reload does not fetch a folder's window again: the slow poll
    /// covers it from the first time.
    followed: HashMap<AccountId, HashSet<String>>,
}

/// The thread list's run. See the module's comment.
pub(super) struct ListingRun {
    state: RefCell<State>,
    pane: Rc<dyn Pane>,
    lists: Rc<dyn Lists>,
}

impl ListingRun {
    /// The inbox, split into `category` when categories are on. Nothing is
    /// listed until a mailbox is shown.
    pub(super) fn new(category: Category, pane: Rc<dyn Pane>, lists: Rc<dyn Lists>) -> ListingRun {
        ListingRun {
            state: RefCell::new(State {
                screen: OnScreen::new(category),
                typing: Typing::default(),
                searched: None,
                stop: None,
                followed: HashMap::new(),
            }),
            pane,
            lists,
        }
    }

    /// The mailbox on screen.
    pub(super) fn mailbox(&self) -> Mailbox {
        self.state.borrow().screen.mailbox().clone()
    }

    /// The inbox category chosen.
    pub(super) fn category(&self) -> Category {
        self.state.borrow().screen.category()
    }

    /// Makes one change to the mailbox on screen and carries out what it
    /// left stale.
    pub(super) fn change(self: &Rc<Self>, change: impl FnOnce(&mut OnScreen) -> Option<Redraw>) {
        if let Some(redraw) = self.transition(change) {
            self.carry(redraw);
        }
    }

    /// Makes one change to the mailbox on screen and hands back what it
    /// left stale, for a caller that has to redraw other parts first. It
    /// passes the answer to [`ListingRun::carry`].
    pub(super) fn transition(&self, change: impl FnOnce(&mut OnScreen) -> Option<Redraw>) -> Option<Redraw> {
        change(&mut self.state.borrow_mut().screen)
    }

    /// Redraws what a change left stale and lists the first page it asks
    /// for.
    pub(super) fn carry(self: &Rc<Self>, redraw: Redraw) {
        self.pane.redraw(&redraw);
        if let Some(ticket) = redraw.list {
            self.first_page(ticket, Waiting::Spinner);
        }
    }

    /// Lists the mailbox on screen again, leaving its rows up until the
    /// new ones land.
    pub(super) fn reload(self: &Rc<Self>) {
        let ticket = self.state.borrow_mut().screen.feed().reload();
        self.first_page(ticket, Waiting::KeepRows);
    }

    /// Lists a folder, a smart mailbox or a search, which the servers
    /// answer, again.
    pub(super) fn reload_remote(self: &Rc<Self>) {
        if self.mailbox().is_remote() {
            self.reload();
        }
    }

    /// The store changed these threads. No threads means it cannot say
    /// which.
    pub(super) fn changed(self: &Rc<Self>, threads: Vec<Named>) {
        let coalesce = self.state.borrow_mut().screen.feed().changed(threads);
        self.coalesce(coalesce);
    }

    /// Something changed that re-reading single threads cannot catch,
    /// such as new mail or an action on many threads.
    pub(super) fn everything(self: &Rc<Self>) {
        let coalesce = self.state.borrow_mut().screen.feed().everything();
        self.coalesce(coalesce);
    }

    /// Waits [`COALESCE`] so that a burst of change events costs one
    /// refresh. The feed decides whether that refresh lists the mailbox
    /// again or re-reads the named threads on their own, which keeps a
    /// change event off the 10,000-row query.
    fn coalesce(self: &Rc<Self>, coalesce: crate::ui::list_feed::Coalesce) {
        if coalesce == crate::ui::list_feed::Coalesce::Joined {
            return;
        }
        let this = Rc::clone(self);
        self.lists.spawn(Box::pin(async move {
            this.lists.sleep(COALESCE).await;
            let refresh = this.state.borrow_mut().screen.feed().fire();
            this.pane.refreshing(&refresh);
            match refresh {
                Refresh::Reload => this.reload(),
                Refresh::Splice(ticket, named) => this.splice(ticket, named).await,
            }
        }));
    }

    /// The reader scrolled near the end of the list: loads the next page,
    /// starting after the last row on screen rather than after as many
    /// rows as the list holds, so mail that arrived or left since neither
    /// repeats a row nor skips one.
    pub(super) fn scrolled_to_end(self: &Rc<Self>) {
        let Some(ticket) = self.state.borrow_mut().screen.feed().scrolled_to_end() else {
            return;
        };
        let mailbox = self.mailbox();
        let (scope, view) = self.asks(&mailbox);
        let held = self.pane.loaded();
        let this = Rc::clone(self);
        self.lists.spawn(Box::pin(async move {
            let loaded = this.lists.list(mailbox, scope, view, held).await;
            if !this.state.borrow_mut().screen.feed().next_page(ticket, &loaded) {
                return;
            }
            match loaded {
                Ok(listing) => this.pane.append(listing.rows),
                Err(err) => this.pane.failed(&err),
            }
        }));
    }

    /// Drops the rows of `targets` that no longer belong in the folder on
    /// screen. The store cannot list these folders, so rows go one by one.
    pub(super) fn prune(self: &Rc<Self>, targets: Vec<Target>) {
        let mailbox = self.mailbox();
        let Some(folder) = mailbox.folder() else {
            return;
        };
        let this = Rc::clone(self);
        self.lists.spawn(Box::pin(async move {
            let gone = this.lists.gone_from(folder, targets).await;
            if !gone.is_empty() && this.mailbox() == mailbox {
                this.pane.drop_rows(&gone);
            }
        }));
    }

    /// Opens a thread from outside the window, such as a notification. It
    /// lives in the inbox, which comes on screen first; the thread is
    /// selected at once when its row can be, and otherwise once the
    /// inbox's first page lands.
    pub(super) fn reveal(self: &Rc<Self>, thread: Revealed) {
        let (redraw, now) = self.state.borrow_mut().screen.reveal(thread);
        if let Some(redraw) = redraw {
            self.carry(redraw);
        }
        if let Some(thread) = now {
            self.pane.select(thread);
        }
    }

    /// The accounts were read again: forget what was followed for the
    /// ones that went.
    pub(super) fn accounts_live(&self, live: &HashSet<AccountId>) {
        self.state
            .borrow_mut()
            .followed
            .retain(|id, _| live.contains(id));
    }

    // ---- The search field ------------------------------------------------

    /// The text in the search field changed to `text`. A pause in typing
    /// runs the search; an empty field puts the mailbox back.
    pub(super) fn typed(self: &Rc<Self>, text: &str) {
        let typed = {
            let mut state = self.state.borrow_mut();
            // The answer to the old text is no longer wanted.
            if let Some((_, stop)) = state.stop.take() {
                stop.stop();
                state.typing.interrupted();
            }
            state.typing.typed(text)
        };
        match typed {
            Typed::Cleared => self.change(OnScreen::search_closed),
            Typed::Wait(pause) => {
                let this = Rc::clone(self);
                self.lists.spawn(Box::pin(async move {
                    this.lists.sleep(typing::PAUSE).await;
                    let run = this.state.borrow_mut().typing.paused(pause);
                    if let Some(run) = run {
                        this.run_search(run);
                    }
                }));
            }
        }
    }

    /// Enter in the search field, or a picked suggestion: runs `text` at
    /// once, as far as the servers.
    pub(super) fn entered(self: &Rc<Self>, text: &str) {
        let run = self.state.borrow_mut().typing.entered(text);
        if let Some(run) = run {
            self.run_search(run);
        }
    }

    /// The search bar closed: the server search still out stops, and a
    /// search on screen gives way to the mailbox it started from.
    pub(super) fn search_bar_closed(self: &Rc<Self>) {
        let stop = {
            let mut state = self.state.borrow_mut();
            state.typing.closed();
            state.stop.take()
        };
        if let Some((_, stop)) = stop {
            stop.stop();
        }
        self.change(OnScreen::search_closed);
    }

    /// Searches for `query` in the account on screen, or in every account
    /// when the mailbox spans them.
    pub(super) fn search(self: &Rc<Self>, query: String) {
        self.change(|screen| Some(screen.search(query)));
    }

    fn run_search(self: &Rc<Self>, run: Run) {
        let query = run.query.clone();
        self.state.borrow_mut().searched = Some(run);
        self.search(query);
    }

    // ---- Listing ---------------------------------------------------------

    /// What a listing of `mailbox` reads with.
    fn asks(&self, mailbox: &Mailbox) -> (Scope, View) {
        (self.pane.scope(), self.pane.view(mailbox, self.category()))
    }

    /// Lists the first page under `ticket`, which the feed dropped all
    /// earlier requests for.
    fn first_page(self: &Rc<Self>, ticket: Ticket, waiting: Waiting) {
        let mailbox = self.mailbox();
        if let Some((account_id, server_mailbox)) = self.pane.follows(&mailbox) {
            let newly = newly_followed(
                &mut self.state.borrow_mut().followed,
                account_id,
                server_mailbox.clone(),
            );
            if newly {
                self.lists.follow(account_id, server_mailbox);
            }
        }
        if let Mailbox::Search { query, account_id } = &mailbox {
            let spinner = waiting == Waiting::Spinner;
            return self.search_first_page(ticket, query.clone(), *account_id, spinner);
        }
        if mailbox.is_remote() && waiting == Waiting::Spinner {
            self.pane.loading();
        }
        let (scope, view) = self.asks(&mailbox);
        let this = Rc::clone(self);
        self.lists.spawn(Box::pin(async move {
            let loaded = this.lists.list(mailbox, scope, view, Loaded::nothing()).await;
            this.land(ticket, loaded);
        }));
    }

    /// Lists the first page of a search under `ticket`: the stored mail
    /// first, then, when the search reaches them, the servers' answer in
    /// its place. A search the field did not run, such as one the
    /// assistant opened, reaches the servers. With no stored rows to show,
    /// `spinner` puts the spinner up while the servers answer; a refresh
    /// leaves the rows on screen instead.
    fn search_first_page(
        self: &Rc<Self>,
        ticket: Ticket,
        query: String,
        only: Option<AccountId>,
        spinner: bool,
    ) {
        let stop = Stop::default();
        let (reach, older) = {
            let mut state = self.state.borrow_mut();
            let reach = match &state.searched {
                Some(run) if run.query == query => run.reach,
                _ => Reach::Servers,
            };
            let held = (reach == Reach::Servers).then(|| (ticket, stop.clone()));
            (reach, std::mem::replace(&mut state.stop, held))
        };
        if let Some((_, older)) = older {
            older.stop();
        }
        let mailbox = Mailbox::Search {
            query: query.clone(),
            account_id: only,
        };
        let (scope, view) = self.asks(&mailbox);
        let this = Rc::clone(self);
        self.lists.spawn(Box::pin(async move {
            let stored = this
                .lists
                .stored_search(query, only, scope.clone(), view.clone())
                .await;
            if reach == Reach::Store {
                return this.land(ticket, stored);
            }
            if stop.stopped() || !this.state.borrow_mut().screen.feed().current(ticket) {
                return;
            }
            match stored {
                Ok(found) if !found.rows.is_empty() => this.pane.set_rows(found),
                Err(err) => tracing::warn!(error = %err, "could not search the stored mail"),
                Ok(_) if spinner => this.pane.loading(),
                Ok(_) => {}
            }
            let found = this
                .lists
                .server_search(mailbox, scope, view, stop.clone())
                .await;
            {
                let mut state = this.state.borrow_mut();
                if state.stop.as_ref().is_some_and(|(at, _)| *at == ticket) {
                    state.stop = None;
                }
            }
            // A stopped search answers with what it had, which is less than
            // the stored rows already on screen.
            if !stop.stopped() {
                this.land(ticket, found);
            }
        }));
    }

    /// Puts a first page on screen while `ticket` is current, and selects
    /// the thread a reveal waited on.
    fn land(&self, ticket: Ticket, loaded: anyhow::Result<Listing>) {
        let landed = self.state.borrow_mut().screen.feed().first_page(ticket, &loaded);
        let Some(landed) = landed else {
            return;
        };
        match loaded {
            Ok(listing) => self.pane.set_rows(listing),
            Err(err) => self.pane.failed(&err),
        }
        if let Some(thread) = landed.reveal {
            self.pane.select(thread);
        }
    }

    /// Re-reads the threads a change event named and puts them back in the
    /// list in place, instead of listing the whole mailbox again.
    async fn splice(self: &Rc<Self>, ticket: Ticket, named: Vec<Named>) {
        let mailbox = self.mailbox();
        let (_, view) = self.asks(&mailbox);
        let fresh = self.lists.changed(mailbox, named.clone(), view).await;
        let mailbox = self.mailbox();
        let prunes = mailbox.folder().is_some();
        let splice = self.state.borrow_mut().screen.feed().spliced(ticket, fresh, prunes);
        match splice {
            Splice::Stale => {}
            Splice::Put(fresh) => {
                let held = self.pane.loaded();
                let rows = self.state.borrow_mut().screen.feed().place(fresh.rows, &held);
                self.pane
                    .replace(&named, rows, &mailbox.title(), &fresh.subtitle);
            }
            // A folder lists through the server. The store says which
            // changed rows left it, so they go at once; mail moved into it
            // shows once the action that moved it is done (see
            // `Aftermath`), when the server has it.
            Splice::Prune => {
                let targets = named
                    .iter()
                    .map(|(account_id, thread_id)| Target::thread(*account_id, thread_id))
                    .collect();
                self.prune(targets);
            }
            Splice::Reload => self.reload(),
        }
    }
}

/// Whether asking to follow `server_mailbox` of `account_id` needs to
/// reach the account at all: `false` once `followed` already holds the
/// pair, so a reload of the folder on screen does not fetch its window
/// again. The slow poll covers it from the first time.
fn newly_followed(
    followed: &mut HashMap<AccountId, HashSet<String>>,
    account_id: AccountId,
    server_mailbox: String,
) -> bool {
    followed.entry(account_id).or_default().insert(server_mailbox)
}
