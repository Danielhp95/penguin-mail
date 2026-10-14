//! The account chosen for each task, with context kept when no override applies.

use serde::{Deserialize, Serialize};

use super::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountUse {
    NewMessages,
    Replies,
    Forwards,
    Calendar,
    Assistant,
    Startup,
}

impl AccountUse {
    pub const ALL: [Self; 6] = [
        Self::NewMessages,
        Self::Replies,
        Self::Forwards,
        Self::Calendar,
        Self::Assistant,
        Self::Startup,
    ];
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountChoice {
    #[default]
    Automatic,
    Default,
    Account(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountDefaults {
    pub use_for_all: bool,
    pub new_messages: AccountChoice,
    pub replies: AccountChoice,
    pub forwards: AccountChoice,
    pub calendar: AccountChoice,
    pub assistant: AccountChoice,
    pub startup: AccountChoice,
}

impl Default for AccountDefaults {
    fn default() -> Self {
        Self {
            use_for_all: false,
            new_messages: AccountChoice::Default,
            replies: AccountChoice::Automatic,
            forwards: AccountChoice::Automatic,
            calendar: AccountChoice::Automatic,
            assistant: AccountChoice::Automatic,
            startup: AccountChoice::Automatic,
        }
    }
}

impl AccountDefaults {
    pub fn choice(&self, task: AccountUse) -> &AccountChoice {
        match task {
            AccountUse::NewMessages => &self.new_messages,
            AccountUse::Replies => &self.replies,
            AccountUse::Forwards => &self.forwards,
            AccountUse::Calendar => &self.calendar,
            AccountUse::Assistant => &self.assistant,
            AccountUse::Startup => &self.startup,
        }
    }

    pub fn set(&mut self, task: AccountUse, choice: AccountChoice) {
        *match task {
            AccountUse::NewMessages => &mut self.new_messages,
            AccountUse::Replies => &mut self.replies,
            AccountUse::Forwards => &mut self.forwards,
            AccountUse::Calendar => &mut self.calendar,
            AccountUse::Assistant => &mut self.assistant,
            AccountUse::Startup => &mut self.startup,
        } = choice;
    }
}

impl Settings {
    /// An override for this task; None leaves its contextual choice intact.
    pub fn preferred_account(&self, task: AccountUse) -> Option<&str> {
        let choice = if self.account_defaults.use_for_all {
            &AccountChoice::Default
        } else {
            self.account_defaults.choice(task)
        };
        match choice {
            AccountChoice::Account(email) => Some(email),
            AccountChoice::Default => self.default_account.as_deref(),
            AccountChoice::Automatic if task == AccountUse::Assistant => {
                self.preferred_account(AccountUse::NewMessages)
            }
            AccountChoice::Automatic => None,
        }
    }

    /// Resolves removed accounts through the task's automatic behavior.
    pub fn preferred_live_account<'a>(&'a self, task: AccountUse, emails: &[&str]) -> Option<&'a str> {
        let live = |email: &&str| emails.iter().any(|current| current.eq_ignore_ascii_case(email));
        self.preferred_account(task).filter(live).or_else(|| {
            (task == AccountUse::Assistant).then(|| self.preferred_account(AccountUse::NewMessages).filter(live)).flatten()
        })
    }

    /// The saved rank; unlisted accounts retain their relative order at the end.
    pub fn account_rank(&self, email: &str) -> usize {
        self.account_order
            .iter()
            .position(|saved| saved.eq_ignore_ascii_case(email))
            .unwrap_or(usize::MAX)
    }

    /// Writes `moved`, the accounts of `shown` in their new order, into the
    /// saved order. Each takes the next place one of them held, and an
    /// account the list does not show, such as a calendar account in the
    /// mail sidebar or a row this build cannot read, keeps its own.
    pub(super) fn place_shown(&mut self, shown: &[&str], moved: Vec<String>) {
        let is_shown = |email: &str| shown.iter().any(|s| s.eq_ignore_ascii_case(email));
        let mut order = self.account_order.clone();
        for email in self.ordered(shown) {
            if !order.iter().any(|saved| saved.eq_ignore_ascii_case(email)) {
                order.push(email.to_string());
            }
        }
        let mut moved = moved.into_iter();
        for slot in order.iter_mut().filter(|slot| is_shown(slot)) {
            if let Some(email) = moved.next() {
                *slot = email;
            }
        }
        self.account_order = order;
    }

    /// Inserts one live account before or after another without changing
    /// its identity. `emails` are the accounts the list on screen shows;
    /// the others keep their places.
    pub fn reorder_account(&mut self, emails: &[&str], email: &str, target: &str, after: bool) {
        if email.eq_ignore_ascii_case(target) {
            return;
        }
        let mut order: Vec<String> = self
            .ordered(emails)
            .into_iter()
            .map(str::to_string)
            .collect();
        let Some(from) = order
            .iter()
            .position(|entry| entry.eq_ignore_ascii_case(email))
        else {
            return;
        };
        if !order.iter().any(|entry| entry.eq_ignore_ascii_case(target)) {
            return;
        }
        let moved = order.remove(from);
        let Some(to) = order
            .iter()
            .position(|entry| entry.eq_ignore_ascii_case(target))
        else {
            return;
        };
        order.insert(to + usize::from(after), moved);
        self.place_shown(emails, order);
    }
}

/// The accounts a picker for `task` offers, in the order given. Only
/// calendar events can go to a calendar account; every other task, and
/// the default account over them all (`None`), sends or reads mail.
pub fn accounts_for(task: Option<AccountUse>, accounts: &[mailrs_domain::Account]) -> Vec<mailrs_domain::Account> {
    match task {
        Some(AccountUse::Calendar) => accounts.to_vec(),
        None
        | Some(
            AccountUse::NewMessages
            | AccountUse::Replies
            | AccountUse::Forwards
            | AccountUse::Assistant
            | AccountUse::Startup,
        ) => mailrs_domain::mail_accounts(accounts),
    }
}

/// Chooses a visible writable calendar from an ordered list of eligible calendars.
pub fn preferred_calendar(
    writable: &[(
        mailrs_domain::AccountId,
        String,
        mailrs_domain::calendar::Calendar,
    )],
    preferred: Option<&str>,
    last: Option<&str>,
) -> Option<(mailrs_domain::AccountId, mailrs_domain::calendar::Calendar)> {
    let primary = |email: Option<&str>| {
        writable.iter().find(|(_, address, calendar)| {
            calendar.primary && email.is_none_or(|email| address.eq_ignore_ascii_case(email))
        })
    };
    preferred
        .and_then(|email| {
            primary(Some(email)).or_else(|| {
                writable
                    .iter()
                    .find(|(_, address, _)| address.eq_ignore_ascii_case(email))
            })
        })
        .or_else(|| last.and_then(|email| primary(Some(email))))
        .or_else(|| primary(None))
        .or_else(|| writable.first())
        .map(|(account, _, calendar)| (*account, calendar.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_calendar_events_can_go_to_a_calendar_account() {
        let account = |id, provider| mailrs_domain::Account {
            id,
            email: format!("{id}@example.org"),
            state: mailrs_domain::AccountState::Ok,
            provider,
            provider_name: None,
        };
        let all = [account(1, mailrs_domain::Provider::Imap), account(2, mailrs_domain::Provider::CalDav)];
        let ids = |task| accounts_for(task, &all).iter().map(|a| a.id).collect::<Vec<_>>();
        assert_eq!(ids(Some(AccountUse::Calendar)), [1, 2]);
        assert_eq!(ids(None), [1], "the default account writes mail");
        for task in AccountUse::ALL.into_iter().filter(|task| *task != AccountUse::Calendar) {
            assert_eq!(ids(Some(task)), [1], "{task:?}");
        }
    }

    #[test]
    fn moving_the_accounts_a_list_shows_keeps_the_others_in_place() {
        let saved = |settings: &Settings| settings.account_order.join(" ");
        let order = |emails: &[&str]| Settings { account_order: emails.iter().map(|e| e.to_string()).collect(), ..Settings::default() };
        let mut settings = order(&["a", "cal", "b", "c"]);
        let shown = ["a", "b", "c"];
        settings.move_account(&shown, "b", -1);
        assert_eq!(saved(&settings), "b cal a c", "the calendar account keeps its place");
        settings.reorder_account(&shown, "c", "b", false);
        assert_eq!(saved(&settings), "c cal b a");

        // One the saved order never listed joins it at the end.
        let mut settings = order(&["cal", "a"]);
        settings.move_account(&["a", "new"], "new", -1);
        assert_eq!(saved(&settings), "cal new a");
    }

    #[test]
    fn a_removed_assistant_default_follows_the_new_message_choice() {
        let mut settings = Settings::default();
        settings.account_defaults.assistant = AccountChoice::Account("removed@example.com".into());
        settings.account_defaults.new_messages = AccountChoice::Account("work@example.com".into());
        assert_eq!(settings.preferred_live_account(AccountUse::Assistant, &["personal@example.com", "work@example.com"]), Some("work@example.com"));
    }

    #[test]
    fn task_overrides_do_not_change_other_defaults() {
        let mut settings = Settings {
            default_account: Some("personal@example.com".into()),
            ..Settings::default()
        };
        settings.account_defaults.replies = AccountChoice::Account("work@example.com".into());
        assert_eq!(
            settings.preferred_account(AccountUse::Replies),
            Some("work@example.com")
        );
        assert_eq!(
            settings.preferred_account(AccountUse::NewMessages),
            Some("personal@example.com")
        );
        assert_eq!(settings.preferred_account(AccountUse::Calendar), None);
    }

    #[test]
    fn one_default_can_cover_all_tasks_without_erasing_overrides() {
        let mut settings = Settings {
            default_account: Some("personal@example.com".into()),
            ..Settings::default()
        };
        settings.account_defaults.replies = AccountChoice::Account("work@example.com".into());
        settings.account_defaults.use_for_all = true;
        for task in AccountUse::ALL {
            assert_eq!(
                settings.preferred_account(task),
                Some("personal@example.com")
            );
        }
        settings.account_defaults.use_for_all = false;
        assert_eq!(
            settings.preferred_account(AccountUse::Replies),
            Some("work@example.com")
        );
    }

    #[test]
    fn automatic_assistant_follows_new_messages_and_replies_keep_context() {
        let mut settings = Settings::default();
        settings.account_defaults.new_messages = AccountChoice::Account("work@example.com".into());
        assert_eq!(
            settings.preferred_account(AccountUse::Assistant),
            Some("work@example.com")
        );
        assert_eq!(settings.preferred_account(AccountUse::Replies), None);
        assert_eq!(settings.preferred_account(AccountUse::Startup), None);
    }

    #[test]
    fn existing_files_keep_their_sending_default_and_automatic_replies() {
        let settings: Settings = toml::from_str("default_account = 'work@example.com'").unwrap();
        assert_eq!(
            settings.preferred_account(AccountUse::NewMessages),
            Some("work@example.com")
        );
        assert_eq!(settings.preferred_account(AccountUse::Replies), None);
        assert_eq!(settings.preferred_account(AccountUse::Calendar), None);
    }

    #[test]
    fn account_choices_survive_saving_and_reopening() {
        let mut settings = Settings::default();
        settings.account_defaults.replies = AccountChoice::Account("work@example.com".into());
        let saved = toml::to_string(&settings).unwrap();
        let reopened: Settings = toml::from_str(&saved).unwrap();
        assert_eq!(
            reopened.account_defaults.replies,
            AccountChoice::Account("work@example.com".into())
        );
    }

    #[test]
    fn a_drop_moves_an_account_without_swapping_the_intervening_accounts() {
        let mut settings = Settings {
            account_order: vec!["a@x".into(), "b@x".into(), "c@x".into()],
            ..Settings::default()
        };
        settings.reorder_account(&["a@x", "b@x", "c@x", "new@x"], "A@X", "c@x", true);
        assert_eq!(settings.account_order, ["b@x", "c@x", "a@x", "new@x"]);
        settings.reorder_account(&["a@x", "b@x", "c@x", "new@x"], "new@x", "b@x", false);
        assert_eq!(settings.account_order, ["new@x", "b@x", "c@x", "a@x"]);
    }

    #[test]
    fn a_foreign_or_self_drop_leaves_account_order_alone() {
        let mut settings = Settings {
            account_order: vec!["a@x".into(), "b@x".into()],
            ..Settings::default()
        };
        settings.reorder_account(&["a@x", "b@x"], "other@x", "a@x", false);
        settings.reorder_account(&["a@x", "b@x"], "a@x", "other@x", true);
        settings.reorder_account(&["a@x", "b@x"], "a@x", "A@X", false);
        assert_eq!(settings.account_order, ["a@x", "b@x"]);
    }
}
