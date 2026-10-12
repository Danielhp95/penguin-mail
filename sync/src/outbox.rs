//! The outbox: every message waiting to go out, and the pass that sends
//! whatever is due. A message that cannot reach Gmail now waits here with
//! the bytes built for it, so closing the laptop mid-send loses nothing.
//! Send Later waits here too, with the hour it chose and, once Gmail has
//! taken it, a draft holding its bytes. The window and the assistant send
//! through this module, so the rule about what waits here and what goes
//! back to the person is written once.
//!
//! Nothing waits on a timer of its own. The caller comes round every so
//! often and calls [`Outbox::send_due`]; a message that failed carries the
//! time of its next try in `send_at`, and the wait doubles from half a
//! minute to half an hour. After about a day of tries the outbox stops and
//! leaves the message to the person, and so does one failure nothing would
//! fix. The network coming back skips the rest of a wait, through
//! [`Outbox::try_now`].
//!
//! A server that holds mail itself (JMAP with delayed submission) takes a
//! Send Later message and sends it at its hour with the laptop shut. Its
//! row keeps the server's hold in `held`, and nothing here sends that
//! message until the server has given it back. A double send is worse
//! than a late one: whenever the server may hold or have sent a message,
//! the outbox asks it first, and when no answer settles that, the person
//! is asked to check Sent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mailrs_domain::translate::gettext;
use mailrs_domain::{AccountId, EpochMillis, Target};
use mailrs_gmail::GmailError;
use mailrs_store::Db;
use mailrs_store::outbox::{self, Queued};

use crate::account::message_id_header;
use crate::backoff::{MOST_TRIES, retry_delay};
use crate::{AccountSync, Accounts, BackendError, Held, MailBackend, SavedDraft, Settled, SyncError, Unheld, Want, now_millis, outbox_id};

/// How long a claim on a waiting message holds. A send that takes longer
/// than this belongs to a run that died, and the message is free again.
const CLAIM_LASTS: EpochMillis = 10 * 60 * 1000;

/// How long the outbox waits before asking about a hold again, once its
/// hour has come and the server has not sent it yet.
const ASK_AGAIN: EpochMillis = 60_000;

/// How `held` marks a hold whose answer never came back and that nobody
/// has found on the server yet: this, then the message's `Message-ID`.
/// No server's handle starts with it.
const UNCONFIRMED: &str = "unconfirmed:";

/// What the row of a message the server may or may not have sent says.
fn check_sent() -> String {
    gettext("Penguin Mail could not confirm whether this message went out. Check Sent.")
}

/// The `Message-ID` a row waits to find its hold by, when it does.
fn unconfirmed(message: &Queued) -> Option<&str> {
    message.held.as_deref()?.strip_prefix(UNCONFIRMED)
}

/// Gives the row the server's hold and the copy waiting in Drafts.
fn adopt(message: &mut Queued, held: Held) {
    message.held = Some(held.handle);
    message.draft_id = Some(held.message_id.clone());
    message.message_id = Some(held.message_id);
    message.thread_id = Some(held.thread_id);
}

/// What handing a message to the server to hold left.
enum Handed {
    Held(Held),
    /// The server cannot hold it, or never took it: the outbox sends it.
    Kept,
    /// No answer came back and the copy could not be looked for yet. The
    /// mark goes in `held`.
    Unconfirmed(String),
    /// No answer came back and the server's answers cannot settle whether
    /// it holds the message. The mark goes in `held`.
    Unknown(String),
}

/// What calling a held message back for Send Now or a new hour left.
enum Recalled {
    /// The message is an ordinary draft now, or never reached the server,
    /// and the row says so.
    Here(Box<Queued>),
    /// The row is settled already, and this is the answer.
    Done(Posted),
}

/// What became of a message handed to the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posted {
    /// It went out. Gmail's id for the sent message comes with it.
    Sent(String),
    /// It is waiting in the outbox, under this row id, and the outbox will
    /// try again.
    Waiting(i64),
    /// Nothing another try would fix. The person has to see this.
    Refused(String),
}

/// What one pass over the outbox did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drained {
    /// The messages that went out.
    pub sent: Vec<Queued>,
    /// The messages the outbox has stopped trying, which now wait on the
    /// person.
    pub stuck: Vec<Queued>,
    /// Whether the table changed, so the lists reading it are stale.
    pub changed: bool,
}

/// What cancelling Send Later did with the messages it stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cancelled {
    /// Messages the server held and had already sent before the cancel
    /// reached it. They are in Sent.
    pub already_sent: usize,
    /// Messages the server holds that it could not be asked about now.
    /// They stay in Send Later, held.
    pub still_held: usize,
    /// Held messages that another app canceled and deleted, so nothing
    /// waits in Drafts.
    pub cancelled_elsewhere: usize,
    /// Held messages whose fate the server could not say. They wait in
    /// the Outbox, asking the person to check Sent.
    pub unconfirmed: usize,
    /// The messages that are in Gmail's Drafts now: the ones Gmail already
    /// held as drafts, and the ones scheduled while Gmail was out of reach,
    /// which cancelling saved there.
    pub in_drafts: usize,
    /// Messages Gmail never had that it could not take as drafts either.
    /// They stay in the table, since their bytes are the only copy, and
    /// the app reopens each in a composer before it drops the row.
    pub unsaved: Vec<Queued>,
}

/// Sends the messages waiting to go out. See the module docs.
pub struct Outbox<A: Accounts> {
    accounts: Arc<A>,
    db: Db,
    /// When to ask about each held row again, by row id. Only held rows
    /// past their hour, or still unconfirmed, have an entry, and a restart
    /// asks about each once more at once.
    next_look: Mutex<HashMap<i64, EpochMillis>>,
}

impl<A: Accounts> Outbox<A> {
    pub fn new(accounts: Arc<A>, db: Db) -> Self {
        Outbox { accounts, db, next_look: Mutex::new(HashMap::new()) }
    }

    /// Sends `message` now, and keeps it for later when the reason it would
    /// not go is one that passes. The bytes travel in `message.raw`, so a
    /// message that never reaches Gmail is still whole on this computer.
    /// A message already waiting that another caller is sending right now
    /// answers `Waiting` and is left to that caller.
    pub async fn post(&self, mut message: Queued) -> Result<Posted, SyncError> {
        if message.id > 0 && self.claim(message.id).await?.is_none() {
            return Ok(Posted::Waiting(message.id));
        }
        if message.held.is_some() {
            message = match self.recall(message).await? {
                Recalled::Here(message) => *message,
                Recalled::Done(posted) => return Ok(posted),
            };
        }
        match self.attempt(&message).await {
            Ok(sent) => {
                self.forget(&message).await?;
                Ok(Posted::Sent(sent))
            }
            // An account that is not connected has told us nothing about
            // the message, so a message already waiting waits on as it is.
            Err(SyncError::UnknownAccount(_)) if message.id > 0 => {
                self.release(message.id).await?;
                Ok(Posted::Waiting(message.id))
            }
            Err(err) if worth_retrying(&err) => {
                message.attempts += 1;
                Ok(Posted::Waiting(self.keep(message, &err).await?.id))
            }
            Err(err) => {
                // A message already waiting keeps its place, so the Outbox
                // goes on showing it and saying what is wrong with it.
                if message.id > 0 {
                    message.attempts = MOST_TRIES;
                    self.keep(message, &err).await?;
                }
                Ok(Posted::Refused(err.to_string()))
            }
        }
    }

    /// A server that holds mail itself takes the message, and sends it at
    /// its hour with the laptop shut. The bytes stay here too, since the
    /// server's copy leaves out the Bcc that Send Now and a new hour need.
    ///
    /// Otherwise saves `message` as a Gmail draft and records the hour to
    /// send it. With no way through to Gmail the bytes wait here instead
    /// and the message goes out from them at its hour, so Send Later works
    /// on a laptop that is offline when the writer picks the time.
    pub async fn schedule(&self, mut message: Queued) -> Result<Posted, SyncError> {
        match self.hand_over(&message, message.send_at).await {
            Ok(Handed::Held(held)) => {
                adopt(&mut message, held);
                return self.put_waiting(message).await;
            }
            Ok(Handed::Unconfirmed(mark)) => {
                message.held = Some(mark);
                return self.put_waiting(message).await;
            }
            Ok(Handed::Unknown(mark)) => {
                message.held = Some(mark);
                let kept = self.unconfirmable(message).await?;
                return Ok(Posted::Waiting(kept.id));
            }
            Ok(Handed::Kept) => {}
            Err(err) => return Ok(Posted::Refused(err.to_string())),
        }
        match self.save_draft(&message).await {
            Ok(saved) => {
                message.draft_id = Some(saved.draft_id);
                message.message_id = Some(saved.message_id);
                message.thread_id = Some(saved.thread_id);
                // Gmail holds the bytes now, so sending the draft sends
                // whatever the writer changed on another device.
                message.raw = None;
            }
            // Nothing has failed yet: the message keeps its hour and goes
            // out from these bytes, so it stays in Send Later.
            Err(err) if worth_retrying(&err) => {
                tracing::info!(error = %err, "scheduled here; Gmail holds no draft for it yet");
            }
            Err(err) => return Ok(Posted::Refused(err.to_string())),
        }
        let id = self.db.write(move |c| outbox::put(c, &message)).await?;
        Ok(Posted::Waiting(id))
    }

    /// Tries every message whose time has come, soonest first. One the
    /// outbox has given up on is left alone until the person asks for it,
    /// and one another caller is sending is left to that caller.
    ///
    /// A held message is the server's to send: the pass asks where it
    /// stands, once a minute past its hour, and never sends it here. One
    /// whose hold's answer was lost is looked for on every pass until the
    /// server says whether it holds it.
    pub async fn send_due(&self, now: EpochMillis) -> Result<Drained, SyncError> {
        let mut drained = Drained::default();
        let waiting = self
            .db
            .read(move |c| Ok(outbox::list(c)?.into_iter().filter(|m| m.send_at <= now || unconfirmed(m).is_some()).collect::<Vec<_>>()))
            .await?;
        for due in waiting {
            if retry_delay(due.attempts).is_none() || (due.held.is_some() && !self.look_due(due.id, now)) {
                continue;
            }
            // The row as it stands once claimed, which another caller may
            // have sent, dropped or moved since the list was read.
            let Some(mut message) = self.claim(due.id).await? else {
                continue;
            };
            if message.held.is_some() && retry_delay(message.attempts).is_some() {
                self.settle(message, now, &mut drained).await?;
                continue;
            }
            if message.send_at > now || retry_delay(message.attempts).is_none() || message.held.is_some() {
                self.release(message.id).await?;
                continue;
            }
            match self.attempt(&message).await {
                // The account may still be connecting, or may have gone
                // for good. Either way the message has not been tried, so
                // nothing is counted against it and the next pass asks
                // again.
                Err(SyncError::UnknownAccount(account_id)) => {
                    tracing::debug!(account = account_id, "not connected yet; the outbox waits");
                    self.release(message.id).await?;
                    continue;
                }
                Ok(_) => {
                    self.forget(&message).await?;
                    drained.sent.push(message);
                }
                Err(err) => {
                    message.attempts = if worth_retrying(&err) {
                        message.attempts + 1
                    } else {
                        MOST_TRIES
                    };
                    let gave_up = retry_delay(message.attempts).is_none();
                    let kept = self.keep(message, &err).await?;
                    if gave_up {
                        drained.stuck.push(kept);
                    }
                }
            }
            drained.changed = true;
        }
        Ok(drained)
    }

    /// Sends one waiting message now, whatever its interval says.
    pub async fn send_one(&self, id: i64) -> Result<Posted, SyncError> {
        match self.db.read(move |c| outbox::find(c, id)).await? {
            Some(message) => self.post(message).await,
            None => Ok(Posted::Refused("That message is no longer waiting".into())),
        }
    }

    /// Brings every stuck message forward, for when the network comes back
    /// and sitting out the rest of a wait would serve nobody.
    pub async fn try_now(&self) -> Result<(), SyncError> {
        let now = now_millis();
        self.db.write(move |c| outbox::try_now(c, now)).await?;
        Ok(())
    }

    /// Drops a waiting message. A Send Later message leaves its Gmail
    /// draft behind, as cancelling one always has.
    pub async fn drop_one(&self, id: i64) -> Result<(), SyncError> {
        self.db.write(move |c| outbox::remove(c, id)).await?;
        Ok(())
    }

    pub async fn find(&self, id: i64) -> Result<Option<Queued>, SyncError> {
        Ok(self.db.read(move |c| outbox::find(c, id)).await?)
    }

    /// The waiting message that sends the Gmail draft `draft_id`, which is
    /// how a reopened draft finds the hour Send Later gave it.
    pub async fn find_draft(
        &self,
        account_id: AccountId,
        draft_id: &str,
    ) -> Result<Option<Queued>, SyncError> {
        let draft_id = draft_id.to_string();
        Ok(self
            .db
            .read(move |c| outbox::find_draft(c, account_id, &draft_id))
            .await?)
    }

    /// Records that the writer saved a draft again. A Send Later message
    /// waiting on that draft keeps its hour and now names the draft's new
    /// message and thread, so the list and a later cancel still find it.
    /// A draft nothing waits on changes nothing here.
    pub async fn draft_saved(
        &self,
        account_id: AccountId,
        saved: SavedDraft,
    ) -> Result<(), SyncError> {
        self.db
            .write(move |c| {
                outbox::set_message(
                    c,
                    account_id,
                    &saved.draft_id,
                    &saved.message_id,
                    &saved.thread_id,
                )
            })
            .await?;
        Ok(())
    }

    /// Stops the Send Later messages the targets name and says what became
    /// of them. Each Gmail draft stays in Drafts. A message scheduled while
    /// Gmail was out of reach has no draft, so stopping it deletes it. A
    /// list row names a scheduled message by its Gmail thread, by its
    /// draft's message, or, for a message Gmail has never seen, by its own
    /// place in the table, so a target matching any of the three stops it.
    pub async fn cancel_scheduled(&self, targets: &[Target]) -> Result<Cancelled, SyncError> {
        let named = self.named(targets).await?;
        let mut cancelled = Cancelled::default();
        for item in named.into_iter().filter(|item| item.problem.is_none()) {
            let item = match item.held {
                Some(_) => match self.cancel_held(item, &mut cancelled).await? {
                    Some(here) => here,
                    None => continue,
                },
                None => item,
            };
            // A message Gmail never had goes to Drafts now, so cancelling
            // does not throw away the only copy.
            if item.draft_id.is_none()
                && let Err(err) = self.save_draft(&item).await
            {
                tracing::info!(error = %err, "could not save a cancelled message to Drafts");
                cancelled.unsaved.push(item);
                continue;
            }
            let id = item.id;
            self.db.write(move |c| outbox::remove(c, id)).await?;
            cancelled.in_drafts += 1;
        }
        Ok(cancelled)
    }

    /// The waiting messages, in Send Later and the Outbox alike, that the
    /// list rows `targets` stand for, soonest first. A row names one the
    /// way [`Outbox::cancel_scheduled`] describes.
    pub async fn named(&self, targets: &[Target]) -> Result<Vec<Queued>, SyncError> {
        let targets = targets.to_vec();
        Ok(self
            .db
            .read(move |c| {
                Ok(outbox::list(c)?
                    .into_iter()
                    .filter(|item| targets.iter().any(|t| names(t, item)))
                    .collect())
            })
            .await?)
    }

    /// Moves a Send Later message to the hour `at`. Gmail holds only the
    /// draft, so the new hour lives here alone. A message the server holds
    /// is called back and held again at the new hour. `None` when the
    /// message has gone (the server may have sent it), or has hit a
    /// problem and waits in the Outbox, where the hour is the next try and
    /// not the writer's to choose.
    pub async fn reschedule(&self, id: i64, at: EpochMillis) -> Result<Option<Queued>, SyncError> {
        if let Some(message) = self.find(id).await?.filter(|m| m.problem.is_none() && m.held.is_some()) {
            return self.hold_again(message, at).await;
        }
        Ok(self
            .db
            .write(move |c| {
                let Some(mut message) = outbox::find(c, id)?.filter(|m| m.problem.is_none()) else {
                    return Ok(None);
                };
                message.send_at = at;
                outbox::put(c, &message)?;
                Ok(Some(message))
            })
            .await?)
    }

    /// Takes the waiting message `id` for this caller, or `None` when it
    /// has gone or another caller is sending it.
    async fn claim(&self, id: i64) -> Result<Option<Queued>, SyncError> {
        let now = now_millis();
        Ok(self
            .db
            .write(move |c| outbox::claim(c, id, now, now - CLAIM_LASTS))
            .await?)
    }

    async fn release(&self, id: i64) -> Result<(), SyncError> {
        self.db.write(move |c| outbox::release(c, id)).await?;
        Ok(())
    }

    /// One try at Gmail: from the bytes this computer holds, or from the
    /// Gmail draft when they are Gmail's. Sending bytes that came out of a
    /// draft deletes that draft, so Drafts is not left holding a copy of
    /// what just went out.
    ///
    /// A message tried before may have reached Gmail on a try whose answer
    /// was lost, so its bytes are looked for in Sent first. A draft needs
    /// no such look: Gmail deletes a draft it sends, and sending one that
    /// has gone answers that it is gone, which counts as sent.
    async fn attempt(&self, message: &Queued) -> Result<String, SyncError> {
        let sync = self
            .accounts
            .account(message.account_id)
            .ok_or(SyncError::UnknownAccount(message.account_id))?;
        match (&message.raw, &message.draft_id) {
            (Some(raw), draft_id) => {
                if message.attempts > 0
                    && let Some(sent) = sync.sent_copy(raw).await?
                {
                    tracing::info!(
                        account = message.account_id,
                        "a message tried before is already in Sent; not sending it again"
                    );
                    // A server that keeps one copy of the same bytes sent
                    // the draft itself: the copy found is the draft, and
                    // deleting it would delete the sent message.
                    if let Some(draft_id) = draft_id
                        && *draft_id != sent
                        && let Err(err) = sync.delete_draft(draft_id).await
                    {
                        tracing::warn!(error = %err, "sent, but could not delete the draft");
                    }
                    return Ok(sent);
                }
                sync.send(raw.clone(), message.thread_id.clone(), draft_id.clone())
                    .await
            }
            (None, Some(draft_id)) => Ok(sync
                .send_draft(draft_id)
                .await?
                .unwrap_or_else(|| draft_id.clone())),
            (None, None) => Err(BackendError::NotFound.into()),
        }
    }

    async fn save_draft(&self, message: &Queued) -> Result<SavedDraft, SyncError> {
        let sync = self
            .accounts
            .account(message.account_id)
            .ok_or(SyncError::UnknownAccount(message.account_id))?;
        let raw = message.raw.clone().unwrap_or_default();
        sync.save_draft(raw, message.thread_id.clone(), message.draft_id.clone())
            .await
    }

    /// Takes a message that has gone out off the table, whether it was
    /// waiting under its own row or under the Gmail draft it occupied.
    async fn forget(&self, message: &Queued) -> Result<(), SyncError> {
        self.looked(message.id);
        let (id, account_id, draft_id) = (message.id, message.account_id, message.draft_id.clone());
        self.db
            .write(move |c| match (id, draft_id) {
                (id, _) if id > 0 => outbox::remove(c, id),
                (_, Some(draft_id)) => outbox::remove_draft(c, account_id, &draft_id),
                _ => Ok(()),
            })
            .await?;
        Ok(())
    }

    /// Keeps a message that would not go out, with why and when to try
    /// again, and gives back the row as it now stands. `message.attempts`
    /// already counts this try.
    async fn keep(&self, mut message: Queued, err: &SyncError) -> Result<Queued, SyncError> {
        message.problem = Some(err.to_string());
        message.send_at = now_millis()
            + retry_delay(message.attempts)
                .unwrap_or_default()
                .as_millis() as EpochMillis;
        message.id = self
            .db
            .write({
                let message = message.clone();
                move |c| outbox::put(c, &message)
            })
            .await?;
        Ok(message)
    }

    /// Stores a message that waits for its hour.
    async fn put_waiting(&self, message: Queued) -> Result<Posted, SyncError> {
        Ok(Posted::Waiting(self.db.write(move |c| outbox::put(c, &message)).await?))
    }

    /// Whether it is time to ask the server about the held row `id`.
    fn look_due(&self, id: i64, now: EpochMillis) -> bool {
        let looks = self.next_look.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        looks.get(&id).is_none_or(|at| *at <= now)
    }

    fn look_at(&self, id: i64, at: EpochMillis) {
        self.next_look.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id, at);
    }

    fn looked(&self, id: i64) {
        self.next_look.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
    }

    /// Hands `message` to its server to send at `at`, when the server holds
    /// mail and the bytes are here. When no answer comes back the server
    /// may hold it anyway, so the outbox looks for its copy before it
    /// keeps a second one. A message without a `Message-ID` could not be
    /// looked for, so it stays here.
    async fn hand_over(&self, message: &Queued, at: EpochMillis) -> Result<Handed, SyncError> {
        let (Some(raw), Some(sync)) = (&message.raw, self.accounts.account(message.account_id)) else {
            return Ok(Handed::Kept);
        };
        let Some(id) = message_id_header(raw) else {
            return Ok(Handed::Kept);
        };
        let err = match sync.hold(raw, at).await {
            Ok(Some(held)) => return Ok(Handed::Held(held)),
            Ok(None) => return Ok(Handed::Kept),
            Err(err) if worth_retrying(&err) => err,
            Err(err) => return Err(err),
        };
        tracing::info!(error = %err, "no answer to a hold; looking for it on the server");
        let mark = format!("{UNCONFIRMED}{id}");
        Ok(match sync.find_hold(&id).await {
            Ok(Some(held)) => Handed::Held(held),
            Ok(None) => Handed::Kept,
            Err(err) if worth_retrying(&err) => Handed::Unconfirmed(mark),
            Err(err) => {
                tracing::info!(error = %err, "the server cannot say whether it holds the message");
                Handed::Unknown(mark)
            }
        })
    }

    /// Looks again for the hold of a row whose hold's answer was lost.
    /// Found, the row takes it, and a copy the row waited on before goes
    /// from Drafts. Not there, the server never took the message and the
    /// outbox keeps it, looking in Sent before it sends.
    async fn confirm(&self, sync: &AccountSync, message: &mut Queued) -> Result<(), SyncError> {
        let Some(id) = unconfirmed(message).map(str::to_string) else {
            return Ok(());
        };
        match sync.find_hold(&id).await? {
            Some(held) => {
                if let Some(old) = message.draft_id.clone().filter(|old| *old != held.message_id)
                    && let Err(err) = sync.delete_draft(&old).await
                {
                    tracing::warn!(error = %err, "held, but the old copy stayed in Drafts");
                }
                adopt(message, held);
            }
            None => {
                message.held = None;
                message.attempts = message.attempts.max(1);
            }
        }
        self.looked(message.id);
        Ok(())
    }

    /// Records that nothing can say whether the server sent `message`. It
    /// keeps its hold and waits in the Outbox, and nothing here sends it.
    async fn unconfirmable(&self, mut message: Queued) -> Result<Queued, SyncError> {
        self.looked(message.id);
        message.problem = Some(check_sent());
        message.attempts = MOST_TRIES;
        let stored = message.clone();
        message.id = self.db.write(move |c| outbox::put(c, &stored)).await?;
        Ok(message)
    }

    /// Where a held message stands once its hour came: still waiting, sent
    /// and filed, called back elsewhere, or past knowing. A row whose
    /// hold's answer was lost is looked for first. `message` is claimed.
    async fn settle(&self, mut message: Queued, now: EpochMillis, drained: &mut Drained) -> Result<(), SyncError> {
        let Some(sync) = self.accounts.account(message.account_id) else {
            return self.release(message.id).await;
        };
        if unconfirmed(&message).is_some() {
            match self.confirm(&sync, &mut message).await {
                Ok(()) => {
                    // The next pass settles the hold or sends the message.
                    self.db.write(move |c| outbox::put(c, &message)).await?;
                    drained.changed = true;
                }
                Err(err) if worth_retrying(&err) => {
                    self.look_at(message.id, now + ASK_AGAIN);
                    self.release(message.id).await?;
                }
                Err(_) => {
                    drained.stuck.push(self.unconfirmable(message).await?);
                    drained.changed = true;
                }
            }
            return Ok(());
        }
        let handle = message.held.clone().unwrap_or_default();
        let copy = message.message_id.clone().unwrap_or_default();
        match sync.settle_hold(&handle, &copy, message.send_at).await {
            Ok(Settled::Waiting(until)) => {
                self.look_at(message.id, until.max(now + ASK_AGAIN));
                self.release(message.id).await?;
            }
            Ok(Settled::Sent) => {
                self.forget(&message).await?;
                drained.sent.push(message);
                drained.changed = true;
            }
            Ok(Settled::Canceled) => {
                self.forget(&message).await?;
                drained.changed = true;
            }
            Ok(Settled::Unknown) => {
                drained.stuck.push(self.unconfirmable(message).await?);
                drained.changed = true;
            }
            Err(err) if worth_retrying(&err) => {
                self.look_at(message.id, now + ASK_AGAIN);
                self.release(message.id).await?;
            }
            Err(err) => {
                self.looked(message.id);
                message.attempts = MOST_TRIES;
                drained.stuck.push(self.keep(message, &err).await?);
                drained.changed = true;
            }
        }
        Ok(())
    }

    /// Calls the server's hold on `message` back, for Send Now or a new
    /// hour. `Here` gives the row as an ordinary draft, claimed again, or
    /// as a message the server never took; `Done` the answer when the
    /// server sent it, cannot say, or cannot be asked. `message` is
    /// claimed.
    async fn recall(&self, mut message: Queued) -> Result<Recalled, SyncError> {
        let id = message.id;
        let Some(sync) = self.accounts.account(message.account_id) else {
            self.release(id).await?;
            return Ok(Recalled::Done(Posted::Waiting(id)));
        };
        if unconfirmed(&message).is_some() {
            match self.confirm(&sync, &mut message).await {
                Ok(()) if message.held.is_none() => return self.store_claimed(message).await,
                Ok(()) => {}
                Err(err) if worth_retrying(&err) => {
                    self.release(id).await?;
                    return Ok(Recalled::Done(Posted::Waiting(id)));
                }
                Err(_) => {
                    self.unconfirmable(message).await?;
                    return Ok(Recalled::Done(Posted::Refused(check_sent())));
                }
            }
        }
        let handle = message.held.clone().unwrap_or_default();
        let copy = message.message_id.clone().unwrap_or_default();
        match sync.release_hold(&handle, &copy, message.send_at).await {
            Ok(Unheld::InDrafts) => {
                message.held = None;
                self.looked(id);
                if !self.copy_left(&sync, &message).await {
                    self.forget(&message).await?;
                    return Ok(Recalled::Done(Posted::Refused(gettext(
                        "This message was canceled in another app and is no longer in Drafts.",
                    ))));
                }
                self.store_claimed(message).await
            }
            Ok(Unheld::AlreadySent) => {
                self.forget(&message).await?;
                Ok(Recalled::Done(Posted::Sent(copy)))
            }
            Ok(Unheld::Unknown) => {
                self.unconfirmable(message).await?;
                Ok(Recalled::Done(Posted::Refused(check_sent())))
            }
            Err(err) if worth_retrying(&err) => {
                self.release(id).await?;
                Ok(Recalled::Done(Posted::Waiting(id)))
            }
            Err(err) => {
                self.release(id).await?;
                Ok(Recalled::Done(Posted::Refused(err.to_string())))
            }
        }
    }

    /// Writes a row the server let go of, so a crash from here on leaves
    /// a message the outbox sends, and claims it again for the caller.
    async fn store_claimed(&self, message: Queued) -> Result<Recalled, SyncError> {
        let stored = message.clone();
        let id = self.db.write(move |c| outbox::put(c, &stored)).await?;
        Ok(match self.claim(id).await? {
            Some(_) => Recalled::Here(Box::new(Queued { id, ..message })),
            None => Recalled::Done(Posted::Waiting(id)),
        })
    }

    /// Whether the copy a called-back hold left is still on the server.
    /// Another app that canceled the hold may have deleted it. A server
    /// that cannot say leaves the copy assumed there.
    async fn copy_left(&self, sync: &AccountSync, message: &Queued) -> bool {
        let Some(copy) = message.draft_id.clone() else {
            return true;
        };
        match sync.services().mail.fetch(vec![Want::message(copy)]).await {
            Ok(found) => !found.metas.is_empty() || found.gone.is_empty(),
            Err(err) => {
                tracing::info!(error = %err, "could not look for a called-back copy");
                true
            }
        }
    }

    /// Calls the hold on a Send Later message back for Cancel Send, and
    /// counts what became of it. `Some` gives the row back when the server
    /// never took the message, for the cancel to treat as its own.
    async fn cancel_held(&self, item: Queued, cancelled: &mut Cancelled) -> Result<Option<Queued>, SyncError> {
        let Some(item) = self.claim(item.id).await? else {
            cancelled.still_held += 1;
            return Ok(None);
        };
        let id = item.id;
        let Some(sync) = self.accounts.account(item.account_id) else {
            self.release(id).await?;
            cancelled.still_held += 1;
            return Ok(None);
        };
        let mut item = item;
        if unconfirmed(&item).is_some() {
            match self.confirm(&sync, &mut item).await {
                Ok(()) if item.held.is_none() => {
                    let stored = item.clone();
                    self.db.write(move |c| outbox::put(c, &stored)).await?;
                    return Ok(Some(item));
                }
                Ok(()) => {}
                Err(err) if worth_retrying(&err) => {
                    self.release(id).await?;
                    cancelled.still_held += 1;
                    return Ok(None);
                }
                Err(_) => {
                    self.unconfirmable(item).await?;
                    cancelled.unconfirmed += 1;
                    return Ok(None);
                }
            }
        }
        let handle = item.held.clone().unwrap_or_default();
        let copy = item.message_id.clone().unwrap_or_default();
        match sync.release_hold(&handle, &copy, item.send_at).await {
            Ok(Unheld::InDrafts) if self.copy_left(&sync, &item).await => cancelled.in_drafts += 1,
            Ok(Unheld::InDrafts) => cancelled.cancelled_elsewhere += 1,
            Ok(Unheld::AlreadySent) => cancelled.already_sent += 1,
            Ok(Unheld::Unknown) => {
                self.unconfirmable(item).await?;
                cancelled.unconfirmed += 1;
                return Ok(None);
            }
            Err(err) => {
                tracing::info!(error = %err, "could not call a held message back");
                self.release(id).await?;
                cancelled.still_held += 1;
                return Ok(None);
            }
        }
        self.forget(&item).await?;
        Ok(None)
    }

    /// Calls the hold back and holds the message again at `at`. Once the
    /// server lets go, the row is a draft the outbox sends at `at`, so a
    /// failure from there on still sends it once, at the new hour.
    async fn hold_again(&self, message: Queued, at: EpochMillis) -> Result<Option<Queued>, SyncError> {
        let Some(message) = self.claim(message.id).await? else {
            return Ok(None);
        };
        let mut message = match self.recall(message).await? {
            Recalled::Here(message) => *message,
            Recalled::Done(Posted::Waiting(_)) => {
                return Err(SyncError::Backend(BackendError::Offline(gettext(
                    "The server could not be asked to let go of the message.",
                ))));
            }
            Recalled::Done(Posted::Refused(reason)) => return Err(SyncError::Backend(BackendError::Refused(reason))),
            Recalled::Done(Posted::Sent(_)) => return Ok(None),
        };
        message.send_at = at;
        let old = message.draft_id.clone();
        match self.hand_over(&message, at).await {
            Ok(Handed::Held(held)) => {
                if let (Some(old), Some(sync)) = (old.filter(|old| *old != held.message_id), self.accounts.account(message.account_id))
                    && let Err(err) = sync.delete_draft(&old).await
                {
                    tracing::warn!(error = %err, "held again, but the old copy stayed in Drafts");
                }
                adopt(&mut message, held);
            }
            Ok(Handed::Unconfirmed(mark)) => message.held = Some(mark),
            Ok(Handed::Unknown(mark)) => {
                message.held = Some(mark);
                self.unconfirmable(message).await?;
                return Ok(None);
            }
            Ok(Handed::Kept) => {}
            Err(err) => tracing::info!(error = %err, "the server would not hold the message again; the app sends it"),
        }
        let kept = message.clone();
        self.db.write(move |c| outbox::put(c, &message)).await?;
        Ok(Some(kept))
    }
}

/// Whether the list row `target` stands for the waiting message `item`.
fn names(target: &Target, item: &Queued) -> bool {
    target.account_id == item.account_id
        && (outbox_id(&target.thread_id) == Some(item.id)
            || item.thread_id.as_deref() == Some(target.thread_id.as_str())
            || (item.message_id.is_some() && target.message_id == item.message_id))
}

/// Whether a message that would not go out is worth trying again. No
/// network, a 5xx and a rate limit are; a refused recipient, a message
/// over Gmail's size limit and a revoked token are not, because the
/// same bytes fail the same way however long the outbox waits, and the
/// person is the only one who can fix any of them.
fn worth_retrying(err: &SyncError) -> bool {
    match err {
        // Gmail answers a request that took too long with 408 and
        // nothing else; every other 4xx is about the message.
        SyncError::Backend(BackendError::Gmail(GmailError::Http { status: 408, .. })) => true,
        SyncError::Backend(err) => err.is_transient(),
        // This computer's own trouble, not the message's, and it
        // usually clears on its own.
        SyncError::Store(_) => true,
        // The account is still connecting.
        SyncError::UnknownAccount(_) => true,
        SyncError::NoLabel(_) | SyncError::ReservedLabel(_) => false,
        // A mail action never names a calendar, so the outbox never
        // meets this one; the same id fails the same way regardless.
        SyncError::NoCalendar(_) | SyncError::MadeInGoogle(_) => false,
        // The bytes could not be written at all, so the same draft
        // would fail the same way on every try.
        SyncError::Mime(_) => false,
        SyncError::NotAnAddress(_) => false,
        // The outbox sends nothing to a list's server.
        SyncError::OneClick(_) => false,
        // The keyring's own trouble, not the message's; it usually clears
        // on its own, as a locked keyring does once the person unlocks it.
        SyncError::Password(_) | SyncError::NoAnswer(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn http(status: u16) -> SyncError {
        SyncError::from(GmailError::Http {
            status,
            body: String::new(),
        })
    }

    #[test]
    fn a_message_waits_out_no_network_a_rate_limit_and_gmail_falling_over() {
        for err in [
            SyncError::from(GmailError::Network("connection refused".into())),
            SyncError::from(GmailError::RateLimited {
                retry_after: Some(Duration::from_secs(3)),
            }),
            http(500),
            http(503),
            http(408),
            SyncError::UnknownAccount(1),
        ] {
            assert!(worth_retrying(&err), "{err} is worth another try");
        }
    }

    #[test]
    fn a_refused_recipient_an_oversized_message_and_a_revoked_token_go_to_the_person() {
        for err in [
            http(400),
            http(413),
            http(403),
            SyncError::from(GmailError::NeedsReauth),
            SyncError::from(GmailError::MissingScope),
            SyncError::from(GmailError::NotFound),
        ] {
            assert!(!worth_retrying(&err), "{err} needs the person");
        }
    }
}
