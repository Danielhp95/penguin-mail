//! Mail written out as the files other programs read: one message as the
//! RFC 822 bytes Gmail holds, a conversation as an mbox.
//!
//! The mbox is mboxrd, the form that survives a round trip. Every line of
//! the message that already reads `From `, however many `>` marks stand in
//! front of it, takes one more, so a reader strips one back off and gets
//! the message it started with. Nothing else about the message changes:
//! its own line endings, CRLF and all, go into the file as they arrived,
//! and only the lines mbox itself adds end in a bare newline.

use std::path::{Path, PathBuf};

use mailrs_domain::{EpochMillis, Target};
use tokio::io::AsyncWriteExt;
use mailrs_mime::address::parse_address_list;

use crate::{Accounts, MailActions, SyncError};

/// The sender an mbox separator names when the message has no usable
/// `From` header. Mail systems have written this since Unix mail began.
const UNKNOWN_SENDER: &str = "MAILER-DAEMON";

/// How many bytes of subject a file name may carry. Linux takes 255 bytes
/// for one name; the rest leaves room for the date and the extension.
const SUBJECT_BYTES: usize = 180;

impl<A: Accounts> MailActions<A> {
    /// The mail `targets` name as one mbox file, in the order given: each
    /// conversation oldest message first, or the one message a target
    /// names. The window's Export and the assistant both write this.
    pub async fn export_mbox(&self, targets: &[Target]) -> Result<Vec<u8>, SyncError> {
        let mut mbox = Vec::new();
        self.export_mbox_to(targets, &mut mbox).await?;
        Ok(mbox)
    }

    /// `export_mbox`, written to `out` one message at a time, so the file
    /// is fed as the messages arrive and the mail is never held whole.
    pub async fn export_mbox_to<W: tokio::io::AsyncWrite + Unpin>(
        &self,
        targets: &[Target],
        out: &mut W,
    ) -> Result<(), SyncError> {
        for target in targets {
            let sync = self
                .accounts
                .account(target.account_id)
                .ok_or(SyncError::UnknownAccount(target.account_id))?;
            sync.export_mbox_to(&target.thread_id, target.message_id.as_deref(), out)
                .await?;
        }
        Ok(())
    }

    /// `export_mbox` into the file at `path`, written beside it under a
    /// temporary name and renamed over it when every message is in, so a
    /// failed export leaves what was there. Answers the file's length.
    pub async fn export_mbox_file(&self, targets: &[Target], path: &Path) -> Result<u64, SyncError> {
        let mut file = Replacing::create(path).await.map_err(SyncError::io)?;
        self.export_mbox_to(targets, file.file()).await?;
        file.finish().await.map_err(SyncError::io)
    }

    /// One message as the RFC 822 bytes Gmail holds, the whole of an
    /// `.eml` file.
    pub async fn export_message(
        &self,
        account_id: mailrs_domain::AccountId,
        message_id: &str,
    ) -> Result<std::sync::Arc<Vec<u8>>, SyncError> {
        let sync = self
            .accounts
            .account(account_id)
            .ok_or(SyncError::UnknownAccount(account_id))?;
        sync.raw_message(message_id).await
    }
}

/// A file written beside `path` under a temporary name and renamed over
/// it once whole, so a failed or interrupted write leaves what was there.
/// Dropped before [`Replacing::finish`], it takes the temporary file away.
pub struct Replacing {
    file: Option<tokio::fs::File>,
    partial: PathBuf,
    path: PathBuf,
}

impl Replacing {
    pub async fn create(path: &Path) -> std::io::Result<Replacing> {
        let mut partial = path.as_os_str().to_owned();
        partial.push(".part");
        let partial = PathBuf::from(partial);
        let file = tokio::fs::File::create(&partial).await?;
        Ok(Replacing { file: Some(file), partial, path: path.to_path_buf() })
    }

    /// The file to write to.
    pub fn file(&mut self) -> &mut tokio::fs::File {
        self.file.as_mut().expect("a file until finish")
    }

    /// Puts the written file in the place of the old one, once its bytes
    /// are on the disk: a rename that reached the disk before them would
    /// leave an empty file after a crash. Answers the file's length.
    pub async fn finish(mut self) -> std::io::Result<u64> {
        let mut file = self.file.take().expect("a file until finish");
        let finished = async {
            file.flush().await?;
            file.sync_all().await?;
            let length = file.metadata().await?.len();
            tokio::fs::rename(&self.partial, &self.path).await?;
            Ok(length)
        }
        .await;
        if finished.is_err() {
            let _ = std::fs::remove_file(&self.partial);
        }
        finished
    }
}

impl Drop for Replacing {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.partial);
        }
    }
}

/// Adds `raw` to `out` as one mbox entry: a `From ` separator built from
/// the message's own headers, the message with its `From ` lines quoted,
/// and the blank line that parts it from whatever comes next.
pub fn append(out: &mut Vec<u8>, raw: &[u8]) {
    out.extend_from_slice(separator(raw).as_bytes());
    for line in lines(raw) {
        if reads_from(line) {
            out.push(b'>');
        }
        out.extend_from_slice(line);
    }
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// A name for the file this mail is saved to, such as
/// `Lunch plans 2026-01-05.mbox`. The subject is the sender's text, so
/// path separators and control characters come out as spaces before it
/// reaches a save dialog. The date is UTC, which names one message one
/// way wherever it is exported.
pub fn file_name(subject: &str, date: EpochMillis, extension: &str) -> String {
    let mut name = String::new();
    let mut space = true;
    for c in subject.chars() {
        let plain = if c == '/' || c == '\\' || c.is_control() {
            ' '
        } else {
            c
        };
        if plain.is_whitespace() {
            space = true;
            continue;
        }
        if space && !name.is_empty() {
            name.push(' ');
        }
        if name.len() + plain.len_utf8() > SUBJECT_BYTES {
            break;
        }
        name.push(plain);
        space = false;
    }
    // A leading dot hides the file, and a pair of them climbs a directory.
    let name = name.trim_start_matches('.').trim();
    let day = chrono::DateTime::from_timestamp_millis(date)
        .unwrap_or_default()
        .format("%Y-%m-%d");
    format!(
        "{} {day}.{extension}",
        if name.is_empty() { "mail" } else { name }
    )
}

/// The `From ` line that opens an entry: the sender's address, then the
/// date in the form `ctime` prints, kept in the offset the message was
/// written in. A message that names neither comes from the mailer daemon
/// at the epoch, which reads as the placeholder it is.
fn separator(raw: &[u8]) -> String {
    let sender = header(raw, "From")
        .and_then(|from| parse_address_list(&from).into_iter().next())
        .map(|address| address.email)
        .filter(|email| !email.is_empty() && !email.contains(char::is_whitespace))
        .unwrap_or_else(|| UNKNOWN_SENDER.to_string());
    let date = header(raw, "Date")
        .and_then(|date| chrono::DateTime::parse_from_rfc2822(&date).ok())
        .map(|date| date.format("%a %b %e %H:%M:%S %Y").to_string())
        .unwrap_or_else(|| "Thu Jan  1 00:00:00 1970".to_string());
    format!("From {sender} {date}\n")
}

/// Whether `line` needs another quote mark: it reads `From `, behind any
/// number of marks it already carries.
fn reads_from(line: &[u8]) -> bool {
    match line.iter().position(|b| *b != b'>') {
        Some(at) => line[at..].starts_with(b"From "),
        None => false,
    }
}

/// The value of header `name`, folded lines joined. A message may repeat
/// a header; this takes the first, as a reader does.
fn header(raw: &[u8], name: &str) -> Option<String> {
    let mut value: Option<String> = None;
    for line in lines(raw) {
        let line = match line.strip_suffix(b"\n") {
            Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
            None => line,
        };
        if line.is_empty() {
            break;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if let Some(held) = value.as_mut() {
                held.push(' ');
                held.push_str(String::from_utf8_lossy(line).trim());
            }
            continue;
        }
        if value.is_some() {
            break;
        }
        if let Some(colon) = line.iter().position(|b| *b == b':')
            && line[..colon].eq_ignore_ascii_case(name.as_bytes())
        {
            value = Some(
                String::from_utf8_lossy(&line[colon + 1..])
                    .trim()
                    .to_string(),
            );
        }
    }
    value
}

/// The message's lines, each keeping the terminator it came with. The last
/// one comes as it is when the message ends without a newline.
fn lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = raw;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .iter()
            .position(|b| *b == b'\n')
            .map_or(rest.len(), |at| at + 1);
        let (line, tail) = rest.split_at(end);
        rest = tail;
        Some(line)
    })
}
