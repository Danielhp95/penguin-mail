//! A JMAP account signed in on its server's own page. The server must
//! publish OAuth metadata (RFC 8414) beside its session, with open dynamic
//! registration: each sign-in registers a public client with the app's
//! loopback redirect and keeps its id with the account. Fastmail and
//! Stalwart both work this way, so no client id is built in.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use mailrs_discover::JmapProvider;
use mailrs_jmap::oauth::{self, Loopback, Metadata, OAuthClient, Pkce};
use mailrs_jmap::{Auth, JmapClient};
use mailrs_store::jmap::{JmapAuth, JmapSaved, SavedOAuth};

use super::{BrowserSignInError, NewJmap, SignedIn, jmap_signed_in};
use crate::{Connector, connect_jmap, now_millis};

/// How long the sign-in waits for the browser to come back: RFC 8252's
/// usual bound for a person at a consent page. The loopback listener
/// closes when it runs out.
pub const BROWSER_WAIT: Duration = Duration::from_secs(600);

/// The redirect a registered client names. RFC 8252 section 7.3 has the
/// server take any port on the loopback, and Fastmail and Stalwart do.
const REGISTERED_REDIRECT: &str = "http://127.0.0.1/";

/// A JMAP session offered for a browser sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JmapBrowser {
    pub session_url: String,
    /// The table's entry for the provider, when it has one.
    pub provider: Option<JmapProvider>,
}

/// Whether the browser sign-in can run for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserReady {
    /// The server's metadata, which offers dynamic registration.
    Registers(Metadata),
    NotOffered,
}

/// Whether the browser sign-in can run for `offer`: the server's metadata
/// must read, pass its checks and name a registration endpoint.
pub async fn browser_ready(offer: &JmapBrowser) -> BrowserReady {
    match oauth::metadata(&offer.session_url).await {
        Ok(found) if found.registration_endpoint.is_some() => BrowserReady::Registers(found),
        _ => BrowserReady::NotOffered,
    }
}

/// The scopes to ask for: the table's for a provider it lists, the mail
/// scopes the metadata lists otherwise.
fn scopes_for(offer: &JmapBrowser, metadata: &Metadata) -> Vec<String> {
    match &offer.provider {
        Some(provider) if !provider.oauth_scopes.is_empty() => provider.oauth_scopes.clone(),
        _ => oauth::oauth_scopes(metadata),
    }
}

/// The host a session URL names, as the account's provider name. The
/// metadata read already refused a URL with a user name in it.
fn host_of(session_url: &str) -> &str {
    let rest = session_url.split_once("://").map_or(session_url, |(_, rest)| rest);
    rest.split(['/', ':', '?', '#']).next().unwrap_or_default()
}

/// The code from the browser's redirect, or why there is none. The wait
/// ends at `wait` or when `canceled` resolves, and the loopback listener
/// closes with it either way.
async fn redirected(
    loopback: Loopback,
    state: &str,
    metadata: &Metadata,
    canceled: impl Future<Output = ()>,
    wait: Duration,
) -> Result<String, BrowserSignInError> {
    tokio::select! {
        code = tokio::time::timeout(wait, loopback.wait_for_code(state, metadata)) => match code {
            Ok(code) => code.map_err(BrowserSignInError::Jmap),
            Err(_) => Err(BrowserSignInError::GaveUp),
        },
        () = canceled => Err(BrowserSignInError::Canceled),
    }
}

/// The whole sign-in: a client registered for this install, the consent
/// page through `open`, the code exchange, the session check, then the
/// account kept with its refresh token and its services built. `address`
/// fills the server's address field. The wait for the browser ends after
/// [`BROWSER_WAIT`] or when `canceled` resolves.
pub async fn jmap_in_browser(
    connector: &Connector,
    offer: JmapBrowser,
    address: &str,
    open: impl FnOnce(&str),
    canceled: impl Future<Output = ()>,
) -> Result<SignedIn, BrowserSignInError> {
    let BrowserReady::Registers(metadata) = browser_ready(&offer).await else {
        return Err(BrowserSignInError::NotOffered);
    };
    let scopes = scopes_for(&offer, &metadata);
    let client_id = oauth::register(&metadata, REGISTERED_REDIRECT, &scopes)
        .await
        .map_err(BrowserSignInError::Jmap)?;
    let loopback = Loopback::bind().await.map_err(BrowserSignInError::Jmap)?;
    let redirect = loopback.redirect_uri.clone();
    let (pkce, state) = (Pkce::generate(), oauth::random_token(32));
    let url = oauth::authorize_url(&metadata, &client_id, &redirect, &scopes, &pkce, &state, &offer.session_url, Some(address))
        .map_err(BrowserSignInError::Jmap)?;
    open(&url);
    let code = redirected(loopback, &state, &metadata, canceled, BROWSER_WAIT).await?;
    let client = OAuthClient {
        client_id: client_id.clone(),
        token_endpoint: metadata.token_endpoint.clone(),
        resource: offer.session_url.clone(),
    };
    let tokens = client.exchange(&code, &redirect, &pkce).await.map_err(BrowserSignInError::Jmap)?;
    let refresh = tokens.refresh_token.ok_or(BrowserSignInError::NoRefreshToken)?;
    // The check sends the access token as it came. A session here could
    // refresh and rotate the refresh token before the keyring holds it,
    // and Fastmail revokes a grant whose old refresh token comes back.
    let api = JmapClient::new(&offer.session_url, Auth::Bearer(tokens.access_token)).map_err(BrowserSignInError::Jmap)?;
    let checked = mailrs_jmap::check(&api).await.map_err(BrowserSignInError::Jmap)?;
    let session = &checked.session;
    let provider_name = match &offer.provider {
        Some(provider) => provider.name.clone(),
        None => host_of(&offer.session_url).to_string(),
    };
    let new = NewJmap {
        address: checked.address.clone().unwrap_or_else(|| address.to_string()),
        provider_name,
        saved: JmapSaved {
            labels: session.mailboxes_per_email().is_none_or(|most| most > 1),
            capabilities: session.capabilities.keys().filter(|c| session.offers(c)).cloned().collect(),
            session_url: offer.session_url,
            auth: JmapAuth::OAuth,
            user_name: None,
            oauth: Some(SavedOAuth { client_id, token_endpoint: metadata.token_endpoint }),
        },
        secret: refresh,
        dav_password: None,
    };
    let Connector { db, config, secrets, .. } = connector;
    // The keyring takes the refresh token as the account is kept; when it
    // refuses, the new account goes again and the sign-in fails.
    let account = jmap_signed_in(db, secrets, new, now_millis()).await?;
    let window_days = config.engine_config().window_days;
    let services = connect_jmap(db, Arc::clone(&secrets.jmap), Arc::clone(&secrets.passwords), &account, window_days).await?;
    Ok(SignedIn { account, name: None, services })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn metadata() -> Metadata {
        Metadata {
            issuer: "https://api.fastmail.com".into(),
            authorization_endpoint: "https://api.fastmail.com/oauth/authorize".into(),
            token_endpoint: "https://api.fastmail.com/oauth/refresh".into(),
            registration_endpoint: Some("https://api.fastmail.com/oauth/register".into()),
            scopes_supported: vec!["urn:ietf:params:oauth:scope:mail".into(), "offline_access".into(), "openid".into()],
            code_challenge_methods_supported: vec!["S256".into()],
            authorization_response_iss_parameter_supported: true,
        }
    }

    /// The loopback's address, as the browser would reach it.
    fn address_of(loopback: &Loopback) -> String {
        loopback.redirect_uri.trim_start_matches("http://").trim_end_matches('/').to_string()
    }

    async fn closed(address: &str) -> bool {
        tokio::net::TcpStream::connect(address).await.is_err()
    }

    #[tokio::test]
    async fn the_browser_s_redirect_hands_back_its_code() {
        let loopback = Loopback::bind().await.unwrap();
        let address = address_of(&loopback);
        let browser = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
            let request = "GET /?code=c1&state=s1&iss=https%3A%2F%2Fapi.fastmail.com HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut page = Vec::new();
            let _ = stream.read_to_end(&mut page).await;
        });
        let code = redirected(loopback, "s1", &metadata(), std::future::pending(), Duration::from_secs(5)).await;
        assert_eq!(code.unwrap(), "c1");
        browser.await.unwrap();
    }

    #[tokio::test]
    async fn a_canceled_sign_in_closes_the_listener() {
        let loopback = Loopback::bind().await.unwrap();
        let address = address_of(&loopback);
        let canceled = redirected(loopback, "s1", &metadata(), std::future::ready(()), Duration::from_secs(600)).await;
        assert!(matches!(canceled, Err(BrowserSignInError::Canceled)));
        assert!(closed(&address).await, "the listener outlived the sign-in");
    }

    #[tokio::test]
    async fn a_sign_in_nobody_finishes_gives_up_and_closes_the_listener() {
        let loopback = Loopback::bind().await.unwrap();
        let address = address_of(&loopback);
        let waited = redirected(loopback, "s1", &metadata(), std::future::pending(), Duration::from_millis(50)).await;
        assert!(matches!(waited, Err(BrowserSignInError::GaveUp)));
        assert!(closed(&address).await, "the listener outlived the sign-in");
    }

    #[test]
    fn the_sign_in_waits_ten_minutes_for_the_browser() {
        assert_eq!(BROWSER_WAIT, Duration::from_secs(600));
    }

    #[test]
    fn a_server_outside_the_table_goes_by_its_host() {
        assert_eq!(host_of("https://mail.example.org:8443/jmap/session"), "mail.example.org");
        assert_eq!(host_of("https://mail.example.org/jmap/session"), "mail.example.org");
    }

    #[test]
    fn the_table_s_scopes_win_over_the_metadata_s() {
        let offer = JmapBrowser { session_url: "https://api.fastmail.com/jmap/session".into(), provider: mailrs_discover::jmap_named("Fastmail") };
        let scopes = scopes_for(&offer, &metadata());
        assert_eq!(scopes, offer.provider.unwrap().oauth_scopes);
    }

    #[test]
    fn a_server_outside_the_table_asks_for_the_mail_scopes_its_metadata_lists() {
        let offer = JmapBrowser { session_url: "https://mail.example.org/jmap/session".into(), provider: None };
        assert_eq!(scopes_for(&offer, &metadata()), ["urn:ietf:params:oauth:scope:mail", "offline_access"]);
    }
}
