//! Which receipts the person already answered.

mod common;

use common::{db, meta, store};
use mailrs_store::receipts::{self, Answered};

#[test]
fn an_answer_is_kept_once_per_message() {
    let (conn, id) = db();
    store(&conn, &[meta(id, "a", "t1", 100, &["INBOX"])]);
    assert_eq!(receipts::answered(&conn, id, "a").unwrap(), None);
    assert!(receipts::record(&conn, id, "a", Answered::Declined, 5).unwrap(), "the first answer is kept");
    assert!(!receipts::record(&conn, id, "a", Answered::Sent, 6).unwrap(), "a second is not");
    assert_eq!(receipts::answered(&conn, id, "a").unwrap(), Some(Answered::Declined), "the first answer stays");
}

#[test]
fn a_withdrawn_answer_leaves_the_question_open() {
    let (conn, id) = db();
    store(&conn, &[meta(id, "a", "t1", 100, &["INBOX"])]);
    receipts::record(&conn, id, "a", Answered::Sent, 5).unwrap();
    receipts::withdraw(&conn, id, "a").unwrap();
    assert_eq!(receipts::answered(&conn, id, "a").unwrap(), None);
    assert!(receipts::record(&conn, id, "a", Answered::Sent, 6).unwrap());
}

#[test]
fn removing_the_account_takes_its_answers() {
    let (conn, id) = db();
    receipts::record(&conn, id, "a", Answered::Sent, 1).unwrap();
    mailrs_store::accounts::delete_account(&conn, id).unwrap();
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 0);
}
