//! A Google client a person made in their own Google Cloud project and
//! names to Penguin Mail in two environment variables, for when the
//! built-in client cannot sign in. An account signed in with one keeps
//! it in the keyring, beside its refresh token, and refreshes through it
//! from then on, whether or not the variables are set at a later start.

use mailrs_domain::translate::{fill, gettext};
use mailrs_gmail::OAuthClient;
use serde::{Deserialize, Serialize};

/// The variable naming the own client's id. A release build reads the
/// same name at build time for the built-in client.
pub const ID_VAR: &str = "PENGUIN_MAIL_GOOGLE_CLIENT_ID";
/// The variable naming the own client's secret.
pub const SECRET_VAR: &str = "PENGUIN_MAIL_GOOGLE_CLIENT_SECRET";

/// An own Google client's id and secret. No `Debug`, so neither reaches
/// a log line.
#[derive(Clone, Serialize, Deserialize)]
pub struct OwnClient {
    client_id: String,
    client_secret: String,
}

impl OwnClient {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        OwnClient {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// The client to sign in and refresh tokens with.
    pub fn oauth(&self) -> OAuthClient {
        OAuthClient::new(&self.client_id, &self.client_secret)
    }

    /// The text the keyring keeps for an account.
    pub fn encode(&self) -> String {
        // Two strings always serialize.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The client from what the keyring kept, or `None` when the entry
    /// is not one this module wrote.
    pub fn decode(kept: &str) -> Option<OwnClient> {
        let own: OwnClient = serde_json::from_str(kept).ok()?;
        let blank = own.client_id.trim().is_empty() || own.client_secret.trim().is_empty();
        (!blank).then_some(own)
    }
}

/// The two variables do not name a client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OwnClientError {
    /// One is set and the other is not. Penguin Mail says so rather than
    /// sign in with the built-in client the person meant to avoid.
    #[error("{}", half(.set, .missing))]
    Half { set: &'static str, missing: &'static str },
}

fn half(set: &str, missing: &str) -> String {
    fill(
        &gettext(
            "{set} is set but {missing} is not. Set both to sign in with your own Google client, \
             or neither to use Penguin Mail's.",
        ),
        &[("set", set), ("missing", missing)],
    )
}

/// The own client two values name: `None` when neither is set, an error
/// when only one is. A blank value counts as unset.
pub fn from_values(id: Option<&str>, secret: Option<&str>) -> Option<Result<OwnClient, OwnClientError>> {
    let id = id.map(str::trim).filter(|v| !v.is_empty());
    let secret = secret.map(str::trim).filter(|v| !v.is_empty());
    match (id, secret) {
        (None, None) => None,
        (Some(id), Some(secret)) => Some(Ok(OwnClient::new(id, secret))),
        (Some(_), None) => Some(Err(OwnClientError::Half { set: ID_VAR, missing: SECRET_VAR })),
        (None, Some(_)) => Some(Err(OwnClientError::Half { set: SECRET_VAR, missing: ID_VAR })),
    }
}

/// The own client the environment names now, read at each sign-in.
pub fn from_env() -> Option<Result<OwnClient, OwnClientError>> {
    let id = std::env::var(ID_VAR).ok();
    let secret = std::env::var(SECRET_VAR).ok();
    from_values(id.as_deref(), secret.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_values_make_a_client() {
        let own = from_values(Some("own.apps.googleusercontent.com"), Some("GOCSPX-own"))
            .expect("both are set")
            .expect("both are valid");
        assert_eq!(own.oauth().id(), "own.apps.googleusercontent.com");
    }

    #[test]
    fn neither_value_means_no_own_client() {
        assert!(from_values(None, None).is_none());
    }

    #[test]
    fn blank_values_count_as_unset() {
        assert!(from_values(Some(" "), Some("")).is_none());
    }

    #[test]
    fn an_id_without_a_secret_names_the_missing_secret() {
        let Some(Err(half)) = from_values(Some("own.apps.googleusercontent.com"), None) else {
            panic!("one value alone is an error");
        };
        assert_eq!(half, OwnClientError::Half { set: ID_VAR, missing: SECRET_VAR });
    }

    #[test]
    fn a_secret_without_an_id_names_the_missing_id() {
        let Some(Err(half)) = from_values(Some(""), Some("GOCSPX-own")) else {
            panic!("one value alone is an error");
        };
        assert_eq!(half, OwnClientError::Half { set: SECRET_VAR, missing: ID_VAR });
    }

    #[test]
    fn the_error_names_both_variables() {
        let half = OwnClientError::Half { set: ID_VAR, missing: SECRET_VAR };
        assert_eq!(
            half.to_string(),
            "PENGUIN_MAIL_GOOGLE_CLIENT_ID is set but PENGUIN_MAIL_GOOGLE_CLIENT_SECRET is not. \
             Set both to sign in with your own Google client, or neither to use Penguin Mail's."
        );
    }

    #[test]
    fn a_kept_client_reads_back_as_it_was() {
        let own = OwnClient::new("own.apps.googleusercontent.com", "GOCSPX-own");
        let back = OwnClient::decode(&own.encode()).expect("it reads back");
        assert_eq!(back.oauth().id(), "own.apps.googleusercontent.com");
        assert_eq!(back.encode(), own.encode());
    }

    #[test]
    fn a_kept_entry_that_does_not_parse_is_no_client() {
        assert!(OwnClient::decode("not json").is_none());
        assert!(OwnClient::decode(r#"{"client_id":"","client_secret":"x"}"#).is_none());
    }
}
