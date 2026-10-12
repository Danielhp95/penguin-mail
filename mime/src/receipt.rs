//! Read receipts in a message (RFC 8098): the address a sender asks for
//! one at, and a receipt that arrived, read from its
//! `message/disposition-notification` part.

use mailrs_domain::ReadReport;

use crate::parts::Part;

/// A real report is a few short fields; anything past this is not read.
const MAX_REPORT_BYTES: usize = 16 * 1024;
const MAX_REPORT_LINES: usize = 64;

/// The address `Disposition-Notification-To` names, in lower case.
pub(crate) fn asked_at(header: Option<&str>) -> Option<String> {
    crate::address::parse_address_list(header?)
        .into_iter()
        .map(|a| a.email.trim().to_ascii_lowercase())
        .find(|email| email.contains('@'))
}

/// The receipt `root` is, when it is a `multipart/report` with a
/// disposition part that names who sent it and what became of the
/// message.
pub(crate) fn report(root: &Part) -> Option<ReadReport> {
    if root.mime_type != "multipart/report" {
        return None;
    }
    let part = root.children.iter().find(|c| c.mime_type == "message/disposition-notification")?;
    let data = part.data.as_deref()?;
    let text = String::from_utf8_lossy(&data[..data.len().min(MAX_REPORT_BYTES)]);
    let (mut final_recipient, mut original_recipient, mut displayed) = (None, None, None);
    for line in text.lines().take(MAX_REPORT_LINES) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // "rfc822; tomas@allotment.example": the address after the type.
        let address = || value.split_once(';').map_or(value, |(_, a)| a).trim().to_ascii_lowercase();
        match name.trim().to_ascii_lowercase().as_str() {
            "final-recipient" => final_recipient = Some(address()),
            "original-recipient" => original_recipient = Some(address()),
            // "manual-action/MDN-sent-manually; displayed", maybe with
            // "/error" after the type.
            "disposition" => {
                let kind = value.rsplit(';').next().unwrap_or_default();
                displayed = Some(kind.split('/').next().unwrap_or_default().trim().eq_ignore_ascii_case("displayed"));
            }
            _ => {}
        }
    }
    let by = final_recipient.filter(|b| !b.is_empty()).or(original_recipient).filter(|b| !b.is_empty())?;
    Some(ReadReport { by, displayed: displayed? })
}

#[cfg(test)]
mod tests {
    use mailrs_domain::ReadReport;

    const ASKS: &[u8] = b"From: Ines Duarte <ines@allotment.example>\r\nTo: dana@fastmail.example\r\n\
Subject: Rota\r\nDisposition-Notification-To: Ines Duarte <Ines@Allotment.example>\r\n\r\nThe rota is up.\r\n";

    fn report(disposition: &str) -> Vec<u8> {
        format!(
            "From: tomas@allotment.example\r\nTo: dana@fastmail.example\r\nSubject: Read: Rota\r\n\
MIME-Version: 1.0\r\nContent-Type: multipart/report; report-type=disposition-notification; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nYour message was shown.\r\n\
--b\r\nContent-Type: message/disposition-notification\r\n\r\n\
Reporting-UA: Mail; Mail\r\nFinal-Recipient: rfc822; tomas@allotment.example\r\n\
Original-Message-ID: <rota@fastmail.example>\r\nDisposition: manual-action/MDN-sent-manually; {disposition}\r\n\
--b--\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn a_message_that_asks_names_where_in_lower_case() {
        assert_eq!(crate::read(ASKS).receipt_to.as_deref(), Some("ines@allotment.example"));
        assert_eq!(crate::read(b"Subject: s\r\n\r\nhi\r\n").receipt_to, None);
    }

    #[test]
    fn a_receipt_says_who_read_it_and_lists_no_file() {
        let body = crate::read(&report("displayed"));
        assert_eq!(body.read_report, Some(ReadReport { by: "tomas@allotment.example".into(), displayed: true }));
        assert!(body.attachments.is_empty());
    }

    #[test]
    fn a_receipt_for_mail_deleted_unread_says_so() {
        assert_eq!(crate::read(&report("deleted")).read_report.map(|r| r.displayed), Some(false));
    }

    #[test]
    fn an_ordinary_message_is_no_receipt() {
        assert_eq!(crate::read(ASKS).read_report, None);
    }

    #[test]
    fn a_hostile_report_does_not_panic() {
        let mut junk = report("displayed");
        junk.extend(std::iter::repeat_n(b':', 100_000));
        let _ = crate::read(&junk);
        let _ = crate::read(b"Disposition-Notification-To: <<<;;;,,,\r\n\r\n");
    }
}
