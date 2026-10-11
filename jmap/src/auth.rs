//! What signs a JMAP account in: the account password over Basic, or a
//! token over Bearer, as the server takes them. Part 5 adds OAuth, whose
//! header needs a refresh now and then, which is why `header` is async.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::HeaderValue;

use crate::JmapError;

#[derive(Clone)]
pub enum Auth {
    /// The account's own password, as Stalwart and James take it.
    Basic { user: String, password: String },
    /// A token the person made on the provider's site, as Fastmail's API
    /// tokens are.
    Bearer(String),
}

/// How an account signs in, as the store keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Basic,
    Bearer,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthKind::Basic => "basic",
            AuthKind::Bearer => "bearer",
        }
    }

    pub fn parse(stored: &str) -> Option<AuthKind> {
        [AuthKind::Basic, AuthKind::Bearer].into_iter().find(|k| k.as_str() == stored)
    }
}

// By hand, so neither the password nor the token reaches a log line.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Basic { user, .. } => f.debug_struct("Basic").field("user", user).finish_non_exhaustive(),
            Auth::Bearer(_) => f.write_str("Bearer(..)"),
        }
    }
}

impl Auth {
    pub fn kind(&self) -> AuthKind {
        match self {
            Auth::Basic { .. } => AuthKind::Basic,
            Auth::Bearer(_) => AuthKind::Bearer,
        }
    }

    /// The `Authorization` header. `force` asks a refreshing credential
    /// for a new one; a password or a fixed token ignores it.
    // Nothing calls it until the HTTP client lands; that task drops this.
    #[allow(dead_code)]
    pub(crate) async fn header(&self, force: bool) -> Result<HeaderValue, JmapError> {
        let _ = force;
        let text = match self {
            Auth::Basic { user, password } => format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
            Auth::Bearer(token) => format!("Bearer {token}"),
        };
        let mut value = HeaderValue::from_str(&text).map_err(|_| JmapError::BadCredential)?;
        value.set_sensitive(true);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn basic_sends_the_user_and_password_in_base64() {
        let auth = Auth::Basic { user: "me@example.org".into(), password: "pw".into() };
        let header = auth.header(false).await.unwrap();
        assert_eq!(header.to_str().unwrap(), "Basic bWVAZXhhbXBsZS5vcmc6cHc=");
        assert!(header.is_sensitive());
    }

    #[tokio::test]
    async fn bearer_sends_the_token() {
        let header = Auth::Bearer("fmu1-abc".into()).header(false).await.unwrap();
        assert_eq!(header.to_str().unwrap(), "Bearer fmu1-abc");
    }

    #[tokio::test]
    async fn a_token_a_header_cannot_carry_is_refused_not_a_panic() {
        let refused = Auth::Bearer("fmu1-abc\n".into()).header(false).await;
        assert_eq!(refused, Err(JmapError::BadCredential));
    }

    #[test]
    fn debug_shows_neither_password_nor_token() {
        let basic = format!("{:?}", Auth::Basic { user: "me".into(), password: "hunter2".into() });
        let bearer = format!("{:?}", Auth::Bearer("fmu1-secret".into()));
        assert!(!basic.contains("hunter2") && basic.contains("me"), "{basic}");
        assert!(!bearer.contains("fmu1-secret"), "{bearer}");
    }

    #[test]
    fn the_kind_round_trips_through_its_stored_form() {
        for kind in [AuthKind::Basic, AuthKind::Bearer] {
            assert_eq!(AuthKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(AuthKind::parse("digest"), None);
    }
}
