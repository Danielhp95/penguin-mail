//! Read receipts (RFC 8098): sent only when the person says so, never on
//! their own. The server's own way where it has one (`MDN/send`), a
//! `multipart/report` sent as ordinary mail everywhere else. Either answer
//! is kept on this computer, which is what stops the question coming
//! back, and marks `$mdnsent` on IMAP and JMAP for the person's other
//! mail programs.
//!
//! A receipt goes out at most once. The record is written before the send,
//! so a lost answer or a restart cannot send a second one; the cost is that
//! a send that failed is not retried. A receipt also goes only to the
//! sender's own address: one to anyone else tells a stranger the mail was
//! opened.

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as MimeAddress;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::MimePart;
use mailrs_domain::EpochMillis;
use mailrs_domain::mailbox::keyword::MDN_SENT;
use mailrs_domain::translate::{fill, gettext};
use mailrs_store::receipts::{self, Answered};
use mailrs_store::{accounts, bodies, messages};

use super::AccountSync;
use crate::{AnyMail, BackendError, MailBackend, MailOp, ReceiptOut, SyncError, now_millis};

/// "Read: Allotment rota".
fn receipt_subject(subject: &str) -> String {
    fill(&gettext("Read: {subject}"), &[("subject", subject)])
}

/// What the sender reads. RFC 8098 asks that a receipt claim no more than
/// that the message was shown.
fn receipt_text(me: &str) -> String {
    fill(
        &gettext("Your message to {address} was shown on screen. This receipt does not say whether it was read or understood."),
        &[("address", me)],
    )
}

/// The receipt as mail: the person's words, then the machine's.
fn receipt_mail(receipt: &ReceiptOut, original: Option<&str>, now: EpochMillis) -> Result<Vec<u8>, String> {
    let mut report = format!("Reporting-UA: Penguin Mail\r\nFinal-Recipient: rfc822;{}\r\n", receipt.from);
    if let Some(id) = original.map(|id| id.trim().trim_matches(['<', '>'])).filter(|id| !id.is_empty()) {
        report.push_str(&format!("Original-Message-ID: <{id}>\r\n"));
    }
    report.push_str("Disposition: manual-action/MDN-sent-manually; displayed\r\n");
    let domain = receipt.from.rsplit_once('@').map_or("penguin-mail.local", |(_, d)| d);
    MessageBuilder::new()
        .from(MimeAddress::from(receipt.from.clone()))
        .to(MimeAddress::from(receipt.to.clone()))
        .subject(receipt.subject.clone())
        .date(now / 1000)
        .message_id(format!("{}.mdn@{domain}", mailrs_gmail::random_token(12)))
        .body(MimePart::new(
            ContentType::new("multipart/report").attribute("report-type", "disposition-notification"),
            vec![
                MimePart::new("text/plain", receipt.text.clone()),
                MimePart::new("message/disposition-notification", report),
            ],
        ))
        .write_to_vec()
        .map_err(|err| err.to_string())
}

impl AccountSync {
    /// Whether the person already answered the receipt `message_id` asks
    /// for, here or, where the server keeps `$mdnsent`, anywhere.
    pub async fn receipt_answered(&self, message_id: &str) -> Result<bool, SyncError> {
        let (account, id) = (self.account_id, message_id.to_string());
        Ok(self
            .db
            .read(move |c| {
                if receipts::answered(c, account, &id)?.is_some() {
                    return Ok(true);
                }
                Ok(messages::by_ids(c, account, std::slice::from_ref(&id))?
                    .iter()
                    .any(|m| m.held.keywords.iter().any(|k| k == MDN_SENT)))
            })
            .await?)
    }

    /// Sends the receipt `message_id` asks for, or declines it, and
    /// records the answer. A message already answered, here or on the
    /// server, is left alone.
    pub async fn answer_receipt(&self, message_id: &str, send: bool) -> Result<(), SyncError> {
        let (account, id) = (self.account_id, message_id.to_string());
        let (me, meta, body) = self
            .db
            .read(move |c| {
                Ok((
                    accounts::account(c, account)?,
                    messages::by_ids(c, account, std::slice::from_ref(&id))?.into_iter().next(),
                    bodies::peek_body(c, account, &id)?,
                ))
            })
            .await?;
        let me = me.ok_or(BackendError::NotFound)?.email;
        let meta = meta.ok_or(BackendError::NotFound)?;
        let no_receipt = || BackendError::Refused(gettext("This message asks for no read receipt."));
        let to = body.and_then(|b| b.receipt_to).ok_or_else(no_receipt)?;
        // Defence in depth: the card offers a receipt only to the sender.
        if !meta.from.as_ref().is_some_and(|from| from.email.eq_ignore_ascii_case(&to)) {
            return Err(no_receipt().into());
        }
        if self.receipt_answered(message_id).await? {
            return Ok(());
        }
        let answered = if send { Answered::Sent } else { Answered::Declined };
        let (id, now) = (message_id.to_string(), now_millis());
        self.db.write(move |c| receipts::record(c, account, &id, answered, now)).await?;
        let remote = self.remote(message_id).await?;
        if send {
            let receipt = ReceiptOut {
                message_id: remote.clone(),
                from: me.clone(),
                to,
                subject: receipt_subject(&meta.subject),
                text: receipt_text(&me),
            };
            match self.services.mail.send_receipt(&receipt).await {
                Ok(()) => {}
                Err(BackendError::Unsupported) => {
                    let raw = receipt_mail(&receipt, meta.rfc822_msgid.as_deref(), now_millis()).map_err(|_| no_receipt())?;
                    self.send(raw, None, None).await?;
                }
                Err(err) => return Err(err.into()),
            }
        }
        self.mark_mdn_sent(&remote).await;
        Ok(())
    }

    /// `$mdnsent` on the server, on IMAP and JMAP, for the person's other
    /// mail programs. Gmail and Outlook keep no such keyword. The record on
    /// this computer is what this app reads, so a refusal only goes to the log.
    async fn mark_mdn_sent(&self, remote: &str) {
        if !matches!(self.services.mail, AnyMail::Imap(_) | AnyMail::Jmap(_)) {
            return;
        }
        let op = MailOp::SetKeyword { keyword: MDN_SENT.to_string(), on: true };
        if self.services.mail.apply(&[remote.to_string()], &[op]).await.is_err() {
            // No server text, address or message id in the log.
            tracing::info!(account = self.account_id, "could not mark a message $mdnsent");
        }
    }
}
