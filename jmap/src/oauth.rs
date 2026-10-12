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
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mailrs_domain::sign_in_page::{self, Finished};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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
    Url::parse(session_url).map_err(|_| JmapError::BadUrl(session_url.into()))?;
    let session = secure(session_url, transport)?;
    let issuer = session.origin().ascii_serialization();
    let at = session
        .join("/.well-known/oauth-authorization-server")
        .map_err(|_| JmapError::BadUrl(session_url.into()))?;
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
        let detail = answer["error_description"].as_str().or(answer["error"].as_str()).unwrap_or_default().to_string();
        return Err(JmapError::Forbidden(detail));
    }
    answer["client_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| JmapError::Decode("no client_id came back".into()))
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
        self.token(transport, &[
            ("grant_type", "refresh_token"),
            ("client_id", &self.client_id),
            ("refresh_token", refresh_token),
            ("resource", &self.resource),
        ])
        .await
    }

    async fn token(&self, transport: Transport, form: &[(&str, &str)]) -> Result<Tokens, JmapError> {
        let endpoint = secure(&self.token_endpoint, transport)?;
        let response = http()?.post(endpoint).form(form).send().await.map_err(network)?;
        let (status, body) = read(response).await?;
        if (200..300).contains(&status) {
            return serde_json::from_value(body).map_err(|e| decode("the token answer", &e));
        }
        Err(match (status, body["error"].as_str().unwrap_or_default()) {
            (_, "invalid_grant") if body["temporary"].as_bool() == Some(true) => JmapError::SignInLocked,
            (_, "invalid_grant" | "invalid_client" | "unauthorized_client") => JmapError::Unauthorized,
            (429 | 503, _) => JmapError::RateLimited { retry_after: None },
            (status, error) => JmapError::Problem {
                status,
                kind: error.to_string(),
                detail: body["error_description"].as_str().unwrap_or_default().to_string(),
            },
        })
    }
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

    /// Answers requests until one carries the code or an error, then
    /// closes the listener. A request that is not the redirect, such as a
    /// favicon, gets a 404 and the listener waits on.
    pub async fn wait_for_code(self, state: &str) -> Result<String, JmapError> {
        loop {
            let (mut stream, _) = self.listener.accept().await.map_err(io)?;
            let mut buf = vec![0u8; REQUEST_LIMIT];
            let n = match tokio::time::timeout(REQUEST_WAIT, stream.read(&mut buf)).await {
                Ok(read) => read.map_err(io)?,
                Err(_) => continue,
            };
            let outcome = parse_redirect(&String::from_utf8_lossy(&buf[..n]), state);
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

fn io(err: std::io::Error) -> JmapError {
    JmapError::Network(format!("loopback listener: {}", err.kind()))
}

/// The code out of a raw HTTP request; `None` for a request that is not
/// the redirect, such as a favicon. A code or an error under another
/// `state` stops the sign-in as a mismatch: it did not come from the
/// consent this sign-in opened.
pub fn parse_redirect(request: &str, state: &str) -> Option<Result<String, JmapError>> {
    let target = request.lines().next()?.split_whitespace().nth(1)?;
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let error = params.get("error");
    let code = params.get("code");
    if error.is_none() && code.is_none() {
        return None;
    }
    if params.get("state").map(String::as_str) != Some(state) {
        return Some(Err(JmapError::Forbidden("the redirect's state did not match".into())));
    }
    if let Some(error) = error {
        return Some(Err(match error.as_str() {
            "access_denied" => JmapError::Declined,
            other => JmapError::Forbidden(other.to_string()),
        }));
    }
    code.map(|code| Ok(code.clone()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    // The mock servers speak http, which only this module's transport
    // takes.
    const LOCAL: Transport = Transport::LocalHttp;

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
            "scopes_supported": ["urn:ietf:params:oauth:scope:mail", "urn:ietf:params:oauth:scope:contacts", "openid", "offline_access"]
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
                    .set_body_json(json!({"access_token": "a1", "expires_in": 3600, "refresh_token": "r1"})),
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
}
