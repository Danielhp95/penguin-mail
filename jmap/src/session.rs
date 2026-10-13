//! The session resource (RFC 8620 section 2): what the server offers, the
//! account that holds each kind of data, the URLs every other call goes
//! to, and the limits a request keeps under.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::JmapError;
use crate::error::decode;

pub const CORE: &str = "urn:ietf:params:jmap:core";
pub const MAIL: &str = "urn:ietf:params:jmap:mail";
pub const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
pub const VACATION: &str = "urn:ietf:params:jmap:vacationresponse";
pub const CONTACTS: &str = "urn:ietf:params:jmap:contacts";
pub const SIEVE: &str = "urn:ietf:params:jmap:sieve";
/// JMAP Calendars, a draft; offered only in draft 32's shape (`calendars::fits`).
pub const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
/// Fastmail's Masked Email, documented at fastmail.com/dev.
pub const MASKED_EMAIL: &str = "https://www.fastmail.com/dev/maskedemail";

/// The most requests the client keeps in flight for one account, whatever
/// the server allows, so a busy account holds little memory.
pub const MOST_AT_ONCE: usize = 4;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub capabilities: BTreeMap<String, Value>,
    #[serde(default)]
    pub accounts: BTreeMap<String, SessionAccount>,
    #[serde(default)]
    pub primary_accounts: BTreeMap<String, String>,
    #[serde(default)]
    pub username: String,
    pub api_url: String,
    pub download_url: String,
    pub upload_url: String,
    #[serde(default)]
    pub event_source_url: Option<String>,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAccount {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_personal: bool,
    #[serde(default)]
    pub is_read_only: bool,
    #[serde(default)]
    pub account_capabilities: BTreeMap<String, Value>,
}

/// What a request must keep under, from the core capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_size_upload: u64,
    pub max_concurrent_upload: usize,
    pub max_size_request: u64,
    pub max_concurrent_requests: usize,
    pub max_calls_in_request: usize,
    pub max_objects_in_get: usize,
    pub max_objects_in_set: usize,
}

impl Limits {
    /// For a server that names none: small enough for any server seen.
    pub const FALLBACK: Limits = Limits {
        max_size_upload: 10_000_000,
        max_concurrent_upload: 1,
        max_size_request: 1_000_000,
        max_concurrent_requests: 1,
        max_calls_in_request: 16,
        max_objects_in_get: 256,
        max_objects_in_set: 128,
    };
}

impl Session {
    pub fn parse(bytes: &[u8]) -> Result<Session, JmapError> {
        serde_json::from_slice(bytes).map_err(|e| decode("the session", &e))
    }

    /// The account that holds `capability`'s data, from `primaryAccounts`.
    /// Never the first account: Fastmail can keep contacts in an account
    /// of their own. `None` when the session or that account lacks it.
    pub fn account_for(&self, capability: &str) -> Option<&str> {
        if !self.capabilities.contains_key(capability) {
            return None;
        }
        let id = self.primary_accounts.get(capability)?;
        self.accounts
            .get(id)
            .filter(|account| account.account_capabilities.contains_key(capability))
            // A calendars capability in an older draft's shape is one this
            // client cannot read, so it counts as not offered.
            .filter(|account| {
                capability != CALENDARS
                    || account.account_capabilities.get(CALENDARS).is_some_and(crate::calendars::fits)
            })
            .map(|_| id.as_str())
    }

    pub fn offers(&self, capability: &str) -> bool {
        self.account_for(capability).is_some()
    }

    /// What `capability` says of its primary account, such as the mail
    /// account's `maxMailboxesPerEmail`.
    pub fn account_capability(&self, capability: &str) -> Option<&Value> {
        let id = self.account_for(capability)?;
        self.accounts.get(id)?.account_capabilities.get(capability)
    }

    pub fn limits(&self) -> Limits {
        let core = self.capabilities.get(CORE);
        let number = |name: &str, fallback: u64| {
            core.and_then(|c| c.get(name)).and_then(Value::as_u64).filter(|n| *n > 0).unwrap_or(fallback)
        };
        let count = |name: &str, fallback: usize| {
            usize::try_from(number(name, fallback as u64)).unwrap_or(fallback)
        };
        let fallback = Limits::FALLBACK;
        Limits {
            max_size_upload: number("maxSizeUpload", fallback.max_size_upload),
            max_concurrent_upload: count("maxConcurrentUpload", fallback.max_concurrent_upload),
            max_size_request: number("maxSizeRequest", fallback.max_size_request),
            max_concurrent_requests: count("maxConcurrentRequests", fallback.max_concurrent_requests),
            max_calls_in_request: count("maxCallsInRequest", fallback.max_calls_in_request),
            max_objects_in_get: count("maxObjectsInGet", fallback.max_objects_in_get),
            max_objects_in_set: count("maxObjectsInSet", fallback.max_objects_in_set),
        }
    }

    /// The mail account's `maxMailboxesPerEmail`: `None` when the server
    /// sets no limit (null or absent), as Stalwart does.
    pub fn mailboxes_per_email(&self) -> Option<u64> {
        self.account_capability(MAIL)?.get("maxMailboxesPerEmail")?.as_u64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FASTMAIL: &str = include_str!("../tests/data/fastmail-session.json");
    const RFC: &str = include_str!("../tests/data/rfc8620-session.json");

    #[test]
    fn the_rfc_example_parses_with_its_urls_and_state() {
        let session = Session::parse(RFC.as_bytes()).unwrap();
        assert_eq!(session.api_url, "https://jmap.example.com/api/");
        assert_eq!(session.state, "75128aab4b1b");
        assert_eq!(session.username, "john@example.com");
        assert!(session.event_source_url.is_some());
    }

    #[test]
    fn each_capability_reads_its_own_primary_account() {
        let session = Session::parse(FASTMAIL.as_bytes()).unwrap();
        assert_eq!(session.account_for(MAIL), Some("u1a2b3c4"));
        assert_eq!(session.account_for(CONTACTS), Some("c9d8e7f6"));
        assert_eq!(session.account_for(MASKED_EMAIL), Some("u1a2b3c4"));
    }

    #[test]
    fn a_capability_the_token_lacks_is_not_offered() {
        let session = Session::parse(FASTMAIL.as_bytes()).unwrap();
        assert!(session.offers(SUBMISSION));
        assert!(!session.offers(VACATION), "Fastmail tokens cannot reach the automatic reply");
        assert!(!session.offers(SIEVE));
        assert!(!session.offers(CALENDARS));
    }

    #[test]
    fn a_primary_account_without_the_capability_does_not_count() {
        let mut session = Session::parse(FASTMAIL.as_bytes()).unwrap();
        session.primary_accounts.insert(CONTACTS.into(), "u1a2b3c4".into());
        assert_eq!(session.account_for(CONTACTS), None);
    }

    #[test]
    fn the_limits_come_from_the_core_capability() {
        let limits = Session::parse(FASTMAIL.as_bytes()).unwrap().limits();
        assert_eq!(limits.max_calls_in_request, 50);
        assert_eq!(limits.max_objects_in_get, 4096);
        assert_eq!(limits.max_size_request, 10_000_000);
        assert_eq!(limits.max_concurrent_requests, 10);
    }

    #[test]
    fn missing_limits_fall_back_to_small_ones() {
        let mut session = Session::parse(RFC.as_bytes()).unwrap();
        session.capabilities.insert(CORE.into(), serde_json::json!({}));
        assert_eq!(session.limits(), Limits::FALLBACK);
    }

    #[test]
    fn mailboxes_per_email_reads_the_mail_account_and_null_means_no_limit() {
        let session = Session::parse(FASTMAIL.as_bytes()).unwrap();
        assert_eq!(session.mailboxes_per_email(), Some(1000));
        let rfc = Session::parse(RFC.as_bytes()).unwrap();
        assert_eq!(rfc.mailboxes_per_email(), None);
    }

    #[test]
    fn a_session_that_does_not_fit_does_not_repeat_its_values() {
        let said = Session::parse(br#"{"username": ["secret@example.org"]}"#).unwrap_err().to_string();
        assert!(!said.contains("secret@example.org"), "{said}");
    }

    #[test]
    fn a_body_that_is_not_a_session_says_so() {
        assert!(matches!(Session::parse(b"<html>"), Err(JmapError::Decode(_))));
    }

    fn with_calendars(shape: serde_json::Value) -> Session {
        let mut session = Session::parse(FASTMAIL.as_bytes()).unwrap();
        session.capabilities.insert(CALENDARS.into(), serde_json::json!({}));
        session.primary_accounts.insert(CALENDARS.into(), "u1a2b3c4".into());
        session.accounts.get_mut("u1a2b3c4").unwrap().account_capabilities.insert(CALENDARS.into(), shape);
        session
    }

    #[test]
    fn calendars_in_draft_32_s_shape_are_offered() {
        let session = with_calendars(serde_json::json!({
            "maxCalendarsPerEvent": null, "minDateTime": "1900-01-01T00:00:00Z", "maxDateTime": "2200-01-01T00:00:00Z",
            "maxExpandedQueryDuration": "P366D", "maxParticipantsPerEvent": null, "mayCreateCalendar": true
        }));
        assert_eq!(session.account_for(CALENDARS), Some("u1a2b3c4"));
    }

    #[test]
    fn an_older_calendars_shape_is_not_offered() {
        // Draft 20 and earlier wrote these as LocalDateTime.
        let session = with_calendars(serde_json::json!({
            "maxCalendarsPerEvent": null, "minDateTime": "1900-01-01T00:00:00", "maxDateTime": "2200-01-01T00:00:00",
            "maxExpandedQueryDuration": "P366D", "maxParticipantsPerEvent": null, "mayCreateCalendar": true
        }));
        assert!(!session.offers(CALENDARS));
        assert!(!with_calendars(serde_json::json!({})).offers(CALENDARS), "an empty object names nothing");
    }
}
