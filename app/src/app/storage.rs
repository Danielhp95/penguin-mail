//! Server-space reads outlive windows and publish each account's latest answer.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use futures::future::{AbortHandle, Abortable, LocalBoxFuture};
use futures::stream::{FuturesUnordered, StreamExt};
use futures::{FutureExt, select_biased};
use gtk::glib;
use mailrs_domain::{AccountId, AccountState, ChangeEvent};
use mailrs_sync::Storage;

use super::App;
use crate::core::Core;
use crate::ui::window::Notice;

const FIRST_LOOK: Duration = Duration::from_secs(10);
const DEMO_FIRST_LOOK: Duration = Duration::from_secs(1);
const EVERY: Duration = Duration::from_secs(60 * 60);
const READ_WAIT: Duration = Duration::from_secs(60);

type Answer = Result<Option<Storage>, ()>;

pub(super) trait Reads {
    fn read(&self, account_id: AccountId) -> LocalBoxFuture<'static, Answer>;
}

pub(super) struct CoreStorage(pub(super) Rc<Core>);

impl Reads for CoreStorage {
    fn read(&self, account_id: AccountId) -> LocalBoxFuture<'static, Answer> {
        let core = Rc::clone(&self.0);
        async move {
            let sync = core.account(account_id).ok_or(())?;
            // Core's worker continues if its caller goes away. Bound that work
            // too, so canceling a superseded read leaves no unbounded task.
            core.call(async move {
                match tokio::time::timeout(READ_WAIT, sync.storage()).await {
                    Ok(read) => read.map_err(anyhow::Error::from),
                    Err(_) => Err(anyhow::anyhow!("no answer in {} seconds", READ_WAIT.as_secs())),
                }
            })
            .await
            .map_err(|err| {
                tracing::info!(account = account_id, error = %err, "could not read the account's storage");
            })
        }
        .boxed_local()
    }
}

#[derive(Default)]
struct State {
    accounts: HashSet<AccountId>,
    first_pass_done: bool,
    found: HashMap<AccountId, Storage>,
    versions: HashMap<AccountId, u64>,
    next_version: u64,
    queued: HashSet<AccountId>,
    active: HashMap<AccountId, AbortHandle>,
}

/// Owns scheduling, current accounts and publication, including answers that
/// arrive after another read or after the account goes away.
pub(super) struct ServerSpace<R> {
    reader: R,
    state: RefCell<State>,
    wake: Rc<tokio::sync::Notify>,
    publisher: RefCell<Option<Box<dyn Fn()>>>,
}

impl<R: Reads + 'static> ServerSpace<R> {
    pub(super) fn new(reader: R) -> Rc<Self> {
        Rc::new(Self {
            reader,
            state: RefCell::default(),
            wake: Rc::default(),
            publisher: RefCell::default(),
        })
    }

    pub(super) fn set_accounts(&self, accounts: Vec<AccountId>) {
        let mut state = self.state.borrow_mut();
        let accounts: HashSet<_> = accounts.into_iter().collect();
        let added: Vec<_> = if state.first_pass_done {
            accounts.difference(&state.accounts).copied().collect()
        } else {
            Vec::new()
        };
        state.accounts = accounts;
        let accounts = state.accounts.clone();
        state.versions.retain(|id, _| accounts.contains(id));
        state.queued.retain(|id| accounts.contains(id));
        state.active.retain(|id, read| {
            if accounts.contains(id) {
                true
            } else {
                read.abort();
                false
            }
        });
        let before = state.found.len();
        state.found.retain(|id, _| accounts.contains(id));
        let changed = before != state.found.len();
        drop(state);
        if changed {
            self.publish();
        }
        for id in added {
            self.request(id);
        }
    }

    pub(super) fn heard(&self, event: &ChangeEvent) {
        if let ChangeEvent::AccountStateChanged {
            account_id,
            state: AccountState::Ok,
        } = event
        {
            self.request(*account_id);
        }
    }

    fn request(&self, id: AccountId) {
        let mut state = self.state.borrow_mut();
        if !state.accounts.contains(&id) {
            return;
        }
        state.next_version += 1;
        let version = state.next_version;
        state.versions.insert(id, version);
        state.queued.insert(id);
        if let Some(read) = state.active.remove(&id) {
            read.abort();
        }
        self.wake.notify_one();
    }

    fn snapshot(&self) -> HashMap<AccountId, Storage> {
        self.state.borrow().found.clone()
    }

    fn publish(&self) {
        if let Some(publish) = self.publisher.borrow().as_ref() {
            publish();
        }
    }

    fn accept(&self, id: AccountId, version: u64, answer: Answer) {
        let mut state = self.state.borrow_mut();
        if state.versions.get(&id) != Some(&version) || !state.accounts.contains(&id) {
            return;
        }
        state.active.remove(&id);
        let changed = match answer {
            Ok(Some(storage)) => state.found.insert(id, storage).as_ref() != Some(&storage),
            Ok(None) => state.found.remove(&id).is_some(),
            Err(()) => false,
        };
        drop(state);
        if changed {
            self.publish();
        }
    }

    fn start<P: Future<Output = ()> + 'static>(
        self: &Rc<Self>,
        first: Duration,
        pause: impl Fn(Duration) -> P + 'static,
        publish: impl Fn() + 'static,
    ) -> impl Future<Output = ()> + 'static {
        enum Finished {
            Tick,
            Read(AccountId, u64, Answer),
            Canceled,
        }
        *self.publisher.borrow_mut() = Some(Box::new(publish));
        let weak = Rc::downgrade(self);
        let wake = Rc::clone(&self.wake);
        let pause = Rc::new(pause);
        async move {
            let mut jobs: FuturesUnordered<LocalBoxFuture<'static, Finished>> =
                FuturesUnordered::new();
            let timer = Rc::clone(&pause);
            jobs.push(
                async move {
                    timer(first).await;
                    Finished::Tick
                }
                .boxed_local(),
            );
            loop {
                let Some(space) = weak.upgrade() else { return };
                let queued: Vec<_> = space.state.borrow_mut().queued.drain().collect();
                for id in queued {
                    let mut state = space.state.borrow_mut();
                    let version = state.versions[&id];
                    let (abort, canceled) = AbortHandle::new_pair();
                    state.active.insert(id, abort);
                    drop(state);
                    let read = space.reader.read(id);
                    let timer = Rc::clone(&pause);
                    jobs.push(
                        async move {
                            let read = async move {
                                let answer =
                                    futures::future::select(read, timer(READ_WAIT).boxed_local())
                                        .await;
                                match answer {
                                    futures::future::Either::Left((answer, _)) => answer,
                                    futures::future::Either::Right(_) => Err(()),
                                }
                            };
                            match Abortable::new(read, canceled).await {
                                Ok(answer) => Finished::Read(id, version, answer),
                                Err(_) => Finished::Canceled,
                            }
                        }
                        .boxed_local(),
                    );
                }
                drop(space);
                select_biased! {
                    _ = wake.notified().fuse() => {},
                    finished = jobs.next().fuse() => {
                        let Some(space) = weak.upgrade() else { return };
                        match finished {
                            Some(Finished::Tick) => {
                                space.state.borrow_mut().first_pass_done = true;
                                let accounts: Vec<_> = space.state.borrow().accounts.iter().copied().collect();
                                for id in accounts { space.request(id); }
                                let timer = Rc::clone(&pause);
                                jobs.push(async move { timer(EVERY).await; Finished::Tick }.boxed_local());
                            }
                            Some(Finished::Read(id, version, answer)) => space.accept(id, version, answer),
                            Some(Finished::Canceled) | None => {},
                        }
                    }
                }
            }
        }
    }
}

impl App {
    pub(super) fn start_storage_checks(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let first = if self.core.demo {
            DEMO_FIRST_LOOK
        } else {
            FIRST_LOOK
        };
        glib::spawn_future_local(self.storage.start(first, glib::timeout_future, move || {
            if let Some(app) = weak.upgrade() {
                app.tell_window(Notice::StorageChanged);
            }
        }));
    }

    /// How full each account's server said it was at the last successful read.
    pub fn storage(&self) -> HashMap<AccountId, Storage> {
        self.storage.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;

    struct Requested {
        id: AccountId,
        reply: async_channel::Sender<Answer>,
    }

    struct Reader(async_channel::Sender<Requested>);

    impl Reads for Reader {
        fn read(&self, id: AccountId) -> LocalBoxFuture<'static, Answer> {
            let (reply, answer) = async_channel::bounded(1);
            self.0.try_send(Requested { id, reply }).unwrap();
            async move { answer.recv().await.unwrap() }.boxed_local()
        }
    }

    struct Harness {
        space: Rc<ServerSpace<Reader>>,
        run: Pin<Box<dyn Future<Output = ()>>>,
        requests: async_channel::Receiver<Requested>,
        shown: Rc<RefCell<Vec<HashMap<AccountId, Storage>>>>,
    }

    impl Harness {
        fn new() -> Self {
            let (send, requests) = async_channel::unbounded();
            let space = ServerSpace::new(Reader(send));
            space.set_accounts(vec![1, 2]);
            let shown = Rc::new(RefCell::new(Vec::new()));
            let seen = Rc::clone(&shown);
            let weak = Rc::downgrade(&space);
            let run = Box::pin(space.start(FIRST_LOOK, tokio::time::sleep, move || {
                seen.borrow_mut().push(weak.upgrade().unwrap().snapshot());
            }));
            Self {
                space,
                run,
                requests,
                shown,
            }
        }

        fn step(&mut self) {
            let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
            assert!(self.run.as_mut().poll(&mut context).is_pending());
        }

        fn connected(&mut self, id: AccountId) -> Requested {
            self.space.heard(&ChangeEvent::AccountStateChanged {
                account_id: id,
                state: AccountState::Ok,
            });
            self.step();
            let request = self.requests.try_recv().unwrap();
            assert_eq!(request.id, id);
            request
        }
    }

    fn used(n: u64) -> Storage {
        Storage::new(n, 100, None).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn startup_publishes_fast_accounts_before_slow_ones() {
        let mut h = Harness::new();
        h.step();
        tokio::time::advance(FIRST_LOOK).await;
        h.step();
        let a = h.requests.try_recv().unwrap();
        let b = h.requests.try_recv().unwrap();
        let (fast, slow) = if a.id == 1 { (a, b) } else { (b, a) };
        fast.reply.try_send(Ok(Some(used(95)))).unwrap();
        h.step();
        assert_eq!(*h.shown.borrow(), vec![HashMap::from([(1, used(95))])]);
        slow.reply.try_send(Ok(Some(used(20)))).unwrap();
        h.step();
        assert_eq!(
            h.space.snapshot(),
            HashMap::from([(1, used(95)), (2, used(20))])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_connection_supersedes_an_older_success() {
        let mut h = Harness::new();
        let old = h.connected(1);
        old.reply.try_send(Ok(Some(used(10)))).unwrap();
        let new = h.connected(1);
        new.reply.try_send(Ok(Some(used(95)))).unwrap();
        h.step();
        assert_eq!(*h.shown.borrow(), vec![HashMap::from([(1, used(95))])]);
    }

    #[tokio::test(start_paused = true)]
    async fn removal_drops_cached_space_and_late_answers_even_after_readding() {
        let mut h = Harness::new();
        h.connected(1).reply.try_send(Ok(Some(used(95)))).unwrap();
        h.step();
        let old = h.connected(1);
        old.reply.try_send(Ok(Some(used(10)))).unwrap();
        h.space.set_accounts(vec![2]);
        h.space.set_accounts(vec![1, 2]);
        h.step();
        assert_eq!(
            *h.shown.borrow(),
            vec![HashMap::from([(1, used(95))]), HashMap::new()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_and_timed_out_reads_keep_the_last_answer_but_unknown_removes_it() {
        let mut h = Harness::new();
        h.connected(1).reply.try_send(Ok(Some(used(95)))).unwrap();
        h.step();
        h.connected(1).reply.try_send(Err(())).unwrap();
        h.step();
        // Let the startup pass finish before testing a later connection's timeout.
        tokio::time::advance(FIRST_LOOK).await;
        h.step();
        while let Ok(request) = h.requests.try_recv() {
            request.reply.try_send(Err(())).unwrap();
        }
        h.step();
        let pending = h.connected(1);
        tokio::time::advance(READ_WAIT).await;
        h.step();
        assert_eq!(h.space.snapshot(), HashMap::from([(1, used(95))]));
        assert!(pending.reply.is_closed());
        h.connected(1).reply.try_send(Ok(None)).unwrap();
        h.step();
        assert!(h.space.snapshot().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn hourly_reads_use_the_current_accounts() {
        let mut h = Harness::new();
        h.step();
        tokio::time::advance(FIRST_LOOK).await;
        h.step();
        for _ in 0..2 {
            h.requests
                .try_recv()
                .unwrap()
                .reply
                .try_send(Ok(Some(used(10))))
                .unwrap();
        }
        h.step();
        h.space.set_accounts(vec![2]);
        tokio::time::advance(EVERY).await;
        h.step();
        let next = h.requests.try_recv().unwrap();
        assert_eq!(next.id, 2);
        next.reply.try_send(Ok(Some(used(95)))).unwrap();
        h.step();
        assert_eq!(h.space.snapshot(), HashMap::from([(2, used(95))]));
    }
    #[tokio::test(start_paused = true)]
    async fn an_account_added_after_the_first_pass_reads_even_if_connection_arrived_first() {
        let mut h = Harness::new();
        h.space.set_accounts(Vec::new());
        h.step();
        tokio::time::advance(FIRST_LOOK).await;
        h.step();
        h.space.heard(&ChangeEvent::AccountStateChanged {
            account_id: 3,
            state: AccountState::Ok,
        });
        h.space.set_accounts(vec![3]);
        h.step();
        let request = h
            .requests
            .try_recv()
            .expect("a newly added account must not wait an hour");
        assert_eq!(request.id, 3);
        request.reply.try_send(Ok(Some(used(95)))).unwrap();
        h.step();
        assert_eq!(h.space.snapshot(), HashMap::from([(3, used(95))]));
    }
}
