//! A JMAP account signed in on its server's own page. The server must
//! publish OAuth metadata (RFC 8414) beside its session, with open dynamic
//! registration: a new account registers a public client with the app's
//! loopback redirect and keeps its id. Signing in again uses that client,
//! and registers a new one only when the server no longer knows it.
//! Fastmail and Stalwart both work this way, so no client id is built in.

use std::future::Future;
use std::time::Duration;

use mailrs_discover::JmapProvider;
use mailrs_domain::{AccountId, Provider};
use mailrs_jmap::oauth::{self, Loopback, Metadata, OAuthClient, Pkce, Tokens};
use mailrs_jmap::{Auth, JmapClient, JmapError};
use mailrs_store::accounts;
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

/// The sign-in server's OAuth calls. The live one goes to the server over
/// https only; tests answer from memory, so nothing here loosens that.
pub(crate) trait SignInServer: Send + Sync {
    fn metadata(&self, session_url: &str) -> impl Future<Output = Result<Metadata, JmapError>> + Send;
    fn register(&self, metadata: &Metadata, redirect: &str, scopes: &[String]) -> impl Future<Output = Result<String, JmapError>> + Send;
    fn exchange(&self, client: &OAuthClient, code: &str, redirect: &str, pkce: &Pkce) -> impl Future<Output = Result<Tokens, JmapError>> + Send;
}

struct Live;

impl SignInServer for Live {
    async fn metadata(&self, session_url: &str) -> Result<Metadata, JmapError> {
        oauth::metadata(session_url).await
    }

    async fn register(&self, metadata: &Metadata, redirect: &str, scopes: &[String]) -> Result<String, JmapError> {
        oauth::register(metadata, redirect, scopes).await
    }

    async fn exchange(&self, client: &OAuthClient, code: &str, redirect: &str, pkce: &Pkce) -> Result<Tokens, JmapError> {
        client.exchange(code, redirect, pkce).await
    }
}

/// Whether the browser sign-in can run for `offer`: the server's metadata
/// must read, pass its checks and name a registration endpoint.
pub async fn browser_ready(offer: &JmapBrowser) -> BrowserReady {
    ready_over(&Live, offer).await
}

async fn ready_over<S: SignInServer>(server: &S, offer: &JmapBrowser) -> BrowserReady {
    match server.metadata(&offer.session_url).await {
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

/// The account at `address` and the client id it signed in with before,
/// when it signed in through the browser to the same session and the
/// server still names the same token endpoint.
async fn kept_client(
    connector: &Connector,
    address: &str,
    session_url: &str,
    metadata: &Metadata,
) -> Result<Option<(AccountId, String)>, BrowserSignInError> {
    let address = address.to_string();
    let saved = connector
        .db
        .read(move |c| match accounts::account_by_email(c, &address)? {
            Some(held) if held.provider == Provider::Jmap => Ok(mailrs_store::jmap::load(c, held.id)?.map(|saved| (held.id, saved))),
            _ => Ok(None),
        })
        .await
        .map_err(|err| BrowserSignInError::KeptJmap(err.into()))?;
    Ok(saved
        .filter(|(_, saved)| saved.auth == JmapAuth::OAuth && saved.session_url == session_url)
        .and_then(|(id, saved)| saved.oauth.map(|client| (id, client)))
        .filter(|(_, client)| client.token_endpoint == metadata.token_endpoint)
        .map(|(id, client)| (id, client.client_id)))
}

/// What one trip to the consent page needs besides the client.
struct Trip<'a, S, O> {
    server: &'a S,
    metadata: &'a Metadata,
    scopes: &'a [String],
    session_url: &'a str,
    address: &'a str,
    open: &'a O,
    wait: Duration,
}

impl<S: SignInServer, O: Fn(&str)> Trip<'_, S, O> {
    /// One consent with `client_id`: the loopback, the page through
    /// `open`, the redirect and the code exchange straight after it.
    async fn consent(&self, client_id: &str, canceled: impl Future<Output = ()>) -> Result<Tokens, BrowserSignInError> {
        let loopback = Loopback::bind().await.map_err(BrowserSignInError::Jmap)?;
        let redirect = loopback.redirect_uri.clone();
        let (pkce, state) = (Pkce::generate(), oauth::random_token(32));
        let url = oauth::authorize_url(self.metadata, client_id, &redirect, self.scopes, &pkce, &state, self.session_url, Some(self.address))
            .map_err(BrowserSignInError::Jmap)?;
        (self.open)(&url);
        let code = redirected(loopback, &state, self.metadata, canceled, self.wait).await?;
        let client = OAuthClient {
            client_id: client_id.to_string(),
            token_endpoint: self.metadata.token_endpoint.clone(),
            resource: self.session_url.to_string(),
        };
        self.server.exchange(&client, &code, &redirect, &pkce).await.map_err(BrowserSignInError::Jmap)
    }

    async fn register(&self) -> Result<String, BrowserSignInError> {
        self.server.register(self.metadata, REGISTERED_REDIRECT, self.scopes).await.map_err(BrowserSignInError::Jmap)
    }
}

/// The whole sign-in: the client this account signed in with before, or
/// one registered now, the consent page through `open`, the code exchange,
/// the session check, then the account kept with its refresh token and its
/// services built. `address`, as the person typed it, fills the server's
/// address field and names the account. When the server no longer knows
/// a kept client, one new client is registered and the consent page opens
/// once more. Each wait for the browser ends after
/// [`BROWSER_WAIT`], and the whole sign-in when `canceled` resolves.
pub async fn jmap_in_browser(
    connector: &Connector,
    offer: JmapBrowser,
    address: &str,
    open: impl Fn(&str),
    canceled: impl Future<Output = ()>,
) -> Result<SignedIn, BrowserSignInError> {
    signing_in(&Live, connector, offer, address, open, canceled, BROWSER_WAIT).await
}

pub(crate) async fn signing_in<S: SignInServer>(
    server: &S,
    connector: &Connector,
    offer: JmapBrowser,
    address: &str,
    open: impl Fn(&str),
    canceled: impl Future<Output = ()>,
    wait: Duration,
) -> Result<SignedIn, BrowserSignInError> {
    let BrowserReady::Registers(metadata) = ready_over(server, &offer).await else {
        return Err(BrowserSignInError::NotOffered);
    };
    let scopes = scopes_for(&offer, &metadata);
    let trip = Trip { server, metadata: &metadata, scopes: &scopes, session_url: &offer.session_url, address, open: &open, wait };
    let mut canceled = std::pin::pin!(canceled);
    let sessions = &connector.secrets.jmap_sessions;
    // A kept client whose last sign-in never came back is not tried again:
    // the server may have forgotten it and show its own page instead of
    // coming back. The row keeps it until a new sign-in succeeds.
    let kept = kept_client(connector, address, &offer.session_url, &metadata)
        .await?
        .filter(|(id, _)| !sessions.kept_client_failed(*id));
    let (client_id, tokens) = match kept {
        Some((id, kept)) => match trip.consent(&kept, canceled.as_mut()).await {
            Err(BrowserSignInError::Jmap(JmapError::UnknownClient)) => {
                let fresh = trip.register().await?;
                let tokens = trip.consent(&fresh, canceled.as_mut()).await?;
                (fresh, tokens)
            }
            Err(err @ (BrowserSignInError::GaveUp | BrowserSignInError::Canceled)) => {
                sessions.kept_client_unanswered(id);
                return Err(err);
            }
            done => (kept, done?),
        },
        None => {
            let fresh = trip.register().await?;
            let tokens = trip.consent(&fresh, canceled.as_mut()).await?;
            (fresh, tokens)
        }
    };
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
    // The account keeps the address the person typed, as a password
    // sign-in does: an alias, or a token account signing in again, then
    // finds its own account rather than making one under the session's.
    let address = match address.trim() {
        "" => checked.address.clone().unwrap_or_default(),
        typed => typed.to_string(),
    };
    let new = NewJmap {
        address,
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
    // refuses, a new account goes again and the sign-in fails.
    let account = jmap_signed_in(db, secrets, new, now_millis()).await?;
    // The new refresh token, and maybe a new client, need a new session.
    secrets.jmap_sessions.forget(account.id);
    let window_days = config.engine_config().window_days;
    let services = connect_jmap(db, secrets, &account, window_days).await?;
    Ok(SignedIn { account, name: None, services })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use mailrs_jmap::oauth::Tokens;
    use mailrs_store::Db;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::passwords::{PasswordStore, Secrets};

    /// A sign-in server that answers from memory: no request leaves the
    /// test, and the https endpoints in its metadata are never reached.
    struct FakeServer {
        /// What the clients it registers are called: `<names>-1` first.
        names: &'static str,
        registered: AtomicUsize,
        exchanges: Mutex<VecDeque<Result<Tokens, JmapError>>>,
    }

    impl FakeServer {
        fn answering(names: &'static str, exchanges: Vec<Result<Tokens, JmapError>>) -> FakeServer {
            FakeServer { names, registered: AtomicUsize::new(0), exchanges: Mutex::new(exchanges.into()) }
        }

        fn registrations(&self) -> usize {
            self.registered.load(Ordering::SeqCst)
        }
    }

    impl SignInServer for FakeServer {
        async fn metadata(&self, _session_url: &str) -> Result<Metadata, JmapError> {
            Ok(metadata())
        }

        async fn register(&self, _metadata: &Metadata, redirect: &str, _scopes: &[String]) -> Result<String, JmapError> {
            assert_eq!(redirect, "http://127.0.0.1/");
            let n = self.registered.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(format!("{}-{n}", self.names))
        }

        async fn exchange(&self, client: &OAuthClient, code: &str, _redirect: &str, _pkce: &Pkce) -> Result<Tokens, JmapError> {
            assert_eq!((code, client.token_endpoint.as_str()), ("c1", "https://api.fastmail.com/oauth/refresh"));
            self.exchanges.lock().unwrap().pop_front().expect("an exchange the test did not expect")
        }
    }

    fn tokens(refresh: &str) -> Result<Tokens, JmapError> {
        Ok(Tokens { access_token: "a1".into(), expires_in: 3600, refresh_token: Some(refresh.into()) })
    }

    /// A JMAP server whose session answers only the access token `a1`.
    async fn jmap_server() -> MockServer {
        let server = MockServer::start().await;
        let session = serde_json::json!({
            "capabilities": {"urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:mail": {}},
            "accounts": {"A1": {"name": "me@fastmail.com", "isPersonal": true, "isReadOnly": false,
                "accountCapabilities": {"urn:ietf:params:jmap:mail": {}}}},
            "primaryAccounts": {"urn:ietf:params:jmap:mail": "A1"},
            "username": "me@fastmail.com",
            "apiUrl": format!("{}/api/", server.uri()),
            "downloadUrl": format!("{}/download/{{accountId}}/{{blobId}}/{{name}}?accept={{type}}", server.uri()),
            "uploadUrl": format!("{}/upload/{{accountId}}/", server.uri()),
            "eventSourceUrl": format!("{}/events/?types={{types}}&closeafter={{closeafter}}&ping={{ping}}", server.uri()),
            "state": "s0"
        });
        Mock::given(method("GET"))
            .and(path("/jmap/session"))
            .and(header("authorization", "Bearer a1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(session))
            .mount(&server)
            .await;
        server
    }

    fn connector(db: &Db) -> Connector {
        Connector { db: db.clone(), config: crate::config::Config::default(), clients: crate::Clients::default(), secrets: Secrets::memory() }
    }

    /// The value of `name` in the authorize URL's query, with the escapes
    /// a loopback address and a token can hold undone.
    fn query_value(url: &str, name: &str) -> String {
        let start = url.find(&format!("{name}=")).unwrap() + name.len() + 1;
        let value = url[start..].split('&').next().unwrap();
        value.replace("%3A", ":").replace("%2F", "/")
    }

    /// Plays the browser: records each consent page and comes back to the
    /// loopback with the code `c1` and this page's state.
    fn browser(pages: Arc<Mutex<Vec<String>>>) -> impl Fn(&str) {
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

    async fn sign_in(server: &FakeServer, reach: &Connector, session_url: &str, pages: &Arc<Mutex<Vec<String>>>) -> Result<SignedIn, BrowserSignInError> {
        sign_in_as(server, reach, session_url, pages, "me@fastmail.com").await
    }

    /// A sign-in where the person typed `address`. The session names
    /// me@fastmail.com whatever the person typed.
    async fn sign_in_as(server: &FakeServer, reach: &Connector, session_url: &str, pages: &Arc<Mutex<Vec<String>>>, address: &str) -> Result<SignedIn, BrowserSignInError> {
        let offer = JmapBrowser { session_url: session_url.into(), provider: None };
        signing_in(server, reach, offer, address, browser(Arc::clone(pages)), std::future::pending(), Duration::from_secs(5)).await
    }

    #[tokio::test]
    async fn the_account_keeps_the_address_the_person_typed() {
        let jmap = jmap_server().await;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let reach = connector(&db);
        let server = FakeServer::answering("client", vec![tokens("r1")]);
        let pages = Arc::new(Mutex::new(Vec::new()));
        let session_url = format!("{}/jmap/session", jmap.uri());
        let signed = sign_in_as(&server, &reach, &session_url, &pages, "alias@fastmail.com").await.unwrap();
        assert_eq!(signed.account.email, "alias@fastmail.com");
    }

    #[tokio::test]
    async fn a_token_account_signing_in_through_the_browser_stays_one_account() {
        let jmap = jmap_server().await;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let reach = connector(&db);
        let session_url = format!("{}/jmap/session", jmap.uri());
        let token = NewJmap {
            address: "alias@fastmail.com".into(),
            provider_name: "Fastmail".into(),
            saved: JmapSaved {
                labels: true,
                capabilities: Vec::new(),
                session_url: session_url.clone(),
                auth: JmapAuth::Bearer,
                user_name: None,
                oauth: None,
            },
            secret: "fmu1-token".into(),
            dav_password: None,
        };
        let held = jmap_signed_in(&db, &reach.secrets, token, 1).await.unwrap();
        let server = FakeServer::answering("client", vec![tokens("r1")]);
        let pages = Arc::new(Mutex::new(Vec::new()));
        let signed = sign_in_as(&server, &reach, &session_url, &pages, "alias@fastmail.com").await.unwrap();
        assert_eq!(signed.account.id, held.id);
        let accounts = db.read(accounts::list_accounts).await.unwrap();
        assert_eq!(accounts.len(), 1, "{accounts:?}");
        let saved = db.read(move |c| mailrs_store::jmap::load(c, held.id)).await.unwrap().unwrap();
        assert_eq!(saved.auth, JmapAuth::OAuth);
    }

    async fn kept_client_id(db: &Db, id: i64) -> Option<String> {
        db.read(move |c| mailrs_store::jmap::load(c, id)).await.unwrap().and_then(|saved| saved.oauth).map(|o| o.client_id)
    }

    /// The app runs the sign-in on its runtime's threads.
    #[expect(dead_code, reason = "only compiled, to check the future is Send")]
    fn the_sign_in_can_move_between_threads(connector: &Connector, offer: JmapBrowser) {
        fn sends<T: Send>(_: T) {}
        sends(jmap_in_browser(connector, offer, "", |_: &str| {}, std::future::pending::<()>()));
    }

    #[tokio::test]
    async fn a_new_account_registers_a_client_and_signs_in() {
        let jmap = jmap_server().await;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let reach = connector(&db);
        let server = FakeServer::answering("client", vec![tokens("r1")]);
        let pages = Arc::new(Mutex::new(Vec::new()));
        let signed = sign_in(&server, &reach, &format!("{}/jmap/session", jmap.uri()), &pages).await.unwrap();
        assert_eq!(server.registrations(), 1);
        assert!(pages.lock().unwrap()[0].contains("client_id=client-1"));
        assert_eq!(signed.account.email, "me@fastmail.com");
        assert_eq!(kept_client_id(&db, signed.account.id).await.as_deref(), Some("client-1"));
        assert_eq!(reach.secrets.jmap.load(signed.account.id).unwrap().as_deref(), Some("r1"));
        assert!(matches!(signed.services.mail, crate::AnyMail::Jmap(_)));
    }

    #[tokio::test]
    async fn signing_in_again_reuses_the_kept_client() {
        let jmap = jmap_server().await;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let reach = connector(&db);
        let session_url = format!("{}/jmap/session", jmap.uri());
        let server = FakeServer::answering("client", vec![tokens("r1"), tokens("r2")]);
        let pages = Arc::new(Mutex::new(Vec::new()));
        let first = sign_in(&server, &reach, &session_url, &pages).await.unwrap();
        let before = reach.secrets.jmap_sessions.held(first.account.id).unwrap();
        let again = sign_in(&server, &reach, &session_url, &pages).await.unwrap();
        let after = reach.secrets.jmap_sessions.held(again.account.id).unwrap();
        assert!(!Arc::ptr_eq(&before, &after), "the new refresh token gets a new session");
        assert_eq!(again.account.id, first.account.id);
        assert_eq!(server.registrations(), 1, "a sign-in again registers nothing");
        assert!(pages.lock().unwrap()[1].contains("client_id=client-1"));
        assert_eq!(reach.secrets.jmap.load(again.account.id).unwrap().as_deref(), Some("r2"));
    }

    #[tokio::test]
    async fn a_kept_client_the_server_forgot_is_registered_again_once() {
        let jmap = jmap_server().await;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let reach = connector(&db);
        let session_url = format!("{}/jmap/session", jmap.uri());
        let first = FakeServer::answering("old", vec![tokens("r1")]);
        let pages = Arc::new(Mutex::new(Vec::new()));
        let account = sign_in(&first, &reach, &session_url, &pages).await.unwrap().account;
        let forgot = FakeServer::answering("new", vec![Err(JmapError::UnknownClient), tokens("r2")]);
        let pages = Arc::new(Mutex::new(Vec::new()));
        let again = sign_in(&forgot, &reach, &session_url, &pages).await.unwrap();
        assert_eq!(again.account.id, account.id);
        assert_eq!(forgot.registrations(), 1);
        let pages = pages.lock().unwrap().clone();
        assert_eq!(pages.len(), 2, "the person consents once more, for the new client");
        assert!(pages[0].contains("client_id=old-1"), "the kept client goes first");
        assert!(pages[1].contains("client_id=new-1"));
        assert_eq!(kept_client_id(&db, account.id).await.as_deref(), Some("new-1"));
        assert_eq!(reach.secrets.jmap.load(account.id).unwrap().as_deref(), Some("r2"));
    }

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

    #[tokio::test]
    async fn a_kept_client_that_never_came_back_is_replaced_on_the_next_attempt() {
        let jmap = jmap_server().await;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let reach = connector(&db);
        let session_url = format!("{}/jmap/session", jmap.uri());
        let first = FakeServer::answering("old", vec![tokens("r1")]);
        let account = sign_in(&first, &reach, &session_url, &Arc::new(Mutex::new(Vec::new()))).await.unwrap().account;
        // The server shows its own page for the kept client and never
        // comes back; the person cancels.
        let offer = JmapBrowser { session_url: session_url.clone(), provider: None };
        let silent = FakeServer::answering("unused", Vec::new());
        let canceled = signing_in(&silent, &reach, offer, "me@fastmail.com", |_: &str| {}, std::future::ready(()), Duration::from_secs(5)).await;
        assert!(matches!(canceled, Err(BrowserSignInError::Canceled)));
        assert_eq!(silent.registrations(), 0);
        assert_eq!(kept_client_id(&db, account.id).await.as_deref(), Some("old-1"), "a cancel changes nothing kept");
        let still = crate::connect_jmap(&db, &reach.secrets, &account, 30).await;
        assert!(still.is_ok(), "the account keeps working with its old client");
        let next = FakeServer::answering("new", vec![tokens("r2")]);
        let pages = Arc::new(Mutex::new(Vec::new()));
        sign_in(&next, &reach, &session_url, &pages).await.unwrap();
        assert_eq!(next.registrations(), 1);
        assert!(pages.lock().unwrap()[0].contains("client_id=new-1"), "the kept client is not tried again");
        assert_eq!(kept_client_id(&db, account.id).await.as_deref(), Some("new-1"));
        let after = FakeServer::answering("later", vec![tokens("r3")]);
        sign_in(&after, &reach, &session_url, &Arc::new(Mutex::new(Vec::new()))).await.unwrap();
        assert_eq!(after.registrations(), 0, "the new client is kept and trusted again");
    }
}
