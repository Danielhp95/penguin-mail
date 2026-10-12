//! Which receipts the person already answered.

mod common;

use common::{db, meta, store};
use mailrs_store::receipts::{self, Answered};

#[test]
fn an_answer_is_kept_once_per_message() {
    let (conn, id) = db();
    store(&conn, &[meta(id, "a", "t1", 100, &["INBOX"])]);
    assert_eq!(receipts::answered(&conn, id, "a").unwrap(), None);
    receipts::record(&conn, id, "a", Answered::Declined, 5).unwrap();
    receipts::record(&conn, id, "a", Answered::Sent, 6).unwrap();
    assert_eq!(receipts::answered(&conn, id, "a").unwrap(), Some(Answered::Sent), "the later answer wins");
}

#[test]
fn removing_the_account_takes_its_answers() {
    let (conn, id) = db();
    receipts::record(&conn, id, "a", Answered::Sent, 1).unwrap();
    mailrs_store::accounts::delete_account(&conn, id).unwrap();
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 0);
}
