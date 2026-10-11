//! The account's one HTTP client. The session comes from the URL the
//! account was saved with, and every other call goes to a URL that
//! session names. Credentials go nowhere else: an authenticated call
//! follows no redirect, and the session fetch follows one only to the
//! same scheme, host and port. Plain HTTP is taken only on this computer.
//! Every body is read a chunk at a time under a cap.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, LOCATION, RETRY_AFTER};
use serde_json::Value;
use tokio::sync::{Semaphore, SemaphorePermit};
use url::Url;

use crate::error::problem;
use crate::request::{Request, Response};
use crate::session::{MOST_AT_ONCE, Session};
use crate::{Auth, AuthKind, JmapError};

/// The most one JSON answer may take: a page of a few hundred messages'
/// metadata is far below it.
pub const JSON_LIMIT: usize = 16 << 20;

/// What an error body may take.
const ERROR_LIMIT: usize = 64 << 10;

/// How long one call may take before the network counts as gone. Blob
/// transfers set their own.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How many same-host redirects the session fetch follows.
const MOST_REDIRECTS: usize = 5;

#[derive(Clone)]
pub struct JmapClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    auth: Auth,
    session_url: Url,
    held: Mutex<Held>,
    permits: Semaphore,
    /// Set once the permits shrank to the session's own limit.
    sized: AtomicBool,
}

#[derive(Default)]
struct Held {
    session: Option<Arc<Session>>,
    stale: bool,
}

impl std::fmt::Debug for JmapClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JmapClient").field("session_url", &self.inner.session_url.as_str()).finish_non_exhaustive()
    }
}

/// Whether `url` names this computer.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

pub(crate) fn network(err: reqwest::Error) -> JmapError {
    JmapError::Network(err.to_string())
}

impl JmapClient {
    /// A client for the session at `session_url`, which must be HTTPS, or
    /// HTTP on this computer. Nothing goes out until the first call.
    pub fn new(session_url: &str, auth: Auth) -> Result<JmapClient, JmapError> {
        let url = Url::parse(session_url).map_err(|_| JmapError::BadUrl(session_url.to_string()))?;
        let usable = url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url));
        if !usable || url.host().is_none() {
            return Err(JmapError::BadUrl(session_url.to_string()));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .user_agent(concat!("Penguin Mail/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(network)?;
        Ok(JmapClient {
            inner: Arc::new(Inner {
                http,
                auth,
                session_url: url,
                held: Mutex::new(Held::default()),
                permits: Semaphore::new(MOST_AT_ONCE),
                sized: AtomicBool::new(false),
            }),
        })
    }

    pub fn session_url(&self) -> &str {
        self.inner.session_url.as_str()
    }

    pub fn auth_kind(&self) -> AuthKind {
        self.inner.auth.kind()
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.inner.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session, fetched the first time and again after a call said it
    /// moved on.
    pub async fn session(&self) -> Result<Arc<Session>, JmapError> {
        {
            let held = self.held();
            if let (Some(session), false) = (&held.session, held.stale) {
                return Ok(Arc::clone(session));
            }
        }
        self.refresh_session().await
    }

    /// The session, fetched now.
    pub async fn refresh_session(&self) -> Result<Arc<Session>, JmapError> {
        let mut url = self.inner.session_url.clone();
        for _ in 0..=MOST_REDIRECTS {
            let response = self
                .authorized(reqwest::Method::GET, url.clone())
                .await?
                .timeout(CALL_TIMEOUT)
                .send()
                .await
                .map_err(network)?;
            match response.status().as_u16() {
                200..=299 => {
                    let bytes = read_capped(response, JSON_LIMIT).await?;
                    let session = Arc::new(Session::parse(&bytes)?);
                    for named in [&session.api_url, &session.upload_url, &session.download_url] {
                        self.endpoint(named)?;
                    }
                    if let Some(events) = &session.event_source_url {
                        self.endpoint(events)?;
                    }
                    self.size_permits(&session);
                    let mut held = self.held();
                    held.session = Some(Arc::clone(&session));
                    held.stale = false;
                    return Ok(session);
                }
                301 | 302 | 303 | 307 | 308 => {
                    let next = response
                        .headers()
                        .get(LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| url.join(v).ok())
                        .ok_or_else(|| JmapError::Decode("a redirect without a location".into()))?;
                    if !same_origin(&next, &url) {
                        return Err(JmapError::OffHost(next.host_str().unwrap_or_default().to_string()));
                    }
                    url = next;
                }
                _ => return Err(failure(response).await),
            }
        }
        Err(JmapError::Problem { status: 310, kind: "redirects".into(), detail: "too many redirects".into() })
    }

    /// Shrinks the permits to the session's `maxConcurrentRequests` the
    /// first time a session says it, when that is under the client's own.
    fn size_permits(&self, session: &Session) {
        if self.inner.sized.swap(true, Ordering::Relaxed) {
            return;
        }
        let allowed = session.limits().max_concurrent_requests.max(1);
        if allowed < MOST_AT_ONCE {
            self.inner.permits.forget_permits(MOST_AT_ONCE - allowed);
        }
    }

    /// `url`, a URL the session named, when the credentials may go there:
    /// HTTPS, or HTTP when the session itself is on this computer.
    pub(crate) fn endpoint(&self, url: &str) -> Result<Url, JmapError> {
        let parsed = Url::parse(url).map_err(|_| JmapError::BadUrl(url.to_string()))?;
        let session_is_local = self.inner.session_url.scheme() == "http";
        let fine = match parsed.scheme() {
            "https" => true,
            "http" => session_is_local && is_loopback(&parsed),
            _ => false,
        };
        match fine && parsed.host().is_some() {
            true => Ok(parsed),
            false => Err(JmapError::BadUrl(url.to_string())),
        }
    }

    /// A request with the account's credentials.
    pub(crate) async fn authorized(&self, method: reqwest::Method, url: Url) -> Result<reqwest::RequestBuilder, JmapError> {
        let header = self.inner.auth.header(false).await?;
        Ok(self.inner.http.request(method, url).header(AUTHORIZATION, header))
    }

    /// One of the account's slots for a request in flight.
    pub(crate) async fn permit(&self) -> SemaphorePermit<'_> {
        // The semaphore is never closed, so acquiring cannot fail.
        self.inner.permits.acquire().await.expect("the permits stay open")
    }

    /// Sends `request` to the session's `apiUrl`. A capability the session
    /// does not offer, a call count or a size past its limits, is refused
    /// before anything goes out.
    pub async fn send(&self, request: &Request) -> Result<Response, JmapError> {
        let session = self.session().await?;
        if let Some(missing) = request.using().find(|c| !session.capabilities.contains_key(*c)) {
            return Err(JmapError::UnknownCapability(missing.to_string()));
        }
        let limits = session.limits();
        if request.len() > limits.max_calls_in_request {
            return Err(JmapError::RequestTooLarge);
        }
        let body = serde_json::to_vec(&request.body()).map_err(|e| JmapError::Decode(e.to_string()))?;
        if body.len() as u64 > limits.max_size_request {
            return Err(JmapError::RequestTooLarge);
        }
        let url = self.endpoint(&session.api_url)?;
        let _permit = self.permit().await;
        let response = self
            .authorized(reqwest::Method::POST, url)
            .await?
            .header(CONTENT_TYPE, "application/json")
            .timeout(CALL_TIMEOUT)
            .body(body)
            .send()
            .await
            .map_err(network)?;
        if !response.status().is_success() {
            return Err(failure(response).await);
        }
        let bytes = read_capped(response, JSON_LIMIT).await?;
        let answer = Response::parse(&bytes)?;
        if !answer.session_state.is_empty() && answer.session_state != session.state {
            self.held().stale = true;
        }
        Ok(answer)
    }
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// The error a failed answer stands for: a rate limit with its wait, or
/// the problem document's type and detail.
pub(crate) async fn failure(response: reqwest::Response) -> JmapError {
    let status = response.status().as_u16();
    if matches!(status, 429 | 503) {
        return JmapError::RateLimited { retry_after: retry_after(&response) };
    }
    let body = read_capped(response, ERROR_LIMIT).await.unwrap_or_default();
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let kind = parsed.get("type").and_then(Value::as_str).unwrap_or_default();
    let detail = parsed.get("detail").and_then(Value::as_str).unwrap_or_default();
    problem(status, kind, detail)
}

/// The body, read a chunk at a time and refused once it passes `limit`,
/// so a large answer never sits whole in memory before it is refused.
pub(crate) async fn read_capped(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, JmapError> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(JmapError::AnswerTooLarge { limit });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if bytes.len() + chunk.len() > limit {
            return Err(JmapError::AnswerTooLarge { limit });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn client_with_concurrency(most: u64) -> JmapClient {
        let server = MockServer::start().await;
        let base = server.uri();
        let session = json!({
            "capabilities": { "urn:ietf:params:jmap:core": { "maxConcurrentRequests": most } },
            "apiUrl": format!("{base}/api/"),
            "downloadUrl": format!("{base}/download/"),
            "uploadUrl": format!("{base}/upload/"),
            "state": "s1"
        });
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(session)).mount(&server).await;
        let client = JmapClient::new(&format!("{base}/jmap/session"), Auth::Bearer("t".into())).unwrap();
        client.session().await.unwrap();
        client
    }

    #[tokio::test]
    async fn the_permits_shrink_to_the_sessions_limit() {
        assert_eq!(client_with_concurrency(2).await.inner.permits.available_permits(), 2);
    }

    #[tokio::test]
    async fn a_server_allowing_more_still_gets_the_clients_own_most() {
        assert_eq!(client_with_concurrency(50).await.inner.permits.available_permits(), MOST_AT_ONCE);
    }
}
