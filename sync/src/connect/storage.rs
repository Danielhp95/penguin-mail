//! Mail quotas beside JMAP when the provider exposes them only over IMAP.

use std::sync::Arc;

use mailrs_discover::{Security, Server, UserName};
use mailrs_imap::{ImapClient, Login};

use crate::services::jmap::StorageFallback;

/// Fastmail's OAuth grant also authenticates IMAP. Send it only to the
/// documented IMAP host, for a session at Fastmail's documented JMAP URL.
pub(super) fn fastmail(
    session_url: &str,
    tokens: Option<Arc<dyn mailrs_dav::TokenSource>>,
) -> Option<StorageFallback> {
    if session_url != "https://api.fastmail.com/jmap/session" {
        return None;
    }
    let tokens = tokens?;
    Some(Arc::new(move |username| {
        let tokens = Arc::clone(&tokens);
        Box::pin(async move {
            let token = tokens
                .token(None)
                .await
                .map_err(|_| crate::BackendError::Offline(String::new()))?;
            let client = ImapClient::new(
                Server {
                    host: "imap.fastmail.com".into(),
                    port: 993,
                    security: Security::Tls,
                    user_name: UserName::Address,
                },
                Login::bearer(username, token),
            );
            if !client.capabilities().await?.quota {
                return Ok(None);
            }
            let quotas = client.quota_root("INBOX").await?;
            Ok(crate::services::imap::storage::from_quotas(&quotas))
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tokens;
    impl mailrs_dav::TokenSource for Tokens {
        fn token<'a>(
            &'a self,
            _: Option<&'a str>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
        {
            Box::pin(async { panic!("a foreign session must not read credentials") })
        }
    }

    #[test]
    fn a_foreign_jmap_session_never_sends_its_token_to_fastmail() {
        assert!(
            fastmail(
                "https://api.fastmail.com.attacker.example/jmap/session",
                Some(Arc::new(Tokens))
            )
            .is_none()
        );
    }

    #[test]
    fn a_fastmail_session_without_oauth_has_no_credential_fallback() {
        assert!(fastmail("https://api.fastmail.com/jmap/session", None).is_none());
    }
}
