//! Masked Email: addresses a provider makes for one site each and
//! delivers to the account, which the person can turn off or delete. Only
//! Fastmail offers it, as `https://www.fastmail.com/dev/maskedemail`; the
//! account's session decides, never its name.

use mailrs_domain::EpochMillis;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_and_an_unknown_one_is_none() {
        for state in [MaskedState::Pending, MaskedState::Enabled, MaskedState::Disabled, MaskedState::Deleted] {
            assert_eq!(MaskedState::parse(state.as_str()), Some(state));
        }
        assert_eq!(MaskedState::parse("frozen"), None);
    }
}
