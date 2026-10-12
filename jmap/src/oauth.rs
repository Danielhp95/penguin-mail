//! OAuth for a JMAP server (draft-ietf-mailmaint-oauth-public): the
//! server's metadata beside its session (RFC 8414), PKCE S256 (RFC 7636),
//! a loopback redirect (RFC 8252), the RFC 8707 `resource`, and dynamic
//! registration of a public client (RFC 7591) where the server offers it.
//!
//! Tokens, codes and the PKCE verifier stay out of `Debug` and out of
//! every error's text.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mailrs_domain::sign_in_page::{self, Finished};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

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

/// The authorization server's metadata, beside the session URL's host.
/// Metadata without PKCE S256 is refused.
pub async fn metadata(session_url: &str) -> Result<Metadata, JmapError> {
    let session = Url::parse(session_url).map_err(|_| JmapError::BadUrl(session_url.into()))?;
    let at = session
        .join("/.well-known/oauth-authorization-server")
        .map_err(|_| JmapError::BadUrl(session_url.into()))?;
    let (status, body) = read(http()?.get(at).send().await.map_err(network)?).await?;
    if !(200..300).contains(&status) {
        return Err(JmapError::NotFound);
    }
    let found: Metadata = serde_json::from_value(body).map_err(|e| decode("the OAuth metadata", &e))?;
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

/// The address the browser opens for consent.
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
    let mut url = Url::parse(&metadata.authorization_endpoint)
        .map_err(|_| JmapError::BadUrl(metadata.authorization_endpoint.clone()))?;
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
/// id. A server without a registration endpoint refuses.
pub async fn register(metadata: &Metadata, redirect_uri: &str, scopes: &[String]) -> Result<String, JmapError> {
    let endpoint = metadata
        .registration_endpoint
        .as_ref()
        .ok_or_else(|| JmapError::Forbidden("the server registers no clients".into()))?;
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
        self.token(&[
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
        self.token(&[
            ("grant_type", "refresh_token"),
            ("client_id", &self.client_id),
            ("refresh_token", refresh_token),
            ("resource", &self.resource),
        ])
        .await
    }

    async fn token(&self, form: &[(&str, &str)]) -> Result<Tokens, JmapError> {
        let response = http()?.post(&self.token_endpoint).form(form).send().await.map_err(network)?;
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
