//! Read receipts as a step of the thread run. The card goes up only when
//! the setting allows, a message asks, and nobody answered it yet; nothing
//! is sent until a button is pressed. A receipt that arrived draws as one
//! line once the store says it answers mail the person sent.

use mailrs_domain::translate::{fill, gettext};

use super::{ThreadRun, Want};

/// What the receipt card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptCard {
    Hidden,
    /// `to` asked to know when `message_id` was read.
    Asked { message_id: String, to: String },
    /// The answer is on its way.
    Working,
}

impl ThreadRun {
    pub(super) async fn receipt(&self, wanted: &Want<'_>) {
        futures::join!(self.receipt_card(wanted), self.reports(wanted));
    }

    async fn receipt_card(&self, wanted: &Want<'_>) {
        if wanted.on_screen(|_| self.desk.offers_receipts()) != Some(true) {
            return;
        }
        let Some(Some((message_id, to))) = wanted.on_screen(|_| self.desk.asks_receipt()) else {
            return;
        };
        let account_id = wanted.target().account_id;
        let asked = message_id.clone();
        let Some(answered) = wanted
            .ask(
                |effects| effects.receipt_answered(account_id, asked),
                "could not read whether the receipt was answered",
            )
            .await
        else {
            return;
        };
        if !answered {
            wanted.on_screen(|effects| effects.receipt_card(ReceiptCard::Asked { message_id, to }));
        }
    }

    /// Checks each receipt in the thread against the person's Sent mail.
    /// Anyone can send a report naming anyone, so one that answers nothing
    /// the person sent stays the mail it is.
    async fn reports(&self, wanted: &Want<'_>) {
        let Some(unchecked) = wanted.on_screen(|_| self.desk.unchecked_reports()) else {
            return;
        };
        let account_id = wanted.target().account_id;
        for (message_id, original) in unchecked {
            let Some(sent) = wanted
                .ask(
                    |effects| effects.sent_here(account_id, original),
                    "could not look for the message a receipt answers",
                )
                .await
            else {
                return;
            };
            wanted.on_screen(|effects| effects.report_checked(message_id, sent));
        }
    }

    /// The card's buttons: send the receipt, or decline it for good.
    pub async fn answer_receipt(&self, message_id: String, send: bool) {
        let Some(wanted) = self.on_screen() else {
            return;
        };
        let account_id = wanted.target().account_id;
        wanted.on_screen(|effects| effects.receipt_card(ReceiptCard::Working));
        let asked = message_id.clone();
        let Some(done) = wanted
            .wait(|effects| effects.answer_receipt(account_id, asked, send))
            .await
        else {
            return;
        };
        match done {
            Ok(()) => {
                wanted.on_screen(|effects| effects.receipt_card(ReceiptCard::Hidden));
                if send {
                    wanted.anyway(|effects| effects.toast(gettext("Read receipt sent")));
                }
            }
            Err(problem) => {
                // The answer is kept before the receipt goes, so a send
                // that failed after that is never tried again, and the
                // card must not offer it.
                let answered = wanted
                    .wait(|effects| effects.receipt_answered(account_id, message_id))
                    .await;
                let card = match (answered, wanted.on_screen(|_| self.desk.asks_receipt())) {
                    (Some(Ok(false)), Some(Some((message_id, to)))) => ReceiptCard::Asked { message_id, to },
                    _ => ReceiptCard::Hidden,
                };
                wanted.on_screen(|effects| effects.receipt_card(card));
                let said = fill(&gettext("Could not send the receipt: {reason}"), &[("reason", &problem)]);
                wanted.anyway(|effects| effects.toast(said));
            }
        }
    }
}
