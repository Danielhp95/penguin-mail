//! JMAP's errors as the backend errors every provider shares. A server's
//! own description of a refusal stays out: it is in English whatever the
//! person reads, and it can name an address or a message's id, which the
//! log must not hold either.

use mailrs_domain::translate::gettext;
use mailrs_jmap::JmapError;

use crate::BackendError;

pub(crate) fn backend(err: JmapError) -> BackendError {
    match err {
        JmapError::Unauthorized | JmapError::BadCredential | JmapError::UnknownClient => BackendError::NeedsReauth,
        JmapError::Network(detail) => BackendError::Offline(detail),
        JmapError::RateLimited { retry_after } => BackendError::RateLimited(retry_after),
        JmapError::NotFound => BackendError::NotFound,
        JmapError::CannotCalculateChanges => BackendError::StateLost,
        JmapError::StateMismatch => BackendError::Changed,
        JmapError::AnswerTooLarge { .. } => BackendError::Refused(gettext("This message is too large to open.")),
        JmapError::UnknownCapability(_) => BackendError::Unsupported,
        JmapError::Forbidden(_) => BackendError::Refused(gettext("The server does not allow this.")),
        JmapError::Method { .. } => BackendError::Refused(gettext("The server refused this request.")),
        JmapError::Problem { .. } => BackendError::Refused(gettext("The server could not take this request.")),
        other => BackendError::Refused(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use mailrs_jmap::JmapError;

    use super::backend;

    /// A description a server wrote, naming an address, as a filter error can.
    const SAID: &str = "unsupportedFilter: no filter for ada@example.com in M42";

    #[test]
    fn a_server_description_never_reaches_the_refusal_or_the_log() {
        let errors = [
            JmapError::Method { kind: "unsupportedFilter".into(), description: SAID.into() },
            JmapError::Problem { status: 400, kind: "notRequest".into(), detail: SAID.into() },
            JmapError::Forbidden(SAID.into()),
            JmapError::Forbidden(String::new()),
        ];
        for error in errors {
            let mapped = backend(error);
            let (shown, logged) = (mapped.to_string(), format!("{mapped:?}"));
            assert!(matches!(mapped, crate::BackendError::Refused(_)), "{logged}");
            assert!(!shown.contains("ada@example.com") && !logged.contains("ada@example.com"), "{logged}");
            assert!(!shown.contains("M42") && !logged.contains("unsupportedFilter"), "{logged}");
        }
    }
}
