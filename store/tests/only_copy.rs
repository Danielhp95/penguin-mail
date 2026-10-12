//! The store is the only copy of a POP3 account's mail. Sync may not
//! delete it, whatever the reason; a person may.

mod common;

use common::{db, meta, store};
use mailrs_domain::{AccountId, RemoveSetting};
use mailrs_store::messages::{self, Change};
use mailrs_store::{accounts, local_messages, open_in_memory, threads, window};
use rusqlite::Connection;

const RAW: &[u8] = b"Subject: hi\r\n\r\nbody";

/// A POP3 account holding one old downloaded message, outside the Inbox,
/// with its bytes.
fn pop3_mail() -> (Connection, AccountId) {
    let conn = open_in_memory().unwrap();
    let id = accounts::insert_pop3_account(&conn, "dana@example.org", "example.org", RemoveSetting::Never, 0)
        .unwrap()
        .expect("a new account");
    store(&conn, &[meta(id, "pop3/u1", "t1", 100, &[])]);
    local_messages::put(&conn, id, "pop3/u1", RAW).unwrap();
    (conn, id)
}

fn kept(conn: &Connection, id: AccountId) -> bool {
    let bytes = local_messages::get(conn, id, "pop3/u1").unwrap();
    let row = messages::thread_id_of(conn, id, "pop3/u1").unwrap();
    match (bytes, row) {
        (Some(bytes), Some(_)) => bytes == RAW,
        (None, None) => false,
        other => panic!("the row and the bytes parted: {other:?}"),
    }
}

#[test]
fn pruning_the_window_keeps_a_pop3_accounts_mail() {
    let (conn, id) = pop3_mail();
    assert!(window::prune_window(&conn, id, 1_000).unwrap().is_empty());
    assert!(kept(&conn, id));
    assert!(threads::get_thread(&conn, id, "t1").unwrap().is_some());
}

#[test]
fn a_sweep_keeps_a_pop3_accounts_mail() {
    let (conn, id) = pop3_mail();
    assert!(window::sweep_stale(&conn, id, 2).unwrap().is_empty());
    assert!(kept(&conn, id));
}

#[test]
fn a_message_the_server_no_longer_has_stays_in_a_pop3_account() {
    let (conn, id) = pop3_mail();
    let gone = Change::Gone { message_id: "pop3/u1".into() };
    let touched = messages::apply(&conn, id, &[gone]).unwrap();
    assert!(touched.threads.is_empty());
    assert!(kept(&conn, id));
}

#[test]
fn a_thread_the_server_no_longer_has_stays_in_a_pop3_account() {
    let (conn, id) = pop3_mail();
    let gone = Change::ThreadGone { thread_id: "t1".into() };
    messages::apply(&conn, id, &[gone]).unwrap();
    assert!(kept(&conn, id));
    assert!(threads::get_thread(&conn, id, "t1").unwrap().is_some());
}

#[test]
fn a_person_erasing_pop3_mail_takes_its_bytes() {
    let (conn, id) = pop3_mail();
    let erase = Change::Erase { message_id: "pop3/u1".into() };
    let touched = messages::apply(&conn, id, &[erase]).unwrap();
    assert_eq!(touched.threads.into_iter().collect::<Vec<_>>(), ["t1"]);
    assert!(!kept(&conn, id));
    assert!(threads::get_thread(&conn, id, "t1").unwrap().is_none());
}

#[test]
fn removing_a_pop3_account_takes_its_mail() {
    let (conn, id) = pop3_mail();
    accounts::delete_account(&conn, id).unwrap();
    assert!(!kept(&conn, id));
}

#[test]
fn a_server_accounts_message_the_server_no_longer_has_leaves_the_store() {
    let (conn, id) = db();
    store(&conn, &[meta(id, "a", "t1", 100, &[]), meta(id, "b", "t1", 200, &[])]);
    let gone = Change::Gone { message_id: "a".into() };
    messages::apply(&conn, id, &[gone]).unwrap();
    assert_eq!(messages::thread_id_of(&conn, id, "a").unwrap(), None);
    assert_eq!(threads::get_thread(&conn, id, "t1").unwrap().unwrap().message_count, 1);
}

#[test]
fn a_server_accounts_thread_the_server_no_longer_has_leaves_the_store() {
    let (conn, id) = db();
    store(&conn, &[meta(id, "a", "t1", 100, &[])]);
    let gone = Change::ThreadGone { thread_id: "t1".into() };
    messages::apply(&conn, id, &[gone]).unwrap();
    assert!(threads::get_thread(&conn, id, "t1").unwrap().is_none());
    assert_eq!(messages::thread_id_of(&conn, id, "a").unwrap(), None);
}

#[test]
fn a_server_accounts_old_mail_leaves_with_the_window() {
    let (conn, id) = db();
    store(&conn, &[meta(id, "a", "t1", 100, &[])]);
    assert_eq!(window::prune_window(&conn, id, 1_000).unwrap(), ["t1"]);
    store(&conn, &[meta(id, "b", "t2", 100, &[])]);
    assert_eq!(window::sweep_stale(&conn, id, 2).unwrap(), ["t2"]);
    assert_eq!(messages::thread_id_of(&conn, id, "b").unwrap(), None);
}
