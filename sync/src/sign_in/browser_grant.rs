//! A browser sign-in that keeps nothing: the grant alone, for an account
//! that already exists under another protocol and switches to JMAP. The
//! switch decides what to keep, in one transaction with the rest of the
//! account, so nothing here touches the store or the keyring. It runs the
//! same steps as [`super::jmap_in_browser`]: a client registered for the
//! loopback redirect this sign-in listens on, the consent page, the code exchange, and the
//! `state` and `iss` checks the loopback makes.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use mailrs_jmap::oauth::{self, Loopback, Metadata, OAuthClient, Pkce};
use mailrs_store::jmap::SavedOAuth;

use super::BrowserSignInError;
use super::jmap_browser::{BROWSER_WAIT, JmapBrowser, SignInServer};
use mailrs_jmap::JmapError;

/// What a browser sign-in handed back. The tokens never show in Debug.
pub struct BrowserGrant {
    pub client: SavedOAuth,
    /// For the checks right after the sign-in. The account's session
    /// refreshes from `refresh_token` once the keyring holds it.
    pub access_token: String,
    pub refresh_token: String,
}

impl fmt::Debug for BrowserGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserGrant").field("client", &self.client).finish_non_exhaustive()
    }
}

struct Live;

impl SignInServer for Live {
    async fn metadata(&self, session_url: &str) -> Result<Metadata, JmapError> {
        oauth::metadata(session_url).await
    }

    async fn register(&self, metadata: &Metadata, redirect: &str, scopes: &[String]) -> Result<String, JmapError> {
        oauth::register(metadata, redirect, scopes).await
    }

    async fn exchange(&self, client: &OAuthClient, code: &str, redirect: &str, pkce: &Pkce) -> Result<oauth::Tokens, JmapError> {
        client.exchange(code, redirect, pkce).await
    }

    fn revoke(&self, grant: crate::JmapGrant) {
        tokio::spawn(grant.revoke());
    }
}

/// Signs in on the JMAP server's own page and hands back the grant.
/// `address` fills the page's address field. The wait for the browser
/// ends after [`BROWSER_WAIT`], or when `canceled` resolves.
pub async fn browser_grant(
    offer: JmapBrowser,
    address: &str,
    open: impl Fn(&str),
    canceled: impl Future<Output = ()>,
) -> Result<BrowserGrant, BrowserSignInError> {
    granting(&Live, offer, address, open, canceled, BROWSER_WAIT).await
}

pub(crate) async fn granting<S: SignInServer>(
    server: &S,
    offer: JmapBrowser,
    address: &str,
    open: impl Fn(&str),
    canceled: impl Future<Output = ()>,
    wait: Duration,
) -> Result<BrowserGrant, BrowserSignInError> {
    let metadata = match server.metadata(&offer.session_url).await {
        Ok(found) if found.registration_endpoint.is_some() => found,
        _ => return Err(BrowserSignInError::NotOffered),
    };
    let scopes = match &offer.provider {
        Some(provider) if !provider.oauth_scopes.is_empty() => provider.oauth_scopes.clone(),
        _ => oauth::oauth_scopes(&metadata),
    };
    // The client names the loopback's exact redirect, port included, as
    // `jmap_in_browser`'s does: Fastmail matches one registered without a
    // port exactly.
    let loopback = Loopback::bind().await.map_err(BrowserSignInError::Jmap)?;
    let redirect = loopback.redirect_uri.clone();
    let client_id = server.register(&metadata, &redirect, &scopes).await.map_err(BrowserSignInError::Jmap)?;
    let (pkce, state) = (Pkce::generate(), oauth::random_token(32));
    let url = oauth::authorize_url(&metadata, &client_id, &redirect, &scopes, &pkce, &state, &offer.session_url, Some(address))
        .map_err(BrowserSignInError::Jmap)?;
    open(&url);
    // The listener moves into the wait and closes when it ends.
    let code = tokio::select! {
        code = tokio::time::timeout(wait, loopback.wait_for_code(&state, &metadata)) => match code {
            Ok(code) => code.map_err(BrowserSignInError::Jmap)?,
            Err(_) => return Err(BrowserSignInError::GaveUp),
        },
        () = canceled => return Err(BrowserSignInError::Canceled),
    };
    let client = OAuthClient { client_id: client_id.clone(), token_endpoint: metadata.token_endpoint.clone(), resource: offer.session_url.clone() };
    let tokens = server.exchange(&client, &code, &redirect, &pkce).await.map_err(BrowserSignInError::Jmap)?;
    let refresh_token = tokens.refresh_token.ok_or(BrowserSignInError::NoRefreshToken)?;
    Ok(BrowserGrant {
        client: SavedOAuth { client_id, token_endpoint: metadata.token_endpoint },
        access_token: tokens.access_token,
        refresh_token,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mailrs_jmap::oauth::Tokens;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Answers from memory: no request leaves the test.
    struct FakeServer {
        registered: AtomicUsize,
        refresh: Option<&'static str>,
        /// The redirects the client was registered with and the exchange
        /// named, in order.
        redirects: Mutex<Vec<String>>,
    }

    impl FakeServer {
        fn giving(refresh: Option<&'static str>) -> FakeServer {
            FakeServer { registered: AtomicUsize::new(0), refresh, redirects: Mutex::new(Vec::new()) }
        }
    }

    impl SignInServer for FakeServer {
        async fn metadata(&self, _session_url: &str) -> Result<Metadata, JmapError> {
            Ok(Metadata {
                issuer: "https://api.fastmail.com".into(),
                authorization_endpoint: "https://api.fastmail.com/oauth/authorize".into(),
                token_endpoint: "https://api.fastmail.com/oauth/refresh".into(),
                registration_endpoint: Some("https://api.fastmail.com/oauth/register".into()),
                scopes_supported: vec!["urn:ietf:params:oauth:scope:mail".into(), "offline_access".into()],
                code_challenge_methods_supported: vec!["S256".into()],
                authorization_response_iss_parameter_supported: true,
                revocation_endpoint: None,
            })
        }

        async fn register(&self, _metadata: &Metadata, redirect: &str, _scopes: &[String]) -> Result<String, JmapError> {
            self.redirects.lock().unwrap().push(redirect.to_string());
            Ok(format!("client-{}", self.registered.fetch_add(1, Ordering::SeqCst) + 1))
        }

        async fn exchange(&self, client: &OAuthClient, code: &str, redirect: &str, _pkce: &Pkce) -> Result<Tokens, JmapError> {
            self.redirects.lock().unwrap().push(redirect.to_string());
            assert_eq!((code, client.client_id.as_str()), ("c1", "client-1"));
            Ok(Tokens { access_token: "a1".into(), expires_in: 3600, refresh_token: self.refresh.map(str::to_string) })
        }

        fn revoke(&self, _grant: crate::JmapGrant) {}
    }

    fn query_value(url: &str, name: &str) -> String {
        let start = url.find(&format!("{name}=")).unwrap() + name.len() + 1;
        url[start..].split('&').next().unwrap().replace("%3A", ":").replace("%2F", "/")
    }

    /// Plays the browser: comes back to the loopback with the code `c1`.
    fn browser(pages: &Mutex<Vec<String>>) -> impl Fn(&str) + '_ {
        move |url: &str| {
            pages.lock().unwrap().push(url.to_string());
            let (state, redirect) = (query_value(url, "state"), query_value(url, "redirect_uri"));
            let address = redirect.trim_start_matches("http://").trim_end_matches('/').to_string();
            tokio::spawn(async move {
                let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
                let request = format!("GET /?code=c1&state={state}&iss=https%3A%2F%2Fapi.fastmail.com HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut page = Vec::new();
                let _ = stream.read_to_end(&mut page).await;
            });
        }
    }

    fn offer() -> JmapBrowser {
        JmapBrowser { session_url: "https://api.fastmail.com/jmap/session".into(), provider: None }
    }

    #[tokio::test]
    async fn a_grant_registers_a_client_and_hands_back_its_tokens() {
        let server = FakeServer::giving(Some("r1"));
        let pages = Mutex::new(Vec::new());
        let grant = granting(&server, offer(), "me@fastmail.com", browser(&pages), std::future::pending(), Duration::from_secs(5)).await.unwrap();
        assert_eq!(grant.client.client_id, "client-1");
        assert_eq!((grant.access_token.as_str(), grant.refresh_token.as_str()), ("a1", "r1"));
        let page = pages.lock().unwrap()[0].clone();
        assert!(page.contains("login_hint=me%40fastmail.com"), "{page}");
    }

    /// The registered redirect is the one the authorize request and the
    /// exchange name, port and all: Fastmail matches a redirect registered
    /// without a port exactly.
    #[tokio::test]
    async fn the_client_is_registered_for_the_redirect_the_browser_comes_back_to() {
        let server = FakeServer::giving(Some("r1"));
        let pages = Mutex::new(Vec::new());
        granting(&server, offer(), "me@fastmail.com", browser(&pages), std::future::pending(), Duration::from_secs(5)).await.unwrap();
        let asked = query_value(&pages.lock().unwrap()[0], "redirect_uri");
        assert!(asked.strip_prefix("http://127.0.0.1:").and_then(|rest| rest.strip_suffix('/')).and_then(|port| port.parse::<u16>().ok()).is_some_and(|p| p != 0), "{asked}");
        assert_eq!(*server.redirects.lock().unwrap(), [asked.clone(), asked]);
    }

    #[tokio::test]
    async fn a_grant_with_no_refresh_token_is_refused() {
        let server = FakeServer::giving(None);
        let pages = Mutex::new(Vec::new());
        let refused = granting(&server, offer(), "me@fastmail.com", browser(&pages), std::future::pending(), Duration::from_secs(5)).await;
        assert!(matches!(refused, Err(BrowserSignInError::NoRefreshToken)), "{refused:?}");
    }

    #[tokio::test]
    async fn a_canceled_grant_says_so() {
        let server = FakeServer::giving(Some("r1"));
        let refused = granting(&server, offer(), "me@fastmail.com", |_: &str| {}, std::future::ready(()), Duration::from_secs(5)).await;
        assert!(matches!(refused, Err(BrowserSignInError::Canceled)), "{refused:?}");
    }

    #[test]
    fn a_grant_never_prints_its_tokens() {
        let grant = BrowserGrant {
            client: SavedOAuth { client_id: "client-1".into(), token_endpoint: "https://api.fastmail.com/oauth/refresh".into() },
            access_token: "secret-access".into(),
            refresh_token: "secret-refresh".into(),
        };
        let shown = format!("{grant:?}");
        assert!(!shown.contains("secret"), "{shown}");
    }
}
