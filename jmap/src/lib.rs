//! JMAP (RFC 8620, RFC 8621) as Penguin Mail uses it: the session, method
//! calls batched in one request with back-references, blobs under a cap,
//! and the sign-in check. The client sends the account's credentials only
//! to the hosts the session names, and every answer is read under a cap.
//! The mail adapter in `mailrs-sync` maps what this crate answers onto the
//! services every provider shares.

mod auth;
mod request;
pub mod error;
pub mod http;
pub mod session;

pub use auth::{Auth, AuthKind};
pub use error::JmapError;
pub use http::JmapClient;
pub use request::{CallId, Request, Response};
pub use session::{
    CALENDARS, CONTACTS, CORE, Limits, MAIL, MASKED_EMAIL, MOST_AT_ONCE, SIEVE, SUBMISSION, Session, SessionAccount,
    VACATION,
};
