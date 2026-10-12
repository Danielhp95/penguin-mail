//! The unread counts: what the sidebar, the space badge, the tray and the
//! Dock badge show. The app counts once per burst of changes, with one
//! grouped read of the store, and hands the same answer to each of them,
//! so the tray never says something the sidebar does not.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use mailrs_domain::AccountId;
use mailrs_sync::mailbox::{Counts, Mailbox, Standard, View};

/// All Inboxes, whose count the tray, the Dock and the space badge show.
pub const ALL_INBOXES: Mailbox = Mailbox::Unified(Standard::Inbox);

/// How long a burst may run before the tray and the Dock count again
/// while no window shows the counts. Sync reports each batch of changed
/// threads on its own, and a new account's first sync sends hundreds.
pub const QUIET: Duration = Duration::from_millis(500);

/// What the window shows besides the inboxes: its sidebar's mailboxes,
/// and the mailbox on screen with the view that shapes its categories.
pub struct Wanted {
    pub mailboxes: Vec<Mailbox>,
    pub shown: Mailbox,
    pub view: View,
}

/// One count to make: every mailbox anyone shows, and whether the
/// servers may be asked again for theirs.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub mailboxes: Vec<Mailbox>,
    pub shown: Mailbox,
    pub view: View,
    /// False reads only the server answers already kept, so a count woken
    /// by a server answer while no window is open starts no polling.
    pub refresh: bool,
}

/// What reads the counts: `Lists::counts` in the app.
pub trait Counter {
    async fn count(&self, ask: Ask) -> Result<Counts, String>;
}

/// `ask` read from `lists`, on the runtime that serves them.
pub async fn read<A: mailrs_sync::Accounts>(
    lists: &mailrs_sync::Mailboxes<A>,
    ask: Ask,
) -> Result<Counts, mailrs_sync::SyncError> {
    let Ask {
        mailboxes,
        shown,
        view,
        refresh,
    } = ask;
    match refresh {
        true => lists.counts(&mailboxes, &shown, &view).await,
        false => lists.cached_counts(&mailboxes, &shown, &view).await,
    }
}

/// The window, while it shows counts. `wanted` gives `None` once the
/// window has gone, and the counts stop asking it.
pub trait Shows {
    fn wanted(&self) -> Option<Wanted>;
    fn show(&self, counted: &Counted);
}

/// One finished count, as every subscriber sees it.
#[derive(Debug, Default)]
pub struct Counted {
    pub counts: Counts,
}

impl Counted {
    /// The count of `mailbox`, zero when nobody asked for it.
    pub fn of(&self, mailbox: &Mailbox) -> i64 {
        self.counts.mailboxes.get(mailbox).copied().unwrap_or(0)
    }

    /// Unread mail in All Inboxes.
    pub fn all_inboxes(&self) -> i64 {
        self.of(&ALL_INBOXES)
    }

    /// Unread mail in one account's inbox.
    pub fn inbox(&self, account_id: AccountId) -> i64 {
        self.of(&Mailbox::Standard {
            account_id,
            which: Standard::Inbox,
        })
    }
}

/// Something that hears every count, such as the tray.
pub type Listener = Box<dyn Fn(&Counted)>;

/// The counts and who hears of them. `changed` and `answered` ask for a
/// count; the run started by `start` makes one per burst and publishes it.
pub struct Unread<C> {
    counter: C,
    accounts: RefCell<Vec<AccountId>>,
    window: RefCell<Option<Box<dyn Shows>>>,
    listeners: RefCell<Vec<Listener>>,
    /// A count is wanted; true when it may ask the servers again.
    asked: Cell<Option<bool>>,
    wake: Rc<tokio::sync::Notify>,
}

impl<C: Counter + 'static> Unread<C> {
    pub fn new(counter: C) -> Rc<Self> {
        Rc::new(Unread {
            counter,
            accounts: RefCell::default(),
            window: RefCell::default(),
            listeners: RefCell::default(),
            asked: Cell::new(None),
            wake: Rc::default(),
        })
    }

    /// Runs the counts until the last handle goes. `pause` waits on the
    /// main loop: a zero pause lets the rest of a burst ask first.
    pub fn start<P: Future<Output = ()> + 'static>(
        self: &Rc<Self>,
        pause: impl Fn(Duration) -> P + 'static,
    ) -> impl Future<Output = ()> + 'static {
        let (weak, wake) = (Rc::downgrade(self), Rc::clone(&self.wake));
        run(weak, wake, pause)
    }

    /// The accounts whose inboxes the tray and the Dock show.
    pub fn set_accounts(&self, accounts: Vec<AccountId>) {
        *self.accounts.borrow_mut() = accounts;
        self.changed();
    }

    /// The window now shows counts, and hears each one. It replaces any
    /// window before it.
    pub fn show_in(&self, window: Box<dyn Shows>) {
        *self.window.borrow_mut() = Some(window);
        self.changed();
    }

    /// Hears every count from here on.
    pub fn listen(&self, listener: Listener) {
        self.listeners.borrow_mut().push(listener);
    }

    /// Mail or a setting changed what the counts say.
    pub fn changed(&self) {
        self.ask(true);
    }

    /// A server's count arrived or grew old. Only an open window asks the
    /// servers again; the tray and the Dock read what is kept.
    pub fn answered(&self) {
        self.ask(self.window.borrow().is_some());
    }

    fn ask(&self, refresh: bool) {
        let before = self.asked.get().unwrap_or(false);
        self.asked.set(Some(before || refresh));
        self.wake.notify_one();
    }

    /// Every mailbox anyone shows: each account's inbox and All Inboxes,
    /// then the window's. `None` when nobody asked since the last count.
    fn take(&self) -> Option<Ask> {
        let refresh = self.asked.take()?;
        let mut mailboxes: Vec<Mailbox> = self
            .accounts
            .borrow()
            .iter()
            .map(|&account_id| Mailbox::Standard {
                account_id,
                which: Standard::Inbox,
            })
            .collect();
        mailboxes.push(ALL_INBOXES);
        let wanted = self.window.borrow().as_ref().map(|w| w.wanted());
        let (shown, view) = match wanted {
            Some(Some(wanted)) => {
                for mailbox in wanted.mailboxes {
                    if !mailboxes.contains(&mailbox) {
                        mailboxes.push(mailbox);
                    }
                }
                (wanted.shown, wanted.view)
            }
            Some(None) => {
                self.window.borrow_mut().take();
                (ALL_INBOXES, View::default())
            }
            None => (ALL_INBOXES, View::default()),
        };
        Some(Ask {
            mailboxes,
            shown,
            view,
            refresh,
        })
    }

    fn publish(&self, counted: Counted) {
        // A listener may ask for another count, which borrows nothing here.
        if let Some(window) = self.window.borrow().as_ref() {
            window.show(&counted);
        }
        for listener in self.listeners.borrow().iter() {
            listener(&counted);
        }
    }
}

async fn run<C: Counter + 'static, P: Future<Output = ()>>(
    weak: Weak<Unread<C>>,
    wake: Rc<tokio::sync::Notify>,
    pause: impl Fn(Duration) -> P,
) {
    loop {
        wake.notified().await;
        // Whatever else this turn of the main loop asks joins this count.
        pause(Duration::ZERO).await;
        let Some(unread) = weak.upgrade() else { return };
        let Some(ask) = unread.take() else { continue };
        match unread.counter.count(ask).await {
            Ok(counts) => unread.publish(Counted { counts }),
            Err(error) => tracing::warn!(%error, "could not count unread mail"),
        }
        // With no window open, nobody watches the counts closely, so a
        // burst of sync events waits a little between counts.
        let quiet = unread.window.borrow().is_none();
        drop(unread);
        if quiet {
            pause(QUIET).await;
        }
    }
}

#[cfg(test)]
mod tests;
