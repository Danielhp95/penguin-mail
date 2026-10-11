//! What went wrong talking JMAP, as kinds the adapter in `mailrs-sync`
//! maps onto the backend errors every provider shares. The words are for
//! the log; whatever a person reads is written in `mailrs-sync` or the
//! app, through the translation catalogue.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JmapError {
    /// The server refused the token or the password (HTTP 401).
    #[error("the server refused the sign-in")]
    Unauthorized,
    #[error("network error: {0}")]
    Network(String),
    /// HTTP 429 or 503, or a `rateLimit` method error.
    #[error("the server asked to slow down")]
    RateLimited { retry_after: Option<Duration> },
    /// The request breaks a limit the session names, or the server said
    /// `requestTooLarge` or the `limit` problem.
    #[error("the request is larger than the server takes")]
    RequestTooLarge,
    #[error("the answer is larger than {limit} bytes")]
    AnswerTooLarge { limit: usize },
    /// A URL the client was handed names a host the credentials must not
    /// reach.
    #[error("refused to send the account's credentials to {0}")]
    OffHost(String),
    #[error("{0} is not a JMAP address Penguin Mail uses")]
    BadUrl(String),
    /// The token or password holds a byte an HTTP header cannot carry,
    /// such as a newline pasted with it.
    #[error("the token or password holds a character a request cannot carry")]
    BadCredential,
    /// The server no longer keeps changes since the state it was handed
    /// (RFC 8620 section 5.2).
    #[error("the server no longer keeps changes since that state")]
    CannotCalculateChanges,
    /// An `ifInState` that no longer holds.
    #[error("the state changed during the call")]
    StateMismatch,
    #[error("the server forbids that: {0}")]
    Forbidden(String),
    #[error("not found")]
    NotFound,
    /// The session does not offer what a request needs.
    #[error("the server does not offer {0}")]
    UnknownCapability(String),
    /// Any other method error, by its type.
    #[error("{kind}: {description}")]
    Method { kind: String, description: String },
    /// A request-level problem (RFC 7807) or another HTTP failure.
    #[error("HTTP {status} {kind}: {detail}")]
    Problem { status: u16, kind: String, detail: String },
    #[error("could not read the server's answer: {0}")]
    Decode(String),
}

impl JmapError {
    pub fn is_transient(&self) -> bool {
        matches!(self, JmapError::Network(_) | JmapError::RateLimited { .. })
    }
}

/// RFC 8620's problem types share this prefix.
const PROBLEM_PREFIX: &str = "urn:ietf:params:jmap:error:";

/// A method-level error (RFC 8620 section 3.6.2) as a kind.
pub fn method_error(kind: &str, description: Option<&str>) -> JmapError {
    let description = description.unwrap_or_default().to_string();
    match kind {
        "cannotCalculateChanges" => JmapError::CannotCalculateChanges,
        "stateMismatch" => JmapError::StateMismatch,
        "forbidden" | "accountReadOnly" => JmapError::Forbidden(description),
        "notFound" | "accountNotFound" => JmapError::NotFound,
        "requestTooLarge" | "tooLarge" => JmapError::RequestTooLarge,
        "rateLimit" | "serverUnavailable" => JmapError::RateLimited { retry_after: None },
        "accountNotSupportedByMethod" | "unknownMethod" => JmapError::UnknownCapability(description),
        other => JmapError::Method { kind: other.to_string(), description },
    }
}

/// A request-level problem (RFC 8620 section 3.6.1) from the HTTP status
/// and the problem's `type` and `detail`. The status decides where it
/// says more than the type.
pub fn problem(status: u16, kind: &str, detail: &str) -> JmapError {
    let short = kind.strip_prefix(PROBLEM_PREFIX).unwrap_or(kind);
    match (status, short) {
        (401, _) => JmapError::Unauthorized,
        (429 | 503, _) => JmapError::RateLimited { retry_after: None },
        (_, "unknownCapability") => JmapError::UnknownCapability(detail.to_string()),
        (_, "limit") | (413, _) => JmapError::RequestTooLarge,
        (404, _) => JmapError::NotFound,
        _ => JmapError::Problem { status, kind: short.to_string(), detail: detail.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn method_errors_map_to_kinds() {
        assert_eq!(method_error("cannotCalculateChanges", None), JmapError::CannotCalculateChanges);
        assert_eq!(method_error("stateMismatch", None), JmapError::StateMismatch);
        assert_eq!(
            method_error("forbidden", Some("no vacation for tokens")),
            JmapError::Forbidden("no vacation for tokens".into())
        );
        assert_eq!(method_error("notFound", None), JmapError::NotFound);
        assert_eq!(method_error("requestTooLarge", None), JmapError::RequestTooLarge);
        assert_eq!(method_error("rateLimit", None), JmapError::RateLimited { retry_after: None });
        assert_eq!(
            method_error("invalidArguments", Some("bad filter")),
            JmapError::Method { kind: "invalidArguments".into(), description: "bad filter".into() }
        );
    }

    #[test]
    fn request_problems_map_to_kinds() {
        assert_eq!(problem(401, "", ""), JmapError::Unauthorized);
        assert_eq!(problem(429, "", ""), JmapError::RateLimited { retry_after: None });
        assert_eq!(
            problem(400, "urn:ietf:params:jmap:error:unknownCapability", "calendars"),
            JmapError::UnknownCapability("calendars".into())
        );
        assert_eq!(problem(400, "urn:ietf:params:jmap:error:limit", "maxCallsInRequest"), JmapError::RequestTooLarge);
        assert_eq!(
            problem(400, "urn:ietf:params:jmap:error:notJSON", "x"),
            JmapError::Problem { status: 400, kind: "notJSON".into(), detail: "x".into() }
        );
    }

    #[test]
    fn only_the_network_and_rate_limits_are_worth_a_retry() {
        assert!(JmapError::Network("reset".into()).is_transient());
        assert!(JmapError::RateLimited { retry_after: Some(Duration::from_secs(2)) }.is_transient());
        assert!(!JmapError::CannotCalculateChanges.is_transient());
        assert!(!JmapError::Unauthorized.is_transient());
    }
}
