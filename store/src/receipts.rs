//! The read receipts the person answered, by message, so the question is
//! asked once. IMAP and JMAP also carry `$mdnsent` on the message; Gmail
//! and Outlook have nowhere to put it, so this table is what every
//! account reads.

use mailrs_domain::{AccountId, EpochMillis};
use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answered {
    Sent,
    Declined,
}

impl Answered {
    pub fn as_str(self) -> &'static str {
        match self {
            Answered::Sent => "sent",
            Answered::Declined => "declined",
        }
    }

    pub fn parse(stored: &str) -> Option<Answered> {
        [Answered::Sent, Answered::Declined].into_iter().find(|a| a.as_str() == stored)
    }
}

pub fn record(conn: &Connection, account_id: AccountId, message_id: &str, answered: Answered, at: EpochMillis) -> Result<()> {
    conn.execute(
        "INSERT INTO receipts (account_id, message_id, answered, at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (account_id, message_id) DO UPDATE SET answered = excluded.answered, at = excluded.at",
        params![account_id, message_id, answered.as_str(), at],
    )?;
    Ok(())
}

pub fn answered(conn: &Connection, account_id: AccountId, message_id: &str) -> Result<Option<Answered>> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT answered FROM receipts WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, message_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored.as_deref().and_then(Answered::parse))
}
