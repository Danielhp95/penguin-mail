//! Masked Email: addresses a provider makes for one site each and
//! delivers to the account, which the person can turn off or delete. Only
//! Fastmail offers it, as `https://www.fastmail.com/dev/maskedemail`; the
//! account's session decides, never its name.

use mailrs_domain::{Address, EpochMillis};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskedState {
    /// Made and not used yet; the server deletes it after a day without
    /// mail.
    Pending,
    Enabled,
    /// Mail to it is dropped.
    Disabled,
    Deleted,
}

impl MaskedState {
    pub fn as_str(self) -> &'static str {
        match self {
            MaskedState::Pending => "pending",
            MaskedState::Enabled => "enabled",
            MaskedState::Disabled => "disabled",
            MaskedState::Deleted => "deleted",
        }
    }

    pub fn parse(said: &str) -> Option<MaskedState> {
        [MaskedState::Pending, MaskedState::Enabled, MaskedState::Disabled, MaskedState::Deleted]
            .into_iter()
            .find(|s| s.as_str() == said)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskedAddress {
    pub id: String,
    pub email: String,
    pub state: MaskedState,
    /// The site it was made for.
    pub site: String,
    pub note: String,
    pub created: Option<EpochMillis>,
    pub last_mail: Option<EpochMillis>,
}

/// Which of `mine`, the account's own addresses and its masked ones, a
/// message was delivered to: the first of its To and Cc addresses,
/// `written_to`, that is one of them, compared without case. A reply goes
/// out from it, and so does a read receipt, so a sender who wrote to a
/// masked address never learns the real one. `None` when the message reached none of them, as
/// mail through a list or a Bcc does.
pub fn delivered_to<'a>(written_to: impl IntoIterator<Item = &'a Address>, mine: &[String]) -> Option<String> {
    written_to
        .into_iter()
        .find(|wrote_to| mine.iter().any(|m| m.eq_ignore_ascii_case(&wrote_to.email)))
        .map(|wrote_to| wrote_to.email.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to(addresses: &[&str]) -> Vec<Address> {
        addresses.iter().map(|email| Address { name: None, email: email.to_string() }).collect()
    }

    #[test]
    fn mail_is_delivered_to_the_first_of_my_addresses_it_names() {
        let mine = ["me@example.com".to_string(), "shop.x1@example.com".to_string()];
        assert_eq!(delivered_to(&to(&["ann@example.com", "Shop.X1@Example.com"]), &mine).as_deref(), Some("Shop.X1@Example.com"));
        assert_eq!(delivered_to(&to(&["list@lists.example"]), &mine), None);
    }

    #[test]
    fn states_round_trip_and_an_unknown_one_is_none() {
        for state in [MaskedState::Pending, MaskedState::Enabled, MaskedState::Disabled, MaskedState::Deleted] {
            assert_eq!(MaskedState::parse(state.as_str()), Some(state));
        }
        assert_eq!(MaskedState::parse("frozen"), None);
    }
}
