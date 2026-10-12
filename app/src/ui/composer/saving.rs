//! The draft save a composer has running, as the close dialog sees it.
//! Save Draft while a save runs waits for that save and builds on the
//! draft it made, rather than making a second one. Discard while a save
//! runs deletes the draft that save made, which the writer just said they
//! did not want.

use std::cell::{Cell, RefCell};

use futures::channel::oneshot;

/// What becomes of the draft a save made, once it lands.
#[derive(Debug, PartialEq, Eq)]
pub enum Landed {
    Keep,
    /// The writer discarded the message while the save ran, and the save
    /// made this draft new.
    Delete(String),
}

#[derive(Default)]
pub struct Saving {
    /// Who waits for the running save, or `None` while none runs.
    waiting: RefCell<Option<Vec<oneshot::Sender<Option<String>>>>>,
    discarded: Cell<bool>,
}

impl Saving {
    /// A save started.
    pub fn began(&self) {
        *self.waiting.borrow_mut() = Some(Vec::new());
    }

    /// The draft id the running save gets back, or `None` when it fails.
    /// `None` here when no save runs.
    pub fn wait(&self) -> Option<oneshot::Receiver<Option<String>>> {
        let mut waiting = self.waiting.borrow_mut();
        let (tell, heard) = oneshot::channel();
        waiting.as_mut()?.push(tell);
        Some(heard)
    }

    /// The writer discarded the message.
    pub fn discard(&self) {
        self.discarded.set(true);
    }

    /// The running save ended. `was` is the draft it started from, `saved`
    /// the one it got back, or `None` when it failed. Whoever waits hears
    /// `saved`.
    pub fn landed(&self, was: Option<&str>, saved: Option<&str>) -> Landed {
        for tell in self.waiting.borrow_mut().take().unwrap_or_default() {
            let _ = tell.send(saved.map(str::to_string));
        }
        match (self.discarded.get(), was, saved) {
            (true, None, Some(made)) => Landed::Delete(made.to_string()),
            _ => Landed::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_while_a_save_runs_hears_the_draft_it_made() {
        let saving = Saving::default();
        assert!(saving.wait().is_none(), "nothing runs, so nothing to wait for");
        saving.began();
        let heard = saving.wait().expect("a save runs");
        assert_eq!(saving.landed(None, Some("d1")), Landed::Keep);
        assert_eq!(futures::executor::block_on(heard), Ok(Some("d1".to_string())));
        assert!(saving.wait().is_none(), "the save is over");
    }

    #[test]
    fn a_failed_save_tells_its_waiters_so() {
        let saving = Saving::default();
        saving.began();
        let heard = saving.wait().expect("a save runs");
        assert_eq!(saving.landed(None, None), Landed::Keep);
        assert_eq!(futures::executor::block_on(heard), Ok(None));
    }

    #[test]
    fn discard_during_a_save_deletes_the_draft_it_made() {
        let saving = Saving::default();
        saving.began();
        saving.discard();
        assert_eq!(saving.landed(None, Some("d1")), Landed::Delete("d1".into()));
    }

    /// A draft saved before stays: Discard drops the changes since, as it
    /// does with no save running.
    #[test]
    fn discard_during_an_update_keeps_the_draft_that_was_there() {
        let saving = Saving::default();
        saving.began();
        saving.discard();
        assert_eq!(saving.landed(Some("d0"), Some("d2")), Landed::Keep);
        saving.began();
        assert_eq!(saving.landed(None, None), Landed::Keep, "a failed save made nothing");
    }
}
