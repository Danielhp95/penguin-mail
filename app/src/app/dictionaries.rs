//! Spelling dictionaries the open composers share. A set of Hunspell
//! dictionaries takes tens of megabytes, so it is held only while a
//! composer uses it, and read again by the next composer after the last
//! one closes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

/// Loaded dictionaries by the languages they cover, held weakly.
pub(crate) struct Shared<V> {
    held: RefCell<HashMap<Vec<String>, Weak<V>>>,
}

impl<V> Default for Shared<V> {
    fn default() -> Self {
        Shared {
            held: RefCell::new(HashMap::new()),
        }
    }
}

impl<V> Shared<V> {
    /// The dictionaries for `languages`, while a composer still holds them.
    pub(crate) fn get(&self, languages: &[String]) -> Option<Rc<V>> {
        self.held.borrow().get(languages)?.upgrade()
    }

    /// Shares `loaded` with the next composer that asks for `languages`.
    pub(crate) fn keep(&self, languages: Vec<String>, loaded: &Rc<V>) {
        let mut held = self.held.borrow_mut();
        held.retain(|_, kept| kept.strong_count() > 0);
        held.insert(languages, Rc::downgrade(loaded));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn english() -> Vec<String> {
        vec!["en_US".to_string()]
    }

    #[test]
    fn a_second_composer_shares_the_first_one_s_dictionaries() {
        let shared = Shared::default();
        let loaded = Rc::new("words".to_string());
        shared.keep(english(), &loaded);
        let again = shared.get(&english()).expect("still held");
        assert!(Rc::ptr_eq(&loaded, &again));
    }

    #[test]
    fn dictionaries_no_composer_holds_are_let_go() {
        let shared = Shared::default();
        shared.keep(english(), &Rc::new("words".to_string()));
        assert!(shared.get(&english()).is_none());
    }

    #[test]
    fn keeping_new_dictionaries_forgets_the_ones_let_go() {
        let shared: Shared<String> = Shared::default();
        shared.keep(english(), &Rc::new("words".to_string()));
        let french = Rc::new("mots".to_string());
        shared.keep(vec!["fr_FR".to_string()], &french);
        assert_eq!(shared.held.borrow().len(), 1);
    }
}
