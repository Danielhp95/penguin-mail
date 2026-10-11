//! JMAP's errors as the backend errors every provider shares.

use mailrs_domain::translate::gettext;
use mailrs_jmap::JmapError;

use crate::BackendError;

pub(crate) fn backend(err: JmapError) -> BackendError {
    match err {
        JmapError::Unauthorized | JmapError::BadCredential => BackendError::NeedsReauth,
        JmapError::Network(detail) => BackendError::Offline(detail),
        JmapError::RateLimited { retry_after } => BackendError::RateLimited(retry_after),
        JmapError::NotFound => BackendError::NotFound,
        JmapError::CannotCalculateChanges => BackendError::StateLost,
        JmapError::StateMismatch => BackendError::Changed,
        JmapError::AnswerTooLarge { .. } => BackendError::Refused(gettext("This message is too large to open.")),
        JmapError::UnknownCapability(_) => BackendError::Unsupported,
        JmapError::Forbidden(detail) if !detail.is_empty() => BackendError::Refused(detail),
        other => BackendError::Refused(other.to_string()),
    }
}
