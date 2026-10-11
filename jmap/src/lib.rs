//! JMAP (RFC 8620, RFC 8621) as Penguin Mail uses it: the session, method
//! calls batched in one request with back-references, blobs under a cap,
//! and the sign-in check. The client sends the account's credentials only
//! to the hosts the session names, and every answer is read under a cap.
//! The mail adapter in `mailrs-sync` maps what this crate answers onto the
//! services every provider shares.

mod auth;
pub mod blob;
mod check;
mod request;
pub mod error;
pub mod http;
pub mod identity;
pub mod mail;
pub mod session;

pub use auth::{Auth, AuthKind};
pub use blob::{MOST_BLOB_BYTES, Uploaded};
pub use check::{Checked, Offered, check};
pub use error::JmapError;
pub use http::JmapClient;
pub use identity::{EmailAddress, Identity};
pub use request::{CallId, Request, Response};
pub use session::{
    CALENDARS, CONTACTS, CORE, Limits, MAIL, MASKED_EMAIL, MOST_AT_ONCE, SIEVE, SUBMISSION, Session, SessionAccount,
    VACATION,
};
pub use mail::{
    BODY, BodyPart, ChangesResponse, Email, EmailQuery, Got, Header, Hit, Imported, Listed, META, MailChanges, Mailbox,
    MailboxWrite, PLACES, Queried, SetError, SetResponse, Submission, Thread, Threads, pointer,
};
