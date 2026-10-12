//! Read receipts on screen: which message asks for one the reader has not
//! answered, what the card shows, and which receipts that arrived answer
//! mail the person sent.

use mailrs_domain::mailbox::keyword::MDN_SENT;
use mailrs_domain::{MessageMeta, ReadReport};

use super::OpenThread;
use super::run::ReceiptCard;
use crate::render::Receipt;

impl OpenThread {
    /// The address `meta` asks a receipt at, when the app would offer one:
    /// the sender's own address (RFC 8098 section 2.1: a receipt to anyone
    /// else tells a stranger the mail was opened), from a sender who is
    /// not the reader.
    fn asked_at<'a>(&'a self, meta: &MessageMeta) -> Option<&'a str> {
        let mine = |address: &str| self.me.iter().any(|m| m.eq_ignore_ascii_case(address));
        let from = &meta.from.as_ref()?.email;
        let to = self.bodies.get(&meta.id)?.as_ref().ok()?.receipt_to.as_deref()?;
        (to.eq_ignore_ascii_case(from) && !mine(to)).then_some(to)
    }

    /// The newest message that asks, with the address it asks at: not one
    /// the reader sent, not one asking anyone but its sender, and not one
    /// marked `$mdnsent` by any mail program.
    pub fn asks_receipt(&self) -> Option<(String, String)> {
        self.messages.iter().rev().find_map(|meta| {
            if meta.held.keywords.iter().any(|k| k == MDN_SENT) {
                return None;
            }
            Some((meta.id.clone(), self.asked_at(meta)?.to_string()))
        })
    }

    /// Puts the card up on a message, marks it at work, or takes it down.
    /// The next [`Self::page`] patches the article it sits in.
    pub fn take_receipt_card(&mut self, card: ReceiptCard) {
        self.receipt_card = match card {
            ReceiptCard::Hidden => None,
            ReceiptCard::Asked { message_id, .. } => Some((message_id, false)),
            ReceiptCard::Working => self.receipt_card.take().map(|(id, _)| (id, true)),
        };
    }

    /// The message the card asks about, while its buttons press.
    pub fn receipt_asked(&self) -> Option<String> {
        self.receipt_card
            .as_ref()
            .filter(|(_, working)| !working)
            .map(|(id, _)| id.clone())
    }

    /// What the page says about `meta`'s request for a receipt.
    pub(super) fn receipt_view<'a>(&'a self, meta: &MessageMeta) -> Option<Receipt<'a>> {
        let to = self.asked_at(meta)?;
        Some(match &self.receipt_card {
            Some((id, working)) if *id == meta.id => Receipt::Asked { to, working: *working },
            _ => Receipt::Noted,
        })
    }

    /// The receipts in the thread not yet checked against Sent, with the
    /// Message-ID each says it answers.
    pub fn unchecked_reports(&self) -> Vec<(String, String)> {
        self.messages
            .iter()
            .filter(|meta| !self.reports.contains_key(&meta.id))
            .filter_map(|meta| {
                let report = self.bodies.get(&meta.id)?.as_ref().ok()?.read_report.as_ref()?;
                Some((meta.id.clone(), report.original.clone()?))
            })
            .collect()
    }

    /// Notes whether the receipt `message_id` answers mail the person
    /// sent. One that does draws as one line; one that does not draws as
    /// the mail it is, and neither is asked about again.
    pub fn report_checked(&mut self, message_id: String, sent: bool) {
        self.reports.insert(message_id, sent);
    }

    /// The receipt `message_id` is, when it answers mail the person sent.
    pub(super) fn read_by(&self, message_id: &str) -> Option<&ReadReport> {
        if self.reports.get(message_id) != Some(&true) {
            return None;
        }
        self.bodies.get(message_id)?.as_ref().ok()?.read_report.as_ref()
    }
}
