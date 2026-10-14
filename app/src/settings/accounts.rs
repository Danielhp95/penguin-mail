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

    /// Inserts one live account before or after another without changing its identity.
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
        self.account_order = order;
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
