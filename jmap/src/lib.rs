//! JMAP (RFC 8620, RFC 8621) as Penguin Mail uses it: the session, method
//! calls batched in one request with back-references, blobs under a cap,
//! and the sign-in check. The client sends the account's credentials only
//! to the hosts the session names, and every answer is read under a cap.
//! The mail adapter in `mailrs-sync` maps what this crate answers onto the
//! services every provider shares.

mod auth;
pub mod error;

pub use auth::{Auth, AuthKind};
pub use error::JmapError;
