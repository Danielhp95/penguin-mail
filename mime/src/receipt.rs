//! Read receipts in a message (RFC 8098): the address a sender asks for
//! one at, and a receipt that arrived, read from its
//! `message/disposition-notification` part.

use mailrs_domain::ReadReport;

use crate::parts::Part;

/// A real report is a few short fields; anything past this is not read.
const MAX_REPORT_BYTES: usize = 16 * 1024;
const MAX_REPORT_LINES: usize = 64;

/// The address `Disposition-Notification-To` names, in lower case. The
/// header holds one mailbox (RFC 8098); anything else, or an address that
/// is not one plain `local@domain`, asks for nothing.
pub(crate) fn asked_at(header: Option<&str>) -> Option<String> {
    let header = header?;
    // The address parser keeps the last `<...>` of a header, so a second
    // angle pair would pass as if it were the only one.
    if header.matches('<').count() > 1 {
        return None;
    }
    let mut list = crate::address::parse_address_list(header);
    if list.len() != 1 {
        return None;
    }
    let email = list.remove(0).email.trim().to_ascii_lowercase();
    let plain = email.matches('@').count() == 1
        && !email.starts_with('@')
        && !email.ends_with('@')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control() || ";<>,\"".contains(c));
    plain.then_some(email)
}

/// The fields of a report, each continuation line (one that starts with
/// white space) joined to the field before it.
fn unfolded(text: &str) -> impl Iterator<Item = String> + '_ {
    let mut lines = text.lines().peekable();
    std::iter::from_fn(move || {
        let mut field = lines.next()?.to_string();
        while let Some(next) = lines.next_if(|l| l.starts_with([' ', '\t'])) {
            if field.len() < MAX_REPORT_BYTES {
                field.push(' ');
                field.push_str(next.trim());
            }
        }
        Some(field)
    })
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
    for line in unfolded(&text).take(MAX_REPORT_LINES) {
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
    use super::asked_at;
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

    fn asking(value: &str) -> Option<String> {
        let raw = format!("Subject: s\r\nDisposition-Notification-To: {value}\r\n\r\nhi\r\n");
        crate::read(raw.as_bytes()).receipt_to
    }

    #[test]
    fn a_header_that_is_not_one_plain_address_asks_nothing() {
        for bad in [
            "a@x.example;b@y.example",
            "a@x.example b@y.example",
            "<a@x.example> <b@y.example>",
            "a@@x.example",
            "\"a@x.example\"",
            "Ines <a@x.example>>",
        ] {
            assert_eq!(asking(bad), None, "{bad}");
        }
        assert_eq!(asking("Ines <a@x.example>").as_deref(), Some("a@x.example"));
        assert_eq!(asking("a@x.example").as_deref(), Some("a@x.example"));
    }

    #[test]
    fn an_address_with_a_control_character_asks_nothing() {
        assert_eq!(asked_at(Some("Ines <a@x.example\r\nBcc: z@y.example>")), None);
        assert_eq!(asked_at(Some("a\u{7}@x.example")), None);
    }

    #[test]
    fn a_header_naming_several_mailboxes_asks_nothing() {
        assert_eq!(asking("a@x.example, b@y.example"), None);
        assert_eq!(asking("Ines <a@x.example>, Tomas <b@y.example>"), None);
    }

    fn report_with(fields: &str) -> Vec<u8> {
        format!(
            "From: t@allotment.example\r\nSubject: Read\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/report; report-type=disposition-notification; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: message/disposition-notification\r\n\r\n{fields}--b--\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn a_folded_final_recipient_is_read_whole() {
        let body = crate::read(&report_with(
            "Final-Recipient: rfc822;\r\n tomas@allotment.example\r\nOriginal-Recipient: rfc822; other@allotment.example\r\nDisposition: manual-action/MDN-sent-manually; displayed\r\n",
        ));
        assert_eq!(body.read_report.map(|r| r.by).as_deref(), Some("tomas@allotment.example"));
    }

    #[test]
    fn an_empty_final_recipient_falls_back_to_the_original() {
        let body = crate::read(&report_with(
            "Final-Recipient: rfc822;\r\nOriginal-Recipient: rfc822; other@allotment.example\r\nDisposition: manual-action/MDN-sent-manually; displayed\r\n",
        ));
        assert_eq!(body.read_report.map(|r| r.by).as_deref(), Some("other@allotment.example"));
    }

    #[test]
    fn junk_inside_the_report_does_not_panic_or_hide_an_early_disposition() {
        let mut fields = String::from(
            "Final-Recipient: rfc822; tomas@allotment.example\r\nDisposition: manual-action/MDN-sent-manually; displayed\r\n",
        );
        fields.push_str(&":".repeat(100_000));
        fields.push_str("\r\n");
        let body = crate::read(&report_with(&fields));
        assert_eq!(body.read_report.map(|r| r.displayed), Some(true));
    }

    #[test]
    fn a_disposition_past_the_line_cap_is_not_read() {
        let mut fields = String::from("Final-Recipient: rfc822; tomas@allotment.example\r\n");
        fields.push_str(&"X-Pad: 1\r\n".repeat(100));
        fields.push_str("Disposition: manual-action/MDN-sent-manually; displayed\r\n");
        assert_eq!(crate::read(&report_with(&fields)).read_report, None);
    }

    #[test]
    fn a_disposition_past_the_byte_cap_is_not_read() {
        let mut fields = String::from("X-Pad: ");
        fields.push_str(&"p".repeat(20 * 1024));
        fields.push_str("\r\nFinal-Recipient: rfc822; tomas@allotment.example\r\nDisposition: x/y; displayed\r\n");
        assert_eq!(crate::read(&report_with(&fields)).read_report, None);
    }

    #[test]
    fn a_hostile_address_header_does_not_panic() {
        let _ = crate::read(b"Disposition-Notification-To: <<<;;;,,,\r\n\r\n");
    }
}
