//! OAuth for a JMAP server (draft-ietf-mailmaint-oauth-public): the
//! server's metadata beside its session (RFC 8414), PKCE S256 (RFC 7636),
//! a loopback redirect (RFC 8252), the RFC 8707 `resource`, and dynamic
//! registration of a public client (RFC 7591) where the server offers it.
//!
//! Tokens, codes and the PKCE verifier stay out of `Debug` and out of
//! every error's text. Every address the client reaches is https; the one
//! http address is the app's own loopback on 127.0.0.1, which the browser
//! reaches.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::BoxFuture;
use mailrs_domain::sign_in_page::{self, Finished};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::Instant;
use url::{Host, Url};

use crate::JmapError;
use crate::error::decode;
use crate::http::{network, read_capped};

const TIMEOUT: Duration = Duration::from_secs(30);

/// What an answer from the authorization server may take.
const ANSWER_LIMIT: usize = 64 << 10;

/// How long the loopback waits for a connection's request before it
/// drops the connection and listens again, so a connection that sends
/// nothing cannot hold up the browser's redirect.
const REQUEST_WAIT: Duration = Duration::from_secs(10);

/// What the loopback reads of one request: the request line and headers.
const REQUEST_LIMIT: usize = 8192;

/// How many connections the loopback reads at once. A browser opens a
/// few; more come from something else, and those wait in the backlog.
const MOST_CONNECTIONS: usize = 16;

/// How long the sign-in listener waits after a failed accept.
const ACCEPT_PAUSE: Duration = Duration::from_millis(50);

/// Which addresses a call may reach. Production code has only
/// `HttpsOnly`; the unit tests' mock servers speak http, and only the
/// tests can name the other variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    HttpsOnly,
    #[cfg(test)]
    LocalHttp,
}

/// `address` when the transport may reach it. The refusal names no
/// address.
fn secure(address: &str, transport: Transport) -> Result<Url, JmapError> {
    let url = Url::parse(address).map_err(|_| JmapError::NotHttps)?;
    match (url.scheme(), transport) {
        ("https", _) => Ok(url),
        #[cfg(test)]
        ("http", Transport::LocalHttp) => Ok(url),
        _ => Err(JmapError::NotHttps),
    }
}

/// Refuses a redirect other than `http://127.0.0.1[:port]/...`, the
/// address `Loopback` listens on.
fn own_loopback(redirect_uri: &str) -> Result<(), JmapError> {
    let url = Url::parse(redirect_uri).map_err(|_| JmapError::BadRedirect)?;
    let ours = url.scheme() == "http"
        && url.host() == Some(Host::Ipv4(Ipv4Addr::LOCALHOST))
        && url.username().is_empty()
        && url.password().is_none();
    if ours { Ok(()) } else { Err(JmapError::BadRedirect) }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Metadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    /// The server puts RFC 9207's `iss` on every redirect, so a redirect
    /// without one did not come from it.
    #[serde(default)]
    pub authorization_response_iss_parameter_supported: bool,
    /// Where RFC 7009 revokes a token, when the server offers it. Only
    /// [`revoke`] reads it, and it calls it over https alone.
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
}

#[derive(Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

// By hand, so the verifier never reaches a log line.
impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce").field("verifier", &"<hidden>").field("challenge", &self.challenge).finish()
    }
}

impl Pkce {
    pub fn generate() -> Pkce {
        let verifier = random_token(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Pkce { verifier, challenge }
    }
}

/// `bytes` random bytes in URL-safe base64, for a PKCE verifier or a
/// `state`.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(buf)
}

#[derive(Clone, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default = "an_hour")]
    pub expires_in: u64,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

// By hand, so neither token reaches a log line.
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("expires_in", &self.expires_in)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<hidden>"))
            .finish_non_exhaustive()
    }
}

fn an_hour() -> u64 {
    3600
}

/// A public client of one authorization server, for one session.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub client_id: String,
    pub token_endpoint: String,
    /// The session URL, sent as RFC 8707's `resource`.
    pub resource: String,
}

// By hand, so the client id stays out of the log.
impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("token_endpoint", &self.token_endpoint)
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

fn http() -> Result<reqwest::Client, JmapError> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("Penguin Mail/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(network)
}

async fn read(response: reqwest::Response) -> Result<(u16, Value), JmapError> {
    let status = response.status().as_u16();
    let bytes = read_capped(response, ANSWER_LIMIT).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

/// The authorization server's metadata, beside the session URL's host,
/// over https. The server's issuer is the session URL's origin.
/// Metadata naming another issuer, an endpoint off https, or no PKCE S256
/// is refused.
pub async fn metadata(session_url: &str) -> Result<Metadata, JmapError> {
    metadata_over(session_url, Transport::HttpsOnly).await
}

async fn metadata_over(session_url: &str, transport: Transport) -> Result<Metadata, JmapError> {
    // The session URL could hold a password or an address, so the errors
    // name which URL it was and none of it, as `JmapClient::new` does.
    let parsed = Url::parse(session_url).map_err(|_| JmapError::BadUrl("the session URL".into()))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(JmapError::BadUrl("the session URL".into()));
    }
    let session = secure(session_url, transport)?;
    let issuer = session.origin().ascii_serialization();
    let at = session
        .join("/.well-known/oauth-authorization-server")
        .map_err(|_| JmapError::BadUrl("the session URL".into()))?;
    let (status, body) = read(http()?.get(at).send().await.map_err(network)?).await?;
    if !(200..300).contains(&status) {
        return Err(JmapError::NotFound);
    }
    let found: Metadata = serde_json::from_value(body).map_err(|e| decode("the OAuth metadata", &e))?;
    checked(found, &issuer, transport)
}

/// `found` when it names `issuer`, compared as RFC 8414 section 3.3 asks
/// (the identical string), its endpoints are on the transport, and it
/// takes PKCE S256.
fn checked(found: Metadata, issuer: &str, transport: Transport) -> Result<Metadata, JmapError> {
    if found.issuer != issuer {
        return Err(JmapError::IssuerMismatch);
    }
    secure(&found.authorization_endpoint, transport)?;
    secure(&found.token_endpoint, transport)?;
    if let Some(registration) = &found.registration_endpoint {
        secure(registration, transport)?;
    }
    if !found.code_challenge_methods_supported.iter().any(|m| m == "S256") {
        return Err(JmapError::Forbidden("the server does not take PKCE S256".into()));
    }
    Ok(found)
}

/// The scopes for a JMAP account the server lists: the mail-client scopes
/// of draft-ietf-mailmaint-oauth-public and `offline_access`, which brings
/// a refresh token.
pub fn oauth_scopes(metadata: &Metadata) -> Vec<String> {
    metadata
        .scopes_supported
        .iter()
        .filter(|s| s.starts_with("urn:ietf:params:oauth:scope:") || *s == "offline_access")
        .cloned()
        .collect()
}

/// The address the browser opens for consent. The authorization endpoint
/// must be https and the redirect the app's own loopback.
#[allow(clippy::too_many_arguments)]
pub fn authorize_url(
    metadata: &Metadata,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    pkce: &Pkce,
    state: &str,
    resource: &str,
    login_hint: Option<&str>,
) -> Result<String, JmapError> {
    own_loopback(redirect_uri)?;
    let mut url = secure(&metadata.authorization_endpoint, Transport::HttpsOnly)?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &scopes.join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", resource);
        if let Some(hint) = login_hint {
            pairs.append_pair("login_hint", hint);
        }
    }
    Ok(url.into())
}

/// Registers a public client with the loopback redirect and answers its
/// id. A server without a registration endpoint, or with one off https,
/// refuses.
pub async fn register(metadata: &Metadata, redirect_uri: &str, scopes: &[String]) -> Result<String, JmapError> {
    register_over(metadata, redirect_uri, scopes, Transport::HttpsOnly).await
}

async fn register_over(
    metadata: &Metadata,
    redirect_uri: &str,
    scopes: &[String],
    transport: Transport,
) -> Result<String, JmapError> {
    own_loopback(redirect_uri)?;
    let endpoint = metadata
        .registration_endpoint
        .as_ref()
        .ok_or_else(|| JmapError::Forbidden("the server registers no clients".into()))?;
    let endpoint = secure(endpoint, transport)?;
    let body = json!({
        "client_name": "Penguin Mail",
        "client_uri": "https://penguin-mail.com",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": scopes.join(" "),
    });
    let (status, answer) = read(http()?.post(endpoint).json(&body).send().await.map_err(network)?).await?;
    if !(200..300).contains(&status) {
        return Err(JmapError::Forbidden(error_code(answer["error"].as_str())));
    }
    answer["client_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| JmapError::Decode("no client_id came back".into()))
}

/// How long removing an account waits on the server to revoke its grant.
const REVOKE_WAIT: Duration = Duration::from_secs(5);

/// Revokes `refresh_token`, and the grant with it, at the revocation
/// endpoint the server beside `session_url` names (RFC 7009). Answers
/// `false` when it names none. Gives up after five seconds, and an error
/// carries none of the server's words.
pub async fn revoke(session_url: &str, client_id: &str, refresh_token: &str) -> Result<bool, JmapError> {
    revoke_over(session_url, client_id, refresh_token, Transport::HttpsOnly, REVOKE_WAIT).await
}

async fn revoke_over(
    session_url: &str,
    client_id: &str,
    refresh_token: &str,
    transport: Transport,
    wait: Duration,
) -> Result<bool, JmapError> {
    let revoking = async {
        let metadata = metadata_over(session_url, transport).await?;
        let Some(endpoint) = metadata.revocation_endpoint else { return Ok(false) };
        revoke_at(&endpoint, client_id, refresh_token, transport).await.map(|()| true)
    };
    tokio::time::timeout(wait, revoking)
        .await
        .unwrap_or_else(|_| Err(JmapError::Network("the revocation took too long".into())))
}

async fn revoke_at(endpoint: &str, client_id: &str, refresh_token: &str, transport: Transport) -> Result<(), JmapError> {
    let endpoint = secure(endpoint, transport)?;
    let form = [("token", refresh_token), ("token_type_hint", "refresh_token"), ("client_id", client_id)];
    let (status, body) = read(http()?.post(endpoint).form(&form).send().await.map_err(network)?).await?;
    match status {
        200..300 => Ok(()),
        status => Err(JmapError::Problem { status, kind: error_code(body["error"].as_str()), detail: String::new() }),
    }
}

impl OAuthClient {
    /// Trades the code from the redirect for tokens.
    pub async fn exchange(&self, code: &str, redirect_uri: &str, pkce: &Pkce) -> Result<Tokens, JmapError> {
        self.exchange_over(code, redirect_uri, pkce, Transport::HttpsOnly).await
    }

    async fn exchange_over(
        &self,
        code: &str,
        redirect_uri: &str,
        pkce: &Pkce,
        transport: Transport,
    ) -> Result<Tokens, JmapError> {
        own_loopback(redirect_uri)?;
        self.token(transport, &[
            ("grant_type", "authorization_code"),
            ("client_id", &self.client_id),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", &pkce.verifier),
            ("resource", &self.resource),
        ])
        .await
    }

    /// New tokens for a refresh token. The answer may carry a new refresh
    /// token; the caller saves it before it uses the access token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, JmapError> {
        self.refresh_over(refresh_token, Transport::HttpsOnly).await
    }

    async fn refresh_over(&self, refresh_token: &str, transport: Transport) -> Result<Tokens, JmapError> {
        self.refresh_answer(refresh_token, transport).await.map_err(|(err, _)| err)
    }

    /// `refresh_over`, with the refresh token a 2xx answer carried when the
    /// client refuses the rest of it.
    async fn refresh_answer(&self, refresh_token: &str, transport: Transport) -> Result<Tokens, Refused> {
        self.answer(transport, &[
            ("grant_type", "refresh_token"),
            ("client_id", &self.client_id),
            ("refresh_token", refresh_token),
            ("resource", &self.resource),
        ])
        .await
    }

    async fn token(&self, transport: Transport, form: &[(&str, &str)]) -> Result<Tokens, JmapError> {
        self.answer(transport, form).await.map_err(|(err, _)| err)
    }

    /// The token endpoint's answer. When a 2xx answer is refused, for a
    /// token type other than Bearer or a body that does not decode, the
    /// refresh token it carried comes back beside the error: the server has
    /// already moved to it.
    async fn answer(&self, transport: Transport, form: &[(&str, &str)]) -> Result<Tokens, Refused> {
        let endpoint = secure(&self.token_endpoint, transport).map_err(|err| (err, None))?;
        let response = http().map_err(|err| (err, None))?.post(endpoint).form(form).send().await;
        let (status, body) = read(response.map_err(|err| (network(err), None))?).await.map_err(|err| (err, None))?;
        if (200..300).contains(&status) {
            let rotated = body["refresh_token"].as_str().filter(|t| !t.is_empty()).map(str::to_string);
            let bearer = body["token_type"].as_str().is_some_and(|kind| kind.eq_ignore_ascii_case("Bearer"));
            if !bearer {
                return Err((JmapError::NotBearer, rotated));
            }
            return serde_json::from_value(body).map_err(|e| (decode("the token answer", &e), rotated));
        }
        Err((match (status, error_code(body["error"].as_str()).as_str()) {
            (_, "invalid_grant") if body["temporary"].as_bool() == Some(true) => JmapError::SignInLocked,
            (_, "invalid_client" | "unauthorized_client") => JmapError::UnknownClient,
            (_, "invalid_grant") => JmapError::Unauthorized,
            (429 | 503, _) => JmapError::RateLimited { retry_after: None },
            (status, error) => JmapError::Problem { status, kind: error.to_string(), detail: String::new() },
        }, None))
    }
}

/// A refused token answer, with the refresh token it carried, if any.
type Refused = (JmapError, Option<String>);

/// What `OAuthSession::on_rotated` runs with each new refresh token.
type Keep = Box<dyn Fn(String) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// How long before its expiry an access token stops going out, so it
/// cannot expire on its way to the server. A token that lives less than
/// twice this keeps half its life instead.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// The longest lifetime an access token is taken at. A server could name
/// one past any clock.
const LONGEST_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// How long saving a rotated refresh token may take before it counts as
/// failed. A keyring can wait on an unlock prompt nobody answers.
const KEEP_WAIT: Duration = Duration::from_secs(30);

struct Held {
    refresh_token: String,
    access: Option<Access>,
}

struct Access {
    token: String,
    until: Instant,
    margin: Duration,
}

impl Access {
    fn new(token: String, expires_in: u64) -> Access {
        let life = Duration::from_secs(expires_in).min(LONGEST_LIFETIME);
        Access { token, until: Instant::now() + life, margin: EXPIRY_MARGIN.min(life / 2) }
    }
}

impl Held {
    /// The access token while it has more than its margin to live.
    fn usable(&self) -> Option<&str> {
        let access = self.access.as_ref()?;
        (access.until > Instant::now() + access.margin).then_some(access.token.as_str())
    }
}

/// Saves rotated refresh tokens through the caller's callback, one save
/// at a time, and holds the newest one until a save of it succeeds.
struct Keeper {
    keep: Keep,
    /// The session's retired mark: once set, nothing is saved.
    retired: Arc<AtomicBool>,
    /// The newest refresh token no save has kept yet. Its lock orders the
    /// saves, so an older token never lands after a newer one. It is taken
    /// with `held` locked or alone, never the other way round.
    unsaved: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl Keeper {
    /// Saves `rotated`, unless the session is retired: its token would
    /// land over the new grant's, or under an account that is gone. The
    /// mark is read under the `unsaved` lock, which `retire` takes after
    /// setting it, so no save starts once `retire` has returned.
    async fn save(&self, rotated: String, wait: Duration) {
        let mut unsaved = self.unsaved.lock().await;
        if self.retired.load(Ordering::SeqCst) {
            return;
        }
        *unsaved = Some(rotated);
        self.flush(&mut unsaved, wait).await;
    }

    /// Saves the token in `unsaved`, if any, and clears it once saved.
    async fn flush(&self, unsaved: &mut Option<String>, wait: Duration) {
        let Some(token) = unsaved.clone() else { return };
        // The callback's words stay out of the log: they are the caller's.
        match tokio::time::timeout(wait, (self.keep)(token)).await {
            Ok(Ok(())) => *unsaved = None,
            Ok(Err(_)) => tracing::warn!("could not keep the server's new refresh token; the next call tries again"),
            Err(_) => tracing::warn!("keeping the server's new refresh token took too long; the next call tries again"),
        }
    }
}

/// One account's OAuth tokens. A refresh runs under the lock, so calls
/// that find the token expired, or that the server refused together,
/// share one refresh. A rotated refresh token replaces the old one before
/// the access token that came with it goes out, since Fastmail revokes a
/// grant whose old refresh token comes back.
pub struct OAuthSession {
    client: OAuthClient,
    held: Arc<tokio::sync::Mutex<Held>>,
    keeper: Option<Arc<Keeper>>,
    keep_wait: Duration,
    transport: Transport,
    /// Set once the session is retired: a new sign-in replaced its grant,
    /// or its account went.
    retired: Arc<AtomicBool>,
}

// By hand, so neither token nor the client id reaches a log line.
impl std::fmt::Debug for OAuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSession").field("token_endpoint", &self.client.token_endpoint).finish_non_exhaustive()
    }
}

impl OAuthSession {
    /// A session that starts from a kept refresh token.
    pub fn new(client: OAuthClient, refresh_token: String) -> OAuthSession {
        OAuthSession::holding(client, Held { refresh_token, access: None })
    }

    /// A session that starts from the code exchange's tokens.
    pub fn signed_in(client: OAuthClient, tokens: Tokens) -> OAuthSession {
        let access = Some(Access::new(tokens.access_token, tokens.expires_in));
        OAuthSession::holding(client, Held { refresh_token: tokens.refresh_token.unwrap_or_default(), access })
    }

    fn holding(client: OAuthClient, held: Held) -> OAuthSession {
        OAuthSession {
            client,
            held: Arc::new(tokio::sync::Mutex::new(held)),
            keeper: None,
            keep_wait: KEEP_WAIT,
            transport: Transport::HttpsOnly,
            retired: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Runs `keep` with each new refresh token, and waits for it, 30
    /// seconds at most, before the new access token is used. When `keep`
    /// fails or takes longer, the new token stays in memory, the old one
    /// is never sent again, and the next call runs `keep` again, in the
    /// background, until it succeeds.
    pub fn on_rotated(
        mut self,
        keep: impl Fn(String) -> BoxFuture<'static, Result<(), String>> + Send + Sync + 'static,
    ) -> OAuthSession {
        let keeper = Keeper { keep: Box::new(keep), retired: Arc::clone(&self.retired), unsaved: Arc::new(tokio::sync::Mutex::new(None)) };
        self.keeper = Some(Arc::new(keeper));
        self
    }

    #[cfg(test)]
    fn waiting_for_keep(mut self, wait: Duration) -> OAuthSession {
        self.keep_wait = wait;
        self
    }

    #[cfg(test)]
    fn over(mut self, transport: Transport) -> OAuthSession {
        self.transport = transport;
        self
    }

    /// An access token with a minute or more to live (half its life for a
    /// short-lived one), refreshed first when `force` or when the held one
    /// is about to expire. A refused refresh is `Unauthorized`, which asks
    /// the person to sign in again.
    pub async fn bearer(&self, force: bool) -> Result<String, JmapError> {
        self.save_again();
        let held = Arc::clone(&self.held).lock_owned().await;
        if !force && let Some(token) = held.usable() {
            return Ok(token.to_string());
        }
        self.renewed(held).await
    }

    /// An access token other than `refused`, which the server just turned
    /// away. When another call refreshed while this one waited for the
    /// lock, its token comes back and no second refresh goes out.
    pub async fn bearer_instead_of(&self, refused: &str) -> Result<String, JmapError> {
        self.save_again();
        let held = Arc::clone(&self.held).lock_owned().await;
        if let Some(token) = held.usable()
            && token != refused
        {
            return Ok(token.to_string());
        }
        self.renewed(held).await
    }

    /// Stops the session from saving any refresh token from now on, and
    /// answers the newest one it holds, once a refresh or a save already
    /// running has ended. A new sign-in retires the session it replaces
    /// before it keeps its own token, and removing the account retires it
    /// before the token is revoked and deleted.
    pub async fn retire(&self) -> String {
        self.retired.store(true, Ordering::SeqCst);
        // A refresh holds `held` until its save ends, and a save run again
        // holds `unsaved`: both are over once these locks are taken.
        let held = self.held.lock().await;
        if let Some(keeper) = &self.keeper {
            drop(keeper.unsaved.lock().await);
        }
        held.refresh_token.clone()
    }

    /// Whether [`OAuthSession::retire`] ran.
    pub fn retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    /// Runs `keep` again in the background for a rotated refresh token an
    /// earlier save failed to keep, unless a save is running already or the
    /// session is retired.
    fn save_again(&self) {
        let Some(keeper) = &self.keeper else { return };
        let Ok(mut unsaved) = Arc::clone(&keeper.unsaved).try_lock_owned() else { return };
        // Read under the lock, as `Keeper::save` reads it.
        if unsaved.is_none() || self.retired() {
            return;
        }
        let (keeper, wait) = (Arc::clone(keeper), self.keep_wait);
        tokio::spawn(async move { keeper.flush(&mut unsaved, wait).await });
    }

    /// Refreshes in a task of its own that holds the lock, so a caller
    /// that gives up halfway cannot drop a rotated refresh token the
    /// server has already moved to.
    async fn renewed(&self, held: tokio::sync::OwnedMutexGuard<Held>) -> Result<String, JmapError> {
        let refreshing = renew(self.client.clone(), held, self.keeper.clone(), self.keep_wait, self.transport);
        tokio::spawn(refreshing).await.map_err(|_| JmapError::Network("the token refresh stopped".into()))?
    }
}

async fn renew(
    client: OAuthClient,
    mut held: tokio::sync::OwnedMutexGuard<Held>,
    keeper: Option<Arc<Keeper>>,
    wait: Duration,
    transport: Transport,
) -> Result<String, JmapError> {
    if held.refresh_token.is_empty() {
        return Err(JmapError::Unauthorized);
    }
    let (tokens, rotated) = match client.refresh_answer(&held.refresh_token, transport).await {
        Ok(mut tokens) => {
            let rotated = tokens.refresh_token.take();
            (Ok(tokens), rotated)
        }
        Err((err, rotated)) => (Err(err), rotated),
    };
    if let Some(rotated) = rotated.filter(|t| *t != held.refresh_token) {
        held.refresh_token = rotated.clone();
        if let Some(keeper) = &keeper {
            keeper.save(rotated, wait).await;
        }
    }
    let tokens = tokens?;
    held.access = Some(Access::new(tokens.access_token.clone(), tokens.expires_in));
    Ok(tokens.access_token)
}

/// Receives the redirect on 127.0.0.1, at a port the system picks, during
/// consent. RFC 8252 section 7.3 has the server take any port on the
/// loopback for a client registered with it.
pub struct Loopback {
    listener: TcpListener,
    pub redirect_uri: String,
}

impl Loopback {
    pub async fn bind() -> Result<Loopback, JmapError> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(io)?;
        let port = listener.local_addr().map_err(io)?.port();
        Ok(Loopback { listener, redirect_uri: format!("http://127.0.0.1:{port}/") })
    }

    /// Answers requests until one carries the code or an error for this
    /// sign-in, then closes the listener. A request that is not that
    /// redirect, such as a favicon or one with another `state`, gets a 404
    /// and the listener waits on, so a local process or a web page that
    /// finds the port cannot end the sign-in. Connections are read side by
    /// side, so an idle one cannot hold up the browser's.
    ///
    /// This waits for as long as the browser takes: the caller bounds the
    /// whole sign-in with its own timeout and drops this future.
    pub async fn wait_for_code(self, state: &str, metadata: &Metadata) -> Result<String, JmapError> {
        let mut reading = JoinSet::new();
        loop {
            tokio::select! {
                // At the cap the loop stops accepting, so a further
                // connection waits in the backlog until a read ends (at most
                // `REQUEST_WAIT`) rather than being dropped. Each read holds
                // at most `REQUEST_LIMIT` bytes.
                stream = accept_one(|| self.listener.accept()), if reading.len() < MOST_CONNECTIONS => {
                    reading.spawn(read_request(stream));
                }
                Some(read) = reading.join_next() => {
                    let Ok(Some((mut stream, request))) = read else { continue };
                    let outcome = parse_redirect(&String::from_utf8_lossy(&request), state, metadata);
                    let finished = match &outcome {
                        Some(Ok(_)) => Finished::Signed,
                        Some(Err(JmapError::Declined)) => Finished::Denied,
                        Some(Err(_)) => Finished::Failed,
                        None => Finished::Nothing,
                    };
                    let (status, body) = sign_in_page::page(finished);
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    if let Some(result) = outcome {
                        return result;
                    }
                }
            }
        }
    }
}

/// The next connection `accept` hands over. A failed accept, such as a
/// connection aborted before it was taken or a moment out of file
/// handles, is no reason to end a sign-in the browser has yet to finish:
/// it is logged, and the next try waits `ACCEPT_PAUSE` so a lasting
/// failure does not spin.
async fn accept_one<F, Fut>(mut accept: F) -> TcpStream
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<(TcpStream, std::net::SocketAddr)>>,
{
    loop {
        match accept().await {
            Ok((stream, _)) => return stream,
            Err(err) => {
                tracing::warn!(kind = %err.kind(), "the sign-in listener could not take a connection");
                tokio::time::sleep(ACCEPT_PAUSE).await;
            }
        }
    }
}

/// The request head from one connection: bytes up to the blank line that
/// ends the headers, or `REQUEST_LIMIT` of them. `None` when the
/// connection sends nothing within `REQUEST_WAIT` or fails.
async fn read_request(mut stream: TcpStream) -> Option<(TcpStream, Vec<u8>)> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let reading = async {
        while request.len() < REQUEST_LIMIT && !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let room = (REQUEST_LIMIT - request.len()).min(chunk.len());
            match stream.read(&mut chunk[..room]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&chunk[..n]),
            }
        }
    };
    tokio::time::timeout(REQUEST_WAIT, reading).await.ok()?;
    (!request.is_empty()).then_some((stream, request))
}

fn io(err: std::io::Error) -> JmapError {
    JmapError::Network(format!("loopback listener: {}", err.kind()))
}

/// The server's `error` code when it keeps to RFC 6749's error charset
/// (%x20-21 / %x23-5B / %x5D-7E) and to 64 bytes, and "unknown" otherwise.
/// The `error_description` is never read: a server can write anything
/// there, a token among it.
fn error_code(error: Option<&str>) -> String {
    match error {
        Some(code)
            if !code.is_empty()
                && code.len() <= 64
                && code.bytes().all(|b| matches!(b, 0x20..=0x21 | 0x23..=0x5B | 0x5D..=0x7E)) =>
        {
            code.to_string()
        }
        _ => "unknown".into(),
    }
}

/// The code out of a raw HTTP request. `None` for a request that is not
/// the redirect this sign-in waits for: a favicon, or a code or an error
/// under another `state`. A redirect whose RFC 9207 `iss` names another
/// issuer than `metadata`'s, or that lacks the `iss` the metadata
/// promised, is refused: the code came from a server other than the one
/// that will be asked to redeem it.
pub fn parse_redirect(request: &str, state: &str, metadata: &Metadata) -> Option<Result<String, JmapError>> {
    let target = request.lines().next()?.split_whitespace().nth(1)?;
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let error = params.get("error");
    let code = params.get("code");
    if error.is_none() && code.is_none() {
        return None;
    }
    if params.get("state").map(String::as_str) != Some(state) {
        return None;
    }
    match params.get("iss") {
        Some(iss) if *iss != metadata.issuer => return Some(Err(JmapError::IssuerMismatch)),
        None if metadata.authorization_response_iss_parameter_supported => {
            return Some(Err(JmapError::IssuerMismatch));
        }
        _ => {}
    }
    if let Some(error) = error {
        return Some(Err(match error.as_str() {
            "access_denied" => JmapError::Declined,
            "invalid_client" | "unauthorized_client" => JmapError::UnknownClient,
            other => JmapError::Forbidden(error_code(Some(other))),
        }));
    }
    code.map(|code| Ok(code.clone()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{any, body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    // The mock servers speak http, which only this module's transport
    // takes.
    const LOCAL: Transport = Transport::LocalHttp;

    /// A connection the kernel aborted before the accept, or a moment out
    /// of file handles, leaves the sign-in waiting for the browser.
    #[tokio::test]
    async fn a_failed_accept_waits_on_for_the_next_connection() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut failures = 2;
        let accepted = accept_one(|| {
            let fail = failures > 0;
            failures -= 1;
            let listener = &listener;
            async move {
                match fail {
                    true => Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted)),
                    false => listener.accept().await,
                }
            }
        });
        let connect = TcpStream::connect(address);
        let (accepted, _) = tokio::join!(accepted, connect);
        assert_eq!(accepted.peer_addr().unwrap().ip(), address.ip());
    }

    async fn serving(server: &MockServer, metadata: Value) {
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
            .mount(server)
            .await;
    }

    fn metadata_for(server: &MockServer, registration: bool) -> Value {
        let mut metadata = json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["urn:ietf:params:oauth:scope:mail", "urn:ietf:params:oauth:scope:contacts", "openid", "offline_access"],
            "authorization_response_iss_parameter_supported": true
        });
        if registration {
            metadata["registration_endpoint"] = json!(format!("{}/register", server.uri()));
        }
        metadata
    }

    async fn serving_metadata(server: &MockServer, registration: bool) {
        serving(server, metadata_for(server, registration)).await;
    }

    fn session_of(server: &MockServer) -> String {
        format!("{}/jmap/session", server.uri())
    }

    fn client_of(server: &MockServer) -> OAuthClient {
        OAuthClient {
            client_id: "c1".into(),
            token_endpoint: format!("{}/token", server.uri()),
            resource: "https://api.example.com/jmap/session".into(),
        }
    }

    #[tokio::test]
    async fn metadata_comes_from_beside_the_session() {
        let server = MockServer::start().await;
        serving_metadata(&server, true).await;
        let found = metadata_over(&session_of(&server), LOCAL).await.unwrap();
        assert_eq!(found.token_endpoint, format!("{}/token", server.uri()));
        assert!(found.authorization_response_iss_parameter_supported);
        assert_eq!(
            oauth_scopes(&found),
            ["urn:ietf:params:oauth:scope:mail", "urn:ietf:params:oauth:scope:contacts", "offline_access"]
        );
    }

    #[tokio::test]
    async fn metadata_without_s256_is_refused() {
        let server = MockServer::start().await;
        let mut metadata = metadata_for(&server, true);
        metadata["code_challenge_methods_supported"] = json!(["plain"]);
        serving(&server, metadata).await;
        assert!(metadata_over(&session_of(&server), LOCAL).await.is_err());
    }

    #[tokio::test]
    async fn a_mismatched_issuer_is_refused() {
        let server = MockServer::start().await;
        let mut metadata = metadata_for(&server, true);
        // RFC 8414 section 3.3 asks for the identical string, so a
        // trailing slash is already another issuer.
        metadata["issuer"] = json!(format!("{}/", server.uri()));
        serving(&server, metadata).await;
        let err = metadata_over(&session_of(&server), LOCAL).await.unwrap_err();
        assert_eq!(err, JmapError::IssuerMismatch);
        assert!(!err.to_string().contains("127.0.0.1"), "{err}");
    }

    #[tokio::test]
    async fn an_issuer_on_another_host_is_refused() {
        let server = MockServer::start().await;
        let mut metadata = metadata_for(&server, true);
        metadata["issuer"] = json!("https://evil.example");
        serving(&server, metadata).await;
        assert_eq!(metadata_over(&session_of(&server), LOCAL).await.unwrap_err(), JmapError::IssuerMismatch);
    }

    #[test]
    fn metadata_naming_an_http_endpoint_is_refused() {
        let https = Metadata {
            issuer: "https://auth.example.com".into(),
            authorization_endpoint: "https://auth.example.com/authorize".into(),
            token_endpoint: "https://auth.example.com/token".into(),
            registration_endpoint: Some("https://auth.example.com/register".into()),
            scopes_supported: Vec::new(),
            code_challenge_methods_supported: vec!["S256".into()],
            authorization_response_iss_parameter_supported: false,
            revocation_endpoint: None,
        };
        assert_eq!(checked(https.clone(), "https://auth.example.com", Transport::HttpsOnly), Ok(https.clone()));
        let http = "http://auth.example.com/x".to_string();
        for bad in [
            Metadata { authorization_endpoint: http.clone(), ..https.clone() },
            Metadata { token_endpoint: http.clone(), ..https.clone() },
            Metadata { registration_endpoint: Some(http.clone()), ..https.clone() },
            Metadata { token_endpoint: "not a url".into(), ..https.clone() },
        ] {
            let err = checked(bad, "https://auth.example.com", Transport::HttpsOnly).unwrap_err();
            assert_eq!(err, JmapError::NotHttps);
            assert!(!err.to_string().contains("auth.example.com"), "{err}");
        }
    }

    #[tokio::test]
    async fn a_server_without_registration_offers_no_browser_sign_in() {
        let server = MockServer::start().await;
        serving_metadata(&server, false).await;
        let found = metadata_over(&session_of(&server), LOCAL).await.unwrap();
        assert!(found.registration_endpoint.is_none());
        assert!(register_over(&found, "http://127.0.0.1/", &[], LOCAL).await.is_err());
    }

    #[tokio::test]
    async fn registration_asks_for_a_public_client_on_the_loopback() {
        let server = MockServer::start().await;
        serving_metadata(&server, true).await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .and(body_string_contains("\"token_endpoint_auth_method\":\"none\""))
            .and(body_string_contains("http://127.0.0.1/"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"client_id": "stateless-123"})))
            .mount(&server)
            .await;
        let found = metadata_over(&session_of(&server), LOCAL).await.unwrap();
        assert_eq!(
            register_over(&found, "http://127.0.0.1/", &oauth_scopes(&found), LOCAL).await.unwrap(),
            "stateless-123"
        );
    }

    #[tokio::test]
    async fn the_code_and_the_refresh_carry_pkce_and_the_resource() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code_verifier="))
            .and(body_string_contains("resource=https%3A%2F%2Fapi.example.com%2Fjmap%2Fsession"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "a1", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "r1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant", "temporary": true})))
            .mount(&server)
            .await;
        let client = client_of(&server);
        let tokens = client.exchange_over("code", "http://127.0.0.1:5555/", &Pkce::generate(), LOCAL).await.unwrap();
        assert_eq!((tokens.access_token.as_str(), tokens.refresh_token.as_deref()), ("a1", Some("r1")));
        assert_eq!(client.refresh_over("r1", LOCAL).await.unwrap_err(), JmapError::SignInLocked);
        assert!(JmapError::SignInLocked.is_transient());
    }

    #[tokio::test]
    async fn a_revoked_grant_asks_to_sign_in_again() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
            .mount(&server)
            .await;
        assert_eq!(client_of(&server).refresh_over("r1", LOCAL).await.unwrap_err(), JmapError::Unauthorized);
    }

    #[tokio::test]
    async fn a_client_the_server_forgot_is_told_apart_from_a_revoked_grant() {
        for error in ["invalid_client", "unauthorized_client"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": error})))
                .mount(&server)
                .await;
            assert_eq!(client_of(&server).refresh_over("r1", LOCAL).await.unwrap_err(), JmapError::UnknownClient, "{error}");
        }
    }

    async fn answering_tokens(server: &MockServer, answer: ResponseTemplate) {
        Mock::given(method("POST")).and(path("/token")).respond_with(answer).mount(server).await;
    }

    #[tokio::test]
    async fn a_token_of_another_type_is_refused() {
        for token_type in [json!("mac"), json!("DPoP"), Value::Null] {
            let server = MockServer::start().await;
            let mut answer = json!({"access_token": "a1", "expires_in": 3600});
            if !token_type.is_null() {
                answer["token_type"] = token_type.clone();
            }
            answering_tokens(&server, ResponseTemplate::new(200).set_body_json(answer)).await;
            let err = client_of(&server).refresh_over("r1", LOCAL).await.unwrap_err();
            assert_eq!(err, JmapError::NotBearer, "{token_type}");
        }
    }

    #[tokio::test]
    async fn a_bearer_token_is_taken_in_any_case() {
        let server = MockServer::start().await;
        let answer = json!({"access_token": "a1", "token_type": "bEaReR", "expires_in": 3600});
        answering_tokens(&server, ResponseTemplate::new(200).set_body_json(answer)).await;
        assert_eq!(client_of(&server).refresh_over("r1", LOCAL).await.unwrap().access_token, "a1");
    }

    #[tokio::test]
    async fn a_token_error_keeps_no_server_text() {
        let server = MockServer::start().await;
        let answer = json!({"error": "invalid_request", "error_description": "refresh token tok-SECRET-123 is malformed"});
        answering_tokens(&server, ResponseTemplate::new(400).set_body_json(answer)).await;
        let err = client_of(&server).refresh_over("r1", LOCAL).await.unwrap_err();
        assert_eq!(err, JmapError::Problem { status: 400, kind: "invalid_request".into(), detail: String::new() });
        let said = format!("{err} {err:?}");
        assert!(!said.contains("SECRET"), "{said}");
    }

    #[tokio::test]
    async fn a_token_error_code_outside_the_rfc_charset_becomes_a_fixed_word() {
        for code in [json!("tok-SECRET-\\x"), json!("quote\"SECRET"), json!("SECRET".repeat(11)), json!(42)] {
            let server = MockServer::start().await;
            answering_tokens(&server, ResponseTemplate::new(400).set_body_json(json!({"error": code}))).await;
            let err = client_of(&server).refresh_over("r1", LOCAL).await.unwrap_err();
            assert_eq!(err, JmapError::Problem { status: 400, kind: "unknown".into(), detail: String::new() }, "{code}");
            assert!(!format!("{err} {err:?}").contains("SECRET"), "{err:?}");
        }
    }

    #[tokio::test]
    async fn a_registration_error_keeps_no_server_text() {
        let server = MockServer::start().await;
        serving_metadata(&server, true).await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                json!({"error": "invalid_redirect_uri", "error_description": "tok-SECRET-123 is not allowed"}),
            ))
            .mount(&server)
            .await;
        let found = metadata_over(&session_of(&server), LOCAL).await.unwrap();
        let err = register_over(&found, "http://127.0.0.1/", &[], LOCAL).await.unwrap_err();
        assert_eq!(err, JmapError::Forbidden("invalid_redirect_uri".into()));
        assert!(!format!("{err} {err:?}").contains("SECRET"), "{err:?}");
    }

    // The OAuth session: refreshes, rotation, and the 401 retry.

    use std::sync::Mutex;

    use crate::{Auth, JmapClient, Request};

    type Saved = Arc<Mutex<Vec<String>>>;

    fn kept() -> (Saved, impl Fn(String) -> BoxFuture<'static, Result<(), String>> + Send + Sync + 'static) {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&kept);
        (kept, move |token: String| {
            let into = Arc::clone(&into);
            Box::pin(async move {
                into.lock().unwrap().push(token);
                Ok(())
            }) as BoxFuture<'static, Result<(), String>>
        })
    }

    fn bearer_answer(answer: Value) -> ResponseTemplate {
        let mut answer = answer;
        answer["token_type"] = json!("Bearer");
        ResponseTemplate::new(200).set_body_json(answer)
    }

    #[tokio::test]
    async fn two_expired_calls_share_one_refresh_saved_first() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                bearer_answer(json!({"access_token": "a2", "expires_in": 3600, "refresh_token": "r2"}))
                    .set_delay(Duration::from_millis(50)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (saved, keep) = kept();
        let session = Arc::new(OAuthSession::new(client_of(&server), "r1".into()).on_rotated(keep).over(LOCAL));
        let (one, two) = tokio::join!(session.bearer(false), session.bearer(false));
        assert_eq!((one.unwrap().as_str(), two.unwrap().as_str()), ("a2", "a2"));
        assert_eq!(*saved.lock().unwrap(), ["r2"]);
    }

    #[tokio::test]
    async fn a_rotation_the_keyring_refuses_keeps_the_new_token_in_memory() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r1"))
            .respond_with(bearer_answer(json!({"access_token": "a2", "expires_in": 0, "refresh_token": "r2"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r2"))
            .respond_with(bearer_answer(json!({"access_token": "a3", "expires_in": 3600})))
            .mount(&server)
            .await;
        let session = OAuthSession::new(client_of(&server), "r1".into())
            .on_rotated(|_| Box::pin(async { Err("the keyring is locked".to_string()) }) as BoxFuture<'static, Result<(), String>>)
            .over(LOCAL);
        assert_eq!(session.bearer(false).await.unwrap(), "a2");
        // a2 expired at once; the next refresh goes with r2, never r1 again.
        assert_eq!(session.bearer(false).await.unwrap(), "a3");
    }

    #[tokio::test]
    async fn a_refresh_whose_caller_gave_up_still_keeps_the_rotation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r1"))
            .respond_with(
                bearer_answer(json!({"access_token": "a2", "expires_in": 3600, "refresh_token": "r2"}))
                    .set_delay(Duration::from_millis(200)),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r2"))
            .respond_with(bearer_answer(json!({"access_token": "a3", "expires_in": 3600})))
            .mount(&server)
            .await;
        let (saved, keep) = kept();
        let session = OAuthSession::new(client_of(&server), "r1".into()).on_rotated(keep).over(LOCAL);
        let gave_up = tokio::time::timeout(Duration::from_millis(20), session.bearer(false)).await;
        assert!(gave_up.is_err(), "the refresh finished before the caller gave up");
        assert_eq!(session.bearer(true).await.unwrap(), "a3");
        assert_eq!(*saved.lock().unwrap(), ["r2"]);
    }

    #[tokio::test]
    async fn a_session_without_a_refresh_token_asks_to_sign_in_again() {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(ResponseTemplate::new(500)).expect(0).mount(&server).await;
        let tokens = Tokens { access_token: "a1".into(), expires_in: 0, refresh_token: None };
        let session = OAuthSession::signed_in(client_of(&server), tokens).over(LOCAL);
        assert_eq!(session.bearer(false).await.unwrap_err(), JmapError::Unauthorized);
    }

    /// A keep that refuses its first `refusals` tokens and keeps the rest.
    fn refusing_first(refusals: usize) -> (Saved, impl Fn(String) -> BoxFuture<'static, Result<(), String>> + Send + Sync + 'static) {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&kept);
        let tries = Arc::new(Mutex::new(0usize));
        (kept, move |token: String| {
            let (into, tries) = (Arc::clone(&into), Arc::clone(&tries));
            Box::pin(async move {
                let mut tries = tries.lock().unwrap();
                *tries += 1;
                if *tries <= refusals {
                    return Err("the keyring is locked".to_string());
                }
                into.lock().unwrap().push(token);
                Ok(())
            }) as BoxFuture<'static, Result<(), String>>
        })
    }

    /// Waits until `saved` holds `want`, for a second at most.
    async fn saved_soon(saved: &Saved, want: &[&str]) {
        for _ in 0..100 {
            if *saved.lock().unwrap() == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(*saved.lock().unwrap(), want);
    }

    async fn rotating_once(server: &MockServer, expires_in: u64) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r1"))
            .respond_with(bearer_answer(json!({"access_token": "a2", "expires_in": expires_in, "refresh_token": "r2"})))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r2"))
            .respond_with(bearer_answer(json!({"access_token": "a3", "expires_in": 3600})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn a_rotation_the_keyring_refused_is_saved_again_on_a_later_call() {
        let server = MockServer::start().await;
        rotating_once(&server, 3600).await;
        let (saved, keep) = refusing_first(1);
        let session = OAuthSession::new(client_of(&server), "r1".into()).on_rotated(keep).over(LOCAL);
        assert_eq!(session.bearer(false).await.unwrap(), "a2");
        assert!(saved.lock().unwrap().is_empty());
        assert_eq!(session.bearer(false).await.unwrap(), "a2");
        saved_soon(&saved, &["r2"]).await;
        // Saved now: a further call has nothing to save again.
        session.bearer(false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*saved.lock().unwrap(), ["r2"]);
    }

    #[tokio::test]
    async fn a_keyring_that_never_answers_does_not_hold_up_the_account() {
        let server = MockServer::start().await;
        rotating_once(&server, 0).await;
        let session = OAuthSession::new(client_of(&server), "r1".into())
            .on_rotated(|_| Box::pin(futures::future::pending()) as BoxFuture<'static, Result<(), String>>)
            .over(LOCAL)
            .waiting_for_keep(Duration::from_millis(50));
        let first = tokio::time::timeout(Duration::from_secs(2), session.bearer(false)).await;
        assert_eq!(first.expect("the refresh waited on the keyring for good").unwrap(), "a2");
        let second = tokio::time::timeout(Duration::from_secs(2), session.bearer(false)).await;
        assert_eq!(second.expect("the refresh waited on the keyring for good").unwrap(), "a3");
    }

    #[tokio::test]
    async fn a_refused_token_answer_still_keeps_the_rotation_it_carried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token": "a2", "token_type": "mac", "expires_in": 3600, "refresh_token": "r2"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r2"))
            .respond_with(bearer_answer(json!({"access_token": "a3", "expires_in": 3600})))
            .mount(&server)
            .await;
        let (saved, keep) = kept();
        let session = OAuthSession::new(client_of(&server), "r1".into()).on_rotated(keep).over(LOCAL);
        assert_eq!(session.bearer(false).await.unwrap_err(), JmapError::NotBearer);
        assert_eq!(*saved.lock().unwrap(), ["r2"]);
        assert_eq!(session.bearer(false).await.unwrap(), "a3");
    }

    #[tokio::test]
    async fn an_answer_that_does_not_decode_still_keeps_the_rotation_it_carried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=r1"))
            .respond_with(bearer_answer(json!({"access_token": 42, "refresh_token": "r2"})))
            .expect(1)
            .mount(&server)
            .await;
        let (saved, keep) = kept();
        let session = OAuthSession::new(client_of(&server), "r1".into()).on_rotated(keep).over(LOCAL);
        assert!(matches!(session.bearer(false).await, Err(JmapError::Decode(_))));
        assert_eq!(*saved.lock().unwrap(), ["r2"]);
    }

    #[tokio::test]
    async fn a_token_that_lives_a_minute_is_used_more_than_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(bearer_answer(json!({"access_token": "a2", "expires_in": 60})))
            .expect(1)
            .mount(&server)
            .await;
        let session = OAuthSession::new(client_of(&server), "r1".into()).over(LOCAL);
        assert_eq!(session.bearer(false).await.unwrap(), "a2");
        assert_eq!(session.bearer(false).await.unwrap(), "a2");
    }

    #[tokio::test]
    async fn a_lifetime_past_any_clock_is_taken_without_a_panic() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(bearer_answer(json!({"access_token": "a2", "expires_in": u64::MAX})))
            .expect(1)
            .mount(&server)
            .await;
        let session = OAuthSession::new(client_of(&server), "r1".into()).over(LOCAL);
        assert_eq!(session.bearer(false).await.unwrap(), "a2");
        assert_eq!(session.bearer(false).await.unwrap(), "a2");
        let signed = Tokens { access_token: "a1".into(), expires_in: u64::MAX, refresh_token: Some("r1".into()) };
        assert_eq!(OAuthSession::signed_in(client_of(&server), signed).bearer(false).await.unwrap(), "a1");
    }

    /// Serves a session that names `server`'s own API, to any token.
    async fn serving_session(server: &MockServer) {
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/jmap/session"))
            .and(header("Authorization", "Bearer fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "capabilities": {"urn:ietf:params:jmap:core": {}}, "apiUrl": format!("{base}/api/"),
                "downloadUrl": format!("{base}/d/{{blobId}}"), "uploadUrl": format!("{base}/u/"), "state": "s"
            })))
            .mount(server)
            .await;
    }

    fn jmap_with(server: &MockServer, access: &str) -> JmapClient {
        let tokens = Tokens { access_token: access.into(), expires_in: 3600, refresh_token: Some("r1".into()) };
        let session = OAuthSession::signed_in(client_of(server), tokens).over(LOCAL);
        JmapClient::new(&format!("{}/jmap/session", server.uri()), Auth::OAuth(Arc::new(session))).unwrap()
    }

    async fn refreshing_to_fresh(server: &MockServer, times: u64) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(bearer_answer(json!({"access_token": "fresh", "expires_in": 3600})).set_delay(Duration::from_millis(50)))
            .expect(times)
            .mount(server)
            .await;
    }

    /// A full session on `server`, read with the stale token, whose API
    /// answers every echo.
    async fn session_read_while_stale(server: &MockServer) {
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/jmap/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "capabilities": {"urn:ietf:params:jmap:core": {"maxConcurrentRequests": 4}, "urn:ietf:params:jmap:mail": {}},
                "accounts": {"A1": {"name": "me", "isPersonal": true, "isReadOnly": false,
                    "accountCapabilities": {"urn:ietf:params:jmap:mail": {}}}},
                "primaryAccounts": {"urn:ietf:params:jmap:mail": "A1"},
                "username": "me", "apiUrl": format!("{base}/api/"),
                "downloadUrl": format!("{base}/d/{{blobId}}"), "uploadUrl": format!("{base}/u/"),
                "eventSourceUrl": format!("{base}/events/"), "state": "s"
            })))
            .mount(server)
            .await;
        Mock::given(method("POST")).and(path("/api/")).respond_with(echoed()).mount(server).await;
    }

    /// `stale` answers 401 at `at` once; `fresh` gets `answer`.
    async fn refused_then(server: &MockServer, verb: &str, at: &str, answer: ResponseTemplate) {
        Mock::given(method(verb))
            .and(path(at))
            .and(header("Authorization", "Bearer stale"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method(verb)).and(path(at)).and(header("Authorization", "Bearer fresh")).respond_with(answer).mount(server).await;
    }

    #[tokio::test]
    async fn a_download_refused_for_a_stale_token_is_refreshed_once() {
        let server = MockServer::start().await;
        refreshing_to_fresh(&server, 1).await;
        session_read_while_stale(&server).await;
        refused_then(&server, "GET", "/d/B1", ResponseTemplate::new(200).set_body_bytes(b"blob".to_vec())).await;
        assert_eq!(jmap_with(&server, "stale").download("A1", "B1", "n", "text/plain", 100).await.unwrap(), b"blob");
    }

    #[tokio::test]
    async fn an_upload_refused_for_a_stale_token_is_refreshed_once() {
        let server = MockServer::start().await;
        refreshing_to_fresh(&server, 1).await;
        session_read_while_stale(&server).await;
        let uploaded = json!({"accountId": "A1", "blobId": "B9", "type": "text/plain", "size": 4});
        refused_then(&server, "POST", "/u/", ResponseTemplate::new(200).set_body_json(uploaded)).await;
        let got = jmap_with(&server, "stale").upload("A1", "text/plain", b"blob".to_vec()).await.unwrap();
        assert_eq!(got.blob_id, "B9");
        let bodies: Vec<Vec<u8>> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/u/")
            .map(|r| r.body)
            .collect();
        assert_eq!(bodies, [b"blob".to_vec(), b"blob".to_vec()], "the same bytes go again");
    }

    #[tokio::test]
    async fn a_push_stream_refused_for_a_stale_token_is_refreshed_once() {
        let server = MockServer::start().await;
        refreshing_to_fresh(&server, 1).await;
        session_read_while_stale(&server).await;
        let body = "event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"A1\":{\"Email\":\"e2\"}}}\n\n";
        let stream = ResponseTemplate::new(200).insert_header("Content-Type", "text/event-stream").set_body_string(body);
        refused_then(&server, "GET", "/events/", stream).await;
        let pushed = jmap_with(&server, "stale").push(Duration::from_secs(5)).await.unwrap();
        assert_eq!(pushed, crate::Pushed::Changed);
    }

    fn echo() -> Request {
        let mut request = Request::new();
        request.call(crate::CORE, "Core/echo", json!({}));
        request
    }

    fn echoed() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"methodResponses": [["Core/echo", {}, "c0"]], "sessionState": "s"}))
    }

    #[tokio::test]
    async fn a_refused_access_token_is_refreshed_once() {
        let server = MockServer::start().await;
        refreshing_to_fresh(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/jmap/session"))
            .and(header("Authorization", "Bearer stale"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        serving_session(&server).await;
        assert_eq!(jmap_with(&server, "stale").session().await.unwrap().state, "s");
    }

    #[tokio::test]
    async fn two_calls_refused_together_share_one_refresh() {
        let server = MockServer::start().await;
        refreshing_to_fresh(&server, 1).await;
        // The first token reads the session, then expires on the server.
        // The session lets both calls out at once.
        Mock::given(method("GET"))
            .and(path("/jmap/session"))
            .and(header("Authorization", "Bearer stale"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "capabilities": {"urn:ietf:params:jmap:core": {"maxConcurrentRequests": 4}},
                "apiUrl": format!("{}/api/", server.uri()),
                "downloadUrl": format!("{}/d/{{blobId}}", server.uri()), "uploadUrl": format!("{}/u/", server.uri()), "state": "s"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/"))
            .and(header("Authorization", "Bearer stale"))
            .respond_with(ResponseTemplate::new(401))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/"))
            .and(header("Authorization", "Bearer fresh"))
            .respond_with(echoed())
            .expect(2)
            .mount(&server)
            .await;
        let jmap = jmap_with(&server, "stale");
        jmap.session().await.unwrap();
        let (first, second) = (echo(), echo());
        let (one, two) = tokio::join!(jmap.send(&first), jmap.send(&second));
        one.unwrap();
        two.unwrap();
    }

    #[tokio::test]
    async fn a_call_refused_after_a_refresh_is_not_sent_a_third_time() {
        let server = MockServer::start().await;
        refreshing_to_fresh(&server, 1).await;
        serving_session(&server).await;
        Mock::given(method("POST")).and(path("/api/")).respond_with(ResponseTemplate::new(401)).expect(2).mount(&server).await;
        let jmap = jmap_with(&server, "fresh");
        jmap.session().await.unwrap();
        assert_eq!(jmap.send(&echo()).await.unwrap_err(), JmapError::Unauthorized);
    }

    #[tokio::test]
    async fn a_call_that_fails_otherwise_is_not_sent_again() {
        let server = MockServer::start().await;
        refreshing_to_fresh(&server, 0).await;
        serving_session(&server).await;
        Mock::given(method("POST")).and(path("/api/")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
        let jmap = jmap_with(&server, "fresh");
        jmap.session().await.unwrap();
        assert!(jmap.send(&echo()).await.is_err());
    }

    #[tokio::test]
    async fn a_refused_refresh_asks_to_sign_in_again_without_the_servers_words() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": "invalid_grant", "error_description": "grant tok-SECRET was revoked"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/jmap/session")).respond_with(ResponseTemplate::new(401)).expect(1).mount(&server).await;
        let err = jmap_with(&server, "stale").session().await.unwrap_err();
        assert_eq!(err, JmapError::Unauthorized);
        assert!(!format!("{err} {err:?}").contains("SECRET"), "{err:?}");
    }

    #[tokio::test]
    async fn a_session_retired_mid_refresh_keeps_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                bearer_answer(json!({"access_token": "a2", "expires_in": 3600, "refresh_token": "r2"}))
                    .set_delay(Duration::from_millis(300)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (saved, keep) = kept();
        let session = Arc::new(OAuthSession::new(client_of(&server), "r1".into()).on_rotated(keep).over(LOCAL));
        let refreshing = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.bearer(false).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A new sign-in or a removal retires the session while its refresh
        // waits on the server.
        let last = session.retire().await;
        assert_eq!(refreshing.await.unwrap().unwrap(), "a2");
        assert!(saved.lock().unwrap().is_empty(), "a retired session wrote the keyring");
        assert_eq!(last, "r2", "retiring hands back the newest refresh token");
    }

    #[tokio::test]
    async fn a_retired_session_stops_saving_a_rotation_the_keyring_refused() {
        let server = MockServer::start().await;
        rotating_once(&server, 3600).await;
        let (saved, keep) = refusing_first(1);
        let session = OAuthSession::new(client_of(&server), "r1".into()).on_rotated(keep).over(LOCAL);
        assert_eq!(session.bearer(false).await.unwrap(), "a2");
        session.retire().await;
        session.bearer(false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(saved.lock().unwrap().is_empty(), "save_again ran for a retired session");
    }

    /// Metadata that names `/revoke` on the mock server.
    async fn serving_revocation(server: &MockServer) {
        let mut metadata = metadata_for(server, true);
        metadata["revocation_endpoint"] = json!(format!("{}/revoke", server.uri()));
        serving(server, metadata).await;
    }

    #[tokio::test]
    async fn revoking_sends_the_refresh_token_with_its_hint() {
        let server = MockServer::start().await;
        serving_revocation(&server).await;
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .and(body_string_contains("token=r1"))
            .and(body_string_contains("token_type_hint=refresh_token"))
            .and(body_string_contains("client_id=c1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let revoked = revoke_over(&session_of(&server), "c1", "r1", LOCAL, Duration::from_secs(5)).await;
        assert_eq!(revoked, Ok(true));
    }

    #[tokio::test]
    async fn a_server_that_names_no_revocation_endpoint_is_left_alone() {
        let server = MockServer::start().await;
        serving_metadata(&server, true).await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
        assert_eq!(revoke_over(&session_of(&server), "c1", "r1", LOCAL, Duration::from_secs(5)).await, Ok(false));
    }

    #[tokio::test]
    async fn a_revocation_that_hangs_is_given_up_soon() {
        let server = MockServer::start().await;
        serving_revocation(&server).await;
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .mount(&server)
            .await;
        let started = std::time::Instant::now();
        let revoked = revoke_over(&session_of(&server), "c1", "r1", LOCAL, Duration::from_millis(200)).await;
        assert!(revoked.is_err());
        assert!(started.elapsed() < Duration::from_secs(2), "the removal would wait on the server");
    }

    #[tokio::test]
    async fn a_refused_revocation_names_nothing_the_server_wrote() {
        let server = MockServer::start().await;
        serving_revocation(&server).await;
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_request", "error_description": "SECRET r1"})))
            .mount(&server)
            .await;
        let err = revoke_over(&session_of(&server), "c1", "r1", LOCAL, Duration::from_secs(5)).await.unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("SECRET") && !shown.contains("r1"), "{shown}");
    }

    #[tokio::test]
    async fn a_revocation_endpoint_off_https_is_never_called() {
        let refused = revoke_at("http://auth.example.com/revoke", "c1", "r1", Transport::HttpsOnly).await;
        assert_eq!(refused, Err(JmapError::NotHttps));
    }
}
