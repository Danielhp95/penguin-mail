//! The rule over deleting mail: the store is the only copy of a POP3
//! account's mail, so only a person can delete it. A sync deletes for
//! other reasons (a server that no longer holds a message or a folder, a
//! window that moved on, a sweep after a relisting), and [`allowed`]
//! refuses each of those for such an account, so a new sync path cannot
//! lose mail by forgetting to ask.
//!
//! The message and thread deletes of `messages::apply` run here. The
//! other places mail leaves the store ask [`allowed`] themselves or cannot
//! reach POP3 mail: `window::prune_window` and `window::sweep_stale` ask at
//! the top, `mailboxes::gone` asks before dropping a folder, `switch`
//! rewrites IMAP accounts alone, and removing an account takes its mail
//! through the foreign keys, as the person asked. The trigger
//! `local_messages_follow` takes a message's bytes with its row.

use mailrs_domain::{AccountId, Provider};
use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;

/// Why mail leaves the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    /// A person asked, as Delete Forever, Empty Trash or removing the
    /// account does.
    Person,
    /// A sync decided the mail should go.
    Sync,
}

/// Whether mail of `account_id` may leave the store for `cause`.
pub(crate) fn allowed(conn: &Connection, account_id: AccountId, cause: Cause) -> Result<bool> {
    Ok(cause == Cause::Person || !holds_only_copy(conn, account_id)?)
}

/// Whether the store holds the only copy of the account's mail: a POP3
/// account's mail lives here and nowhere else.
fn holds_only_copy(conn: &Connection, account_id: AccountId) -> Result<bool> {
    let provider: Option<String> = conn
        .prepare_cached("SELECT provider FROM accounts WHERE id = ?1")?
        .query_row(params![account_id], |row| row.get(0))
        .optional()?;
    Ok(provider.as_deref() == Some(Provider::Pop3.as_str()))
}

/// Deletes a message with its memberships, body and bytes. Returns its
/// thread, or `None` when the message was not stored or `cause` may not
/// delete it.
pub(crate) fn delete_message(
    conn: &Connection,
    account_id: AccountId,
    message_id: &str,
    cause: Cause,
) -> Result<Option<String>> {
    let thread_id = crate::messages::thread_id_of(conn, account_id, message_id)?;
    if thread_id.is_none() || !allowed(conn, account_id, cause)? {
        return Ok(None);
    }
    conn.prepare_cached("DELETE FROM messages WHERE account_id = ?1 AND id = ?2")?
        .execute(params![account_id, message_id])?;
    Ok(thread_id)
}

/// Deletes a thread with every message in it. Returns false, deleting
/// nothing, when `cause` may not delete the account's mail.
pub(crate) fn delete_thread(
    conn: &Connection,
    account_id: AccountId,
    thread_id: &str,
    cause: Cause,
) -> Result<bool> {
    if !allowed(conn, account_id, cause)? {
        return Ok(false);
    }
    conn.prepare_cached("DELETE FROM messages WHERE account_id = ?1 AND thread_id = ?2")?
        .execute(params![account_id, thread_id])?;
    conn.prepare_cached("DELETE FROM threads WHERE account_id = ?1 AND id = ?2")?
        .execute(params![account_id, thread_id])?;
    Ok(true)
}
