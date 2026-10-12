//! The way an account tells the window what changed. The channel to the
//! window is bounded, and a window that stops reading for a while (its
//! thread busy, a dialog open) must not make the engine pile events up
//! without limit, or lose one.
//!
//! Most events are hints to read the store again: some threads changed,
//! some mail arrived, labels changed. Two hints of one kind for one account
//! merge into one carrying both sets of ids, so a burst of any length waits
//! as a few entries. The rest, a failed write or a state change, each say
//! something once and keep their order, however alike; there are few of
//! them.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use mailrs_domain::ChangeEvent;

/// How many events the channel to the window holds before the rest wait,
/// merged, in the sender.
pub const CHANNEL_EVENTS: usize = 256;

#[derive(Default)]
struct Waiting {
    events: VecDeque<Entry>,
    /// Whether a task is handing `events` to the channel.
    flushing: bool,
}

/// An event waiting for room in the channel. A hint that carries ids keeps
/// them in a set too, so merging one more hint into it costs the new ids
/// alone and not a pass over everything held.
struct Entry {
    event: ChangeEvent,
    ids: HashSet<String>,
}

impl Entry {
    fn new(event: ChangeEvent) -> Self {
        let ids = match &event {
            ChangeEvent::ThreadsChanged { thread_ids: ids, .. } | ChangeEvent::NewMail { message_ids: ids, .. } => {
                ids.iter().cloned().collect()
            }
            _ => HashSet::new(),
        };
        Entry { event, ids }
    }
}

/// An account's end of the channel to the window.
#[derive(Clone)]
pub(crate) struct Outgoing {
    sender: async_channel::Sender<ChangeEvent>,
    waiting: Arc<Mutex<Waiting>>,
}

impl Outgoing {
    pub(crate) fn new(sender: async_channel::Sender<ChangeEvent>) -> Self {
        Outgoing { sender, waiting: Arc::default() }
    }

    /// Sends `event` now when the channel has room and nothing waits before
    /// it; otherwise merges it into what waits and makes sure a task is
    /// handing that on as the window reads.
    pub(crate) fn send(&self, event: ChangeEvent) {
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        let event = match waiting.events.is_empty() && !waiting.flushing {
            true => match self.sender.try_send(event) {
                Ok(()) | Err(async_channel::TrySendError::Closed(_)) => return,
                Err(async_channel::TrySendError::Full(event)) => event,
            },
            false => event,
        };
        merge(&mut waiting.events, event);
        if waiting.flushing {
            return;
        }
        waiting.flushing = true;
        let (sender, queue) = (self.sender.clone(), Arc::clone(&self.waiting));
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn(flush(sender, queue))),
            // Off the runtime, a thread of its own waits for the window to
            // read, so the event does not sit until the next send.
            Err(_) => drop(std::thread::spawn(move || flush_blocking(&sender, &queue))),
        }
    }

    /// How many events wait in the sender, not yet in the channel.
    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner).events.len()
    }
}

/// Hands the waiting events to the channel, one at a time, until none wait.
async fn flush(sender: async_channel::Sender<ChangeEvent>, queue: Arc<Mutex<Waiting>>) {
    while let Some(event) = next(&queue) {
        if sender.send(event).await.is_err() {
            return closed(&queue);
        }
    }
}

/// [`flush`] for a sender with no runtime around it, on a thread of its own.
fn flush_blocking(sender: &async_channel::Sender<ChangeEvent>, queue: &Mutex<Waiting>) {
    while let Some(event) = next(queue) {
        if sender.send_blocking(event).is_err() {
            return closed(queue);
        }
    }
}

/// The next waiting event, or `None` with the flush marked over.
fn next(queue: &Mutex<Waiting>) -> Option<ChangeEvent> {
    let mut waiting = queue.lock().unwrap_or_else(PoisonError::into_inner);
    let next = waiting.events.pop_front();
    if next.is_none() {
        waiting.flushing = false;
    }
    next.map(|entry| entry.event)
}

/// Nobody listens any more, so nothing waits either.
fn closed(queue: &Mutex<Waiting>) {
    let mut waiting = queue.lock().unwrap_or_else(PoisonError::into_inner);
    waiting.events.clear();
    waiting.flushing = false;
}

/// Adds `event` to `events`: a hint into the entry that already says the
/// same kind of thing about the same account when there is one, anything
/// else at the end, since a notice said twice happened twice.
fn merge(events: &mut VecDeque<Entry>, event: ChangeEvent) {
    for held in events.iter_mut() {
        match (&mut held.event, &event) {
            (
                ChangeEvent::ThreadsChanged { account_id: a, thread_ids: ids },
                ChangeEvent::ThreadsChanged { account_id: b, thread_ids: more },
            )
            | (
                ChangeEvent::NewMail { account_id: a, message_ids: ids },
                ChangeEvent::NewMail { account_id: b, message_ids: more },
            ) if a == b => return union(ids, &mut held.ids, more),
            (ChangeEvent::LabelsChanged { account_id: a }, ChangeEvent::LabelsChanged { account_id: b }) if a == b => {
                return;
            }
            _ => {}
        }
    }
    events.push_back(Entry::new(event));
}

/// Adds the ids of `more` that `held` lacks, keeping the order they came
/// in. `known` holds the same ids as `held`.
fn union(held: &mut Vec<String>, known: &mut HashSet<String>, more: &[String]) {
    for id in more {
        if !known.contains(id) {
            known.insert(id.clone());
            held.push(id.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use mailrs_domain::AccountState;

    use super::*;

    fn threads(account_id: i64, id: &str) -> ChangeEvent {
        ChangeEvent::ThreadsChanged { account_id, thread_ids: vec![id.to_string()] }
    }

    /// Everything the window reads from `receiver` until it has heard `want`
    /// thread ids of `account`, or times out.
    async fn heard_threads(receiver: &async_channel::Receiver<ChangeEvent>, account: i64, want: usize) -> BTreeSet<String> {
        let mut heard = BTreeSet::new();
        while heard.len() < want {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
                .await
                .expect("an event")
                .unwrap();
            if let ChangeEvent::ThreadsChanged { account_id, thread_ids } = event
                && account_id == account
            {
                heard.extend(thread_ids);
            }
        }
        heard
    }

    #[tokio::test]
    async fn a_burst_past_the_channel_loses_no_thread_and_waits_as_one_entry() {
        let (sender, receiver) = async_channel::bounded(4);
        let out = Outgoing::new(sender);
        for n in 0..1000 {
            out.send(threads(1, &format!("t{n}")));
        }
        assert!(receiver.len() <= 4, "the channel holds its bound");
        assert!(out.waiting() <= 1, "the rest waits merged, not as {} events", out.waiting());
        let heard = heard_threads(&receiver, 1, 1000).await;
        assert_eq!(heard, (0..1000).map(|n| format!("t{n}")).collect());
    }

    #[tokio::test]
    async fn accounts_and_kinds_stay_apart_and_every_message_arrives() {
        let (sender, receiver) = async_channel::bounded(2);
        let out = Outgoing::new(sender);
        for n in 0..50 {
            out.send(threads(1, &format!("a{n}")));
            out.send(threads(2, &format!("b{n}")));
            out.send(ChangeEvent::NewMail { account_id: 1, message_ids: vec![format!("m{n}")] });
            out.send(ChangeEvent::WriteFailed { account_id: 1, message: format!("failure {n}") });
        }
        out.send(ChangeEvent::AccountStateChanged { account_id: 1, state: AccountState::Ok });
        let (mut a, mut b, mut mail, mut failures) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), Vec::new());
        let mut state = false;
        while !(a.len() == 50 && b.len() == 50 && mail.len() == 50 && failures.len() == 50 && state) {
            match tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv()).await.expect("an event").unwrap() {
                ChangeEvent::ThreadsChanged { account_id: 1, thread_ids } => a.extend(thread_ids),
                ChangeEvent::ThreadsChanged { account_id: 2, thread_ids } => b.extend(thread_ids),
                ChangeEvent::NewMail { message_ids, .. } => mail.extend(message_ids),
                ChangeEvent::WriteFailed { message, .. } => failures.push(message),
                ChangeEvent::AccountStateChanged { .. } => state = true,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(failures, (0..50).map(|n| format!("failure {n}")).collect::<Vec<_>>(), "in order");
    }

    #[tokio::test]
    async fn the_same_notice_twice_in_a_row_arrives_twice() {
        let (sender, receiver) = async_channel::bounded(1);
        let out = Outgoing::new(sender);
        out.send(threads(1, "t0"));
        let failed = ChangeEvent::WriteFailed { account_id: 1, message: "could not archive".into() };
        out.send(failed.clone());
        out.send(failed.clone());
        let mut failures = 0;
        while failures < 2 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv()).await.expect("an event").unwrap();
            if event == failed {
                failures += 1;
            }
        }
    }

    #[test]
    fn a_send_with_no_runtime_still_reaches_the_window() {
        let (sender, receiver) = async_channel::bounded(1);
        let out = Outgoing::new(sender);
        out.send(threads(1, "t0"));
        out.send(ChangeEvent::WriteFailed { account_id: 1, message: "late".into() });
        let (heard, got) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let _ = heard.send(receiver.recv_blocking().unwrap());
            }
        });
        let wait = std::time::Duration::from_secs(5);
        assert_eq!(got.recv_timeout(wait).unwrap(), threads(1, "t0"));
        assert!(matches!(got.recv_timeout(wait), Ok(ChangeEvent::WriteFailed { .. })), "the waiting event goes on its own");
    }

    #[tokio::test]
    async fn a_closed_channel_drops_what_waits() {
        let (sender, receiver) = async_channel::bounded(1);
        let out = Outgoing::new(sender);
        for n in 0..10 {
            out.send(threads(1, &format!("t{n}")));
        }
        drop(receiver);
        // The flusher meets the closed channel and clears the queue.
        for _ in 0..100 {
            if out.waiting() == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("events still wait for a window that is gone");
    }
}
