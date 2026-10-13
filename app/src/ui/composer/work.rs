//! The one slow job a composer runs at a time: building the message off
//! the GTK thread to send it, or saving a draft. It keeps a second press
//! out, and lets a closing window drop the result of a job still running.

use std::cell::Cell;

/// Proof that a job began; hand it back to [`Work::finish`] with the result.
#[derive(Debug)]
pub struct Ticket(u64);

#[derive(Default)]
pub struct Work {
    issued: Cell<u64>,
    running: Cell<Option<u64>>,
}

impl Work {
    /// Starts a job, or nothing when one already runs.
    pub fn begin(&self) -> Option<Ticket> {
        if self.running.get().is_some() {
            return None;
        }
        let id = self.issued.get() + 1;
        self.issued.set(id);
        self.running.set(Some(id));
        Some(Ticket(id))
    }

    pub fn is_busy(&self) -> bool {
        self.running.get().is_some()
    }

    /// Ends the running job without waiting for it. Its `finish` answers
    /// false, so nothing is sent or saved from it.
    pub fn cancel(&self) {
        self.running.set(None);
    }

    /// Ends the job. True when its result is still wanted, false when
    /// [`Work::cancel`] came first.
    pub fn finish(&self, ticket: Ticket) -> bool {
        if self.running.get() != Some(ticket.0) {
            return false;
        }
        self.running.set(None);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discarding_during_a_build_drops_its_result() {
        let work = Work::default();
        let build = work.begin().unwrap();
        work.cancel();
        assert!(!work.finish(build), "a cancelled build must not be sent");
        assert!(!work.is_busy());
    }

    #[test]
    fn a_second_save_is_refused_while_one_runs() {
        let work = Work::default();
        let first = work.begin().unwrap();
        assert!(work.begin().is_none());
        assert!(work.finish(first));
        assert!(work.begin().is_some());
    }

    #[test]
    fn a_save_cannot_start_while_a_send_builds() {
        let work = Work::default();
        let _send = work.begin().unwrap();
        assert!(work.begin().is_none(), "a second job would leave an orphan draft");
    }

    #[test]
    fn a_cancelled_job_finishing_late_does_not_end_the_next_one() {
        let work = Work::default();
        let old = work.begin().unwrap();
        work.cancel();
        let next = work.begin().unwrap();
        assert!(!work.finish(old));
        assert!(work.is_busy());
        assert!(work.finish(next));
    }
}
