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

/// How long a call may wait for the head of its answer while a push
/// stream is open before the stream counts as what holds it up. Cyrus
/// answers no other request of the account while a stream is open.
const STALL_LIMIT: Duration = Duration::from_secs(20);

/// How long push stays off after an API call stalled behind the stream.
/// The account polls meanwhile, and the next stream is probed again.
const PUSH_PAUSE: Duration = Duration::from_secs(30 * 60);

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
    /// Held while the session is read, so callers that find it missing or
    /// stale wait for one read instead of each starting their own.
    reading: tokio::sync::Mutex<()>,
    permits: Semaphore,
    /// Set once the permits shrank to the session's own limit.
    sized: AtomicBool,
    /// The open push stream. It takes no permit: it lives for hours.
    events: tokio::sync::Mutex<Option<crate::push::EventStream>>,
    /// Set while a push stream is open.
    streaming: AtomicBool,
    /// Set once the probe beside a new stream went unanswered; the
    /// account then polls for the client's life.
    push_off: AtomicBool,
    /// Until when push stays off after a call stalled behind the stream.
    paused_until: Mutex<Option<tokio::time::Instant>>,
    /// The task that holds the open stream's connection.
    reader: Mutex<Option<tokio::task::AbortHandle>>,
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

/// reqwest's message names the URL, and a download URL can carry the
/// account's address, so the URL stays out of the error.
pub(crate) fn network(err: reqwest::Error) -> JmapError {
    JmapError::Network(err.without_url().to_string())
}

/// `url` when it carries no user name or password. Credentials in a URL
/// would go out as a second `Authorization` header and show in the log,
/// so the refusal names the URL without them.
fn without_userinfo(mut url: Url) -> Result<Url, JmapError> {
    if url.username().is_empty() && url.password().is_none() {
        return Ok(url);
    }
    // Both setters fail only on a URL that cannot have a host, and a URL
    // with a user name has one.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Err(JmapError::BadUrl(url.to_string()))
}

impl JmapClient {
    /// A client for the session at `session_url`, which must be HTTPS, or
    /// HTTP on this computer. Nothing goes out until the first call.
    pub fn new(session_url: &str, auth: Auth) -> Result<JmapClient, JmapError> {
        // A URL that does not parse could still hold a password or an
        // address, so the error names which URL it was and none of it.
        let url = Url::parse(session_url).map_err(|_| JmapError::BadUrl("the session URL".into()))?;
        let url = without_userinfo(url)?;
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
                reading: tokio::sync::Mutex::new(()),
                permits: Semaphore::new(MOST_AT_ONCE),
                sized: AtomicBool::new(false),
                events: tokio::sync::Mutex::new(None),
                streaming: AtomicBool::new(false),
                push_off: AtomicBool::new(false),
                paused_until: Mutex::new(None),
                reader: Mutex::new(None),
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

    fn cached(&self) -> Option<Arc<Session>> {
        let held = self.held();
        held.session.as_ref().filter(|_| !held.stale).map(Arc::clone)
    }

    /// The session, fetched the first time and again after a call said it
    /// moved on. Callers that arrive during a read wait for its result.
    pub async fn session(&self) -> Result<Arc<Session>, JmapError> {
        if let Some(session) = self.cached() {
            return Ok(session);
        }
        let _reading = self.inner.reading.lock().await;
        if let Some(session) = self.cached() {
            return Ok(session);
        }
        self.read_and_keep().await
    }

    /// The session, fetched now.
    pub async fn refresh_session(&self) -> Result<Arc<Session>, JmapError> {
        let _reading = self.inner.reading.lock().await;
        self.read_and_keep().await
    }

    /// Reads the session under a permit and keeps it. The caller holds
    /// `reading`.
    async fn read_and_keep(&self) -> Result<Arc<Session>, JmapError> {
        let session = {
            let _permit = self.permit().await;
            Arc::new(self.read_session().await?)
        };
        // The permit is back by now, and no call holds one before the
        // first session exists, so every permit is free to forget.
        self.size_permits(&session);
        let mut held = self.held();
        held.session = Some(Arc::clone(&session));
        held.stale = false;
        Ok(session)
    }

    /// The session document, following redirects on the same origin, with
    /// every URL it names checked.
    async fn read_session(&self) -> Result<Session, JmapError> {
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
                    let session = Session::parse(&bytes)?;
                    for named in [&session.api_url, &session.upload_url, &session.download_url] {
                        self.endpoint(named)?;
                    }
                    if let Some(events) = &session.event_source_url {
                        self.endpoint(events)?;
                    }
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
                    url = without_userinfo(next)?;
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
        let parsed = Url::parse(url).map_err(|_| JmapError::BadUrl("a URL the session names".into()))?;
        let parsed = without_userinfo(parsed)?;
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

    pub(crate) fn events(&self) -> &tokio::sync::Mutex<Option<crate::push::EventStream>> {
        &self.inner.events
    }

    pub(crate) fn set_streaming(&self, open: bool) {
        self.inner.streaming.store(open, Ordering::SeqCst);
    }

    pub(crate) fn push_off(&self) -> bool {
        self.inner.push_off.load(Ordering::SeqCst)
    }

    /// When push comes back, while it is paused after a stall.
    pub fn push_paused(&self) -> Option<tokio::time::Instant> {
        let mut until = self.inner.paused_until.lock().unwrap_or_else(PoisonError::into_inner);
        if until.is_some_and(|at| at <= tokio::time::Instant::now()) {
            *until = None;
        }
        *until
    }

    /// Records the task that holds a newly opened stream.
    pub(crate) fn keep_reader(&self, reader: tokio::task::AbortHandle) {
        *self.inner.reader.lock().unwrap_or_else(PoisonError::into_inner) = Some(reader);
    }

    /// Turns push off for the client's life and closes the stream.
    pub(crate) fn stop_pushing(&self) {
        self.inner.push_off.store(true, Ordering::SeqCst);
        self.close_stream();
    }

    /// Turns push off for a while and closes the stream.
    fn pause_pushing(&self) {
        *self.inner.paused_until.lock().unwrap_or_else(PoisonError::into_inner) = Some(tokio::time::Instant::now() + PUSH_PAUSE);
        self.close_stream();
    }

    /// Aborts the task that holds the stream, which drops the connection
    /// whether or not anyone polls the watch that waits on it. The watch
    /// finds the stream closed when it next runs.
    fn close_stream(&self) {
        if let Some(reader) = self.inner.reader.lock().unwrap_or_else(PoisonError::into_inner).take() {
            reader.abort();
        }
        self.set_streaming(false);
        if let Ok(mut held) = self.inner.events.try_lock() {
            *held = None;
        }
    }

    /// Sends an API call and waits for the head of its answer. While a
    /// push stream is open, a head that takes past the stall limit pauses
    /// push and drops the stream, and the wait goes on: the call may be a
    /// write, and giving up on it could let it run later unseen. Only the
    /// head is timed, so a slow body is never taken for a stall.
    async fn sent(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, JmapError> {
        let answer = request.send();
        tokio::pin!(answer);
        if self.inner.streaming.load(Ordering::SeqCst) && !self.push_off() {
            tokio::select! {
                got = &mut answer => return got.map_err(network),
                _ = tokio::time::sleep(STALL_LIMIT) => {
                    tracing::warn!("a call stalled while the push stream was open; pausing push");
                    self.pause_pushing();
                }
            }
        }
        answer.await.map_err(network)
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
        let request = self
            .authorized(reqwest::Method::POST, url)
            .await?
            .header(CONTENT_TYPE, "application/json")
            .timeout(CALL_TIMEOUT)
            .body(body);
        let response = self.sent(request).await?;
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

    #[test]
    fn an_https_session_refuses_a_loopback_http_url() {
        let client = JmapClient::new("https://mail.example.org/jmap/session", Auth::Bearer("t".into())).unwrap();
        assert!(matches!(client.endpoint("http://127.0.0.1/api/"), Err(JmapError::BadUrl(_))));
        assert!(client.endpoint("https://mail.example.org/api/").is_ok());
    }

    #[tokio::test]
    async fn the_permits_shrink_to_the_sessions_limit() {
        assert_eq!(client_with_concurrency(2).await.inner.permits.available_permits(), 2);
    }

    #[tokio::test]
    async fn a_server_allowing_one_request_leaves_one_permit() {
        assert_eq!(client_with_concurrency(1).await.inner.permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn the_session_read_waits_for_a_free_permit() {
        let client = client_with_concurrency(50).await;
        let held: Vec<_> = every_permit(&client).await;
        let waited = tokio::time::timeout(Duration::from_millis(100), client.refresh_session()).await;
        assert!(waited.is_err(), "the session read went out with every permit taken");
        drop(held);
        client.refresh_session().await.unwrap();
    }

    async fn every_permit(client: &JmapClient) -> Vec<SemaphorePermit<'_>> {
        let mut held = Vec::new();
        for _ in 0..MOST_AT_ONCE {
            held.push(client.permit().await);
        }
        held
    }

    #[tokio::test]
    async fn a_network_error_does_not_repeat_the_url() {
        let url = "http://127.0.0.1:1/download/me@example.org/b1/x";
        let err = reqwest::Client::new().get(url).send().await.unwrap_err();
        let said = network(err).to_string();
        assert!(!said.contains("me@example.org"), "{said}");
    }

    #[tokio::test(start_paused = true)]
    async fn push_comes_back_after_a_stall_has_paused_it() {
        let client = JmapClient::new("https://mail.example.org/jmap/session", Auth::Bearer("t".into())).unwrap();
        client.pause_pushing();
        assert!(client.push_paused().is_some());
        tokio::time::advance(PUSH_PAUSE).await;
        assert_eq!(client.push_paused(), None);
        assert!(!client.push_off());
    }

    #[tokio::test]
    async fn a_server_allowing_more_still_gets_the_clients_own_most() {
        assert_eq!(client_with_concurrency(50).await.inner.permits.available_permits(), MOST_AT_ONCE);
    }
}
