//! What reading an outgoing message's headers holds in memory.

use crate::tests::heap::ProcessMark;

/// A message of about 25 MB: a text part and one base64 file, with a Bcc.
fn big_message() -> Vec<u8> {
    let line = format!("{}\r\n", "QUFB".repeat(19));
    let mut raw = String::from(
        "From: Me <me@example.org>\r\nTo: ana@example.org\r\nBcc: cy@example.org\r\nSubject: Scans\r\n\
         Message-ID: <big@example.org>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
         --b\r\nContent-Type: text/plain\r\n\r\nThe scans are attached.\r\n--b\r\nContent-Type: application/pdf\r\n\
         Content-Disposition: attachment; filename=scans.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\n",
    );
    while raw.len() < 25 << 20 {
        raw.push_str(&line);
    }
    raw.push_str("--b--\r\n");
    raw.into_bytes()
}

/// Run alone, so other tests add nothing to the process's peak:
/// `cargo test -p mailrs-sync --lib -- --ignored sending_a_large_message --nocapture`
///
/// Compares what the envelope, the Message-ID and the Bcc-less copy cost
/// through the full parse sending used before and through the header reader.
#[test]
#[ignore = "a measurement of the whole process, run alone"]
fn sending_a_large_message_reads_only_its_headers() {
    let raw = big_message();
    let size = raw.len();

    let mark = ProcessMark::start();
    let parts = mailrs_mime::parts(&raw).unwrap();
    let before = mark.peak();
    assert!(parts.header("From").is_some());
    drop(parts);

    let mark = ProcessMark::start();
    let head = mailrs_mime::head::head(&raw);
    let envelope = head.envelope();
    let copy = mailrs_mime::head::without_header(&raw, "Bcc").into_owned();
    let after = mark.peak();
    eprintln!("a {size}-byte message: full parse peak {before} bytes; header reader and Bcc-less copy peak {after} bytes");
    assert_eq!(envelope.recipients, ["ana@example.org", "cy@example.org"]);
    assert_eq!(copy.len(), size - "Bcc: cy@example.org\r\n".len());
    // The copy is the one allocation of the message's size.
    assert!(after < size + (1 << 20), "the header reader held {after} bytes");
    assert!(before > size, "the full parse held {before} bytes");
}
