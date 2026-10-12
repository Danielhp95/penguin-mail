//! The swap from IMAP ids to JMAP ids, in one transaction.

use mailrs_domain::{Filter, FilterAction, FilterCriteria, MailSet, Provider};
use mailrs_store::jmap::{JmapAuth, JmapSaved};
use mailrs_store::switch::{Adoption, Moved, adopt_jmap, stored_mailboxes, stored_messages};
use mailrs_store::{accounts, local_rules, open_in_memory};
use rusqlite::Connection;

const INBOX: &str = "INBOX";
const TRIPS: &str = "Trips";

fn saved() -> JmapSaved {
    JmapSaved {
        session_url: "https://api.fastmail.com/jmap/session".into(),
        auth: JmapAuth::Bearer,
        user_name: None,
        labels: true,
        capabilities: vec!["urn:ietf:params:jmap:mail".into()],
        oauth: None,
    }
}

/// An IMAP account with two mailboxes and three messages: m1 and m2 in
/// thread t1, m3 alone in t3. Its ids are IMAP-style locations.
fn imap_account(conn: &Connection) -> i64 {
    let id = accounts::insert_imap_account(conn, "me@fastmail.com", "Fastmail", 0).unwrap().unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO mailboxes (account_id, id, name, role, kind) VALUES ({id}, '{INBOX}', 'Inbox', 'inbox', 'system');
         INSERT INTO mailboxes (account_id, id, name, role, kind) VALUES ({id}, '{TRIPS}', 'Trips', NULL, 'folder');
         INSERT INTO messages (account_id, id, thread_id, rfc822_msgid, to_addrs, cc_addrs, subject, date, snippet, size, has_attachments, sync_gen, seen)
             VALUES ({id}, 'INBOX/7/1', 't1', 'a@x', '[]', '[]', 'Plans', 1000, '', 10, 0, 1, 0),
                    ({id}, 'INBOX/7/2', 't1', 'b@x', '[]', '[]', 'Re: Plans', 2000, '', 10, 0, 1, 1),
                    ({id}, 'Trips/9/1', 't3', 'c@x', '[]', '[]', 'Rome', 3000, '', 10, 0, 1, 1);
         INSERT INTO message_mailboxes (account_id, message_id, mailbox)
             SELECT {id}, 'INBOX/7/1', key FROM mailboxes WHERE account_id = {id} AND id = '{INBOX}';
         INSERT INTO message_mailboxes (account_id, message_id, mailbox)
             SELECT {id}, 'Trips/9/1', key FROM mailboxes WHERE account_id = {id} AND id = '{TRIPS}';
         INSERT INTO message_keywords (account_id, message_id, keyword, local) VALUES ({id}, 'INBOX/7/2', '$flagged', 0);
         INSERT INTO flags (account_id, message_id, color) VALUES ({id}, 'INBOX/7/2', 'red');
         INSERT INTO drafts (account_id, message_id, draft_id) VALUES ({id}, 'Trips/9/1', 'Trips/9/1');
         INSERT INTO reminders (account_id, thread_id, subject, remind_at) VALUES ({id}, 't1', 'Plans', 99);
         INSERT INTO account_servers (account_id, role, host, port, security, user_name)
             VALUES ({id}, 'imap', 'imap.fastmail.com', 993, 'tls', 'me@fastmail.com');
         INSERT INTO remote_refs (account_id, message_id, remote, mailbox, uidvalidity, uid) VALUES ({id}, 'INBOX/7/1', 'INBOX/7/1', 'INBOX', 7, 1);"
    ))
    .unwrap();
    id
}

fn adoption() -> Adoption {
    Adoption {
        // m1 and m2 are on the server; m3 is not.
        messages: vec![
            Moved { from: "INBOX/7/1".into(), to: "M1".into(), thread: "T1".into() },
            Moved { from: "INBOX/7/2".into(), to: "M2".into(), thread: "T1".into() },
        ],
        mailboxes: vec![(INBOX.into(), "mb-inbox".into()), (TRIPS.into(), "mb-trips".into())],
        saved: saved(),
        contacts_from_jmap: false,
    }
}

fn ids(conn: &Connection, account: i64) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT id FROM messages WHERE account_id = ?1 ORDER BY id").unwrap();
    stmt.query_map([account], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
}

fn adopt(conn: &mut Connection, account: i64, adoption: &Adoption) -> mailrs_store::switch::Adopted {
    let tx = conn.transaction().unwrap();
    let adopted = adopt_jmap(&tx, account, adoption).unwrap().expect("an IMAP account");
    tx.commit().unwrap();
    adopted
}

#[test]
fn matched_messages_take_their_jmap_ids_and_the_rest_go() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    let adopted = adopt(&mut conn, account, &adoption());
    assert_eq!((adopted.kept, adopted.dropped), (2, 1));
    assert_eq!(ids(&conn, account), ["M1", "M2"]);
    let thread: String = conn.query_row("SELECT thread_id FROM messages WHERE id = 'M2'", [], |r| r.get(0)).unwrap();
    assert_eq!(thread, "T1");
    let refs: i64 = conn.query_row("SELECT COUNT(*) FROM remote_refs WHERE account_id = ?1", [account], |r| r.get(0)).unwrap();
    assert_eq!(refs, 0);
}

#[test]
fn the_row_becomes_a_jmap_account_and_keeps_its_address() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    adopt(&mut conn, account, &adoption());
    let row = accounts::account(&conn, account).unwrap().unwrap();
    assert_eq!(row.provider, Provider::Jmap);
    assert_eq!(row.email, "me@fastmail.com");
    assert_eq!(row.provider_name.as_deref(), Some("Fastmail"));
    assert_eq!(mailrs_store::jmap::load(&conn, account).unwrap(), Some(saved()));
    let servers: i64 = conn.query_row("SELECT COUNT(*) FROM account_servers WHERE account_id = ?1", [account], |r| r.get(0)).unwrap();
    assert_eq!(servers, 0);
    assert_eq!(accounts::sync_cursor(&conn, account).unwrap().state, None, "the first JMAP sync starts the feed");
}

#[test]
fn a_reminder_follows_its_thread_to_its_jmap_id() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    adopt(&mut conn, account, &adoption());
    let threads: Vec<String> = conn
        .prepare("SELECT thread_id FROM reminders WHERE account_id = ?1")
        .unwrap()
        .query_map([account], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(threads, ["T1"]);
}

#[test]
fn flags_and_drafts_cross_over() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    let mut with_draft = adoption();
    with_draft.messages.push(Moved { from: "Trips/9/1".into(), to: "M3".into(), thread: "T3".into() });
    adopt(&mut conn, account, &with_draft);
    let color: String = conn.query_row("SELECT color FROM flags WHERE account_id = ?1 AND message_id = 'M2'", [account], |r| r.get(0)).unwrap();
    assert_eq!(color, "red");
    let draft: String = conn.query_row("SELECT draft_id FROM drafts WHERE account_id = ?1", [account], |r| r.get(0)).unwrap();
    assert_eq!(draft, "M3", "a JMAP draft goes by its Email id");
    let flagged: i64 = conn
        .query_row("SELECT COUNT(*) FROM message_keywords WHERE account_id = ?1 AND message_id = 'M2' AND keyword = '$flagged'", [account], |r| r.get(0))
        .unwrap();
    assert_eq!(flagged, 1);
}

#[test]
fn mailboxes_take_their_jmap_ids_and_keep_their_messages() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    adopt(&mut conn, account, &adoption());
    let listed: Vec<String> = stored_mailboxes(&conn, account).unwrap().into_iter().map(|m| m.id).collect();
    assert!(listed.contains(&"mb-inbox".to_string()) && listed.contains(&"mb-trips".to_string()));
    let in_inbox: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM message_mailboxes h JOIN mailboxes m ON m.key = h.mailbox WHERE h.account_id = ?1 AND m.id = 'mb-inbox' AND h.message_id = 'M1'",
            [account],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(in_inbox, 1);
    let threads: i64 = conn.query_row("SELECT COUNT(*) FROM threads WHERE account_id = ?1 AND id = 'T1'", [account], |r| r.get(0)).unwrap();
    assert_eq!(threads, 1, "the thread list is rebuilt under the new ids");
}

#[test]
fn a_local_rule_files_into_the_same_folder_after_the_switch() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    let rule = Filter {
        id: Some("r1".into()),
        criteria: FilterCriteria { from: Some("travel@example.com".into()), ..FilterCriteria::default() },
        action: FilterAction { add: vec![MailSet::Mailbox(TRIPS.into())], ..FilterAction::default() },
        read_only: false,
        off: false,
    };
    local_rules::add(&conn, account, &rule).unwrap();
    adopt(&mut conn, account, &adoption());
    let rules = local_rules::list(&conn, account).unwrap();
    assert_eq!(rules[0].action.add, [MailSet::Mailbox("mb-trips".into())]);
}

#[test]
fn a_failed_swap_changes_nothing() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    let mut bad = adoption();
    // Two stored messages may never take one id.
    bad.messages[1].to = "M1".into();
    let tx = conn.transaction().unwrap();
    assert!(adopt_jmap(&tx, account, &bad).is_err());
    drop(tx);
    assert_eq!(ids(&conn, account), ["INBOX/7/1", "INBOX/7/2", "Trips/9/1"]);
    assert_eq!(accounts::account(&conn, account).unwrap().unwrap().provider, Provider::Imap);
}

#[test]
fn the_snapshot_lists_what_the_switch_matches() {
    let conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    let stored = stored_messages(&conn, account).unwrap();
    assert_eq!(stored.len(), 3);
    assert!(stored.iter().any(|m| m.message_id.as_deref() == Some("c@x") && m.date == 3000));
}

#[test]
fn a_scheduled_send_keeps_pointing_at_its_draft() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    conn.execute(
        "INSERT INTO outbox (account_id, draft_id, message_id, thread_id, subject, recipients, send_at) \
         VALUES (?1, 'Trips/9/1', 'Trips/9/1', 't3', 'Rome', '[]', 5)",
        [account],
    )
    .unwrap();
    let mut with_draft = adoption();
    with_draft.messages.push(Moved { from: "Trips/9/1".into(), to: "M3".into(), thread: "T3".into() });
    adopt(&mut conn, account, &with_draft);
    let (draft, message, thread): (String, String, String) = conn
        .query_row("SELECT draft_id, message_id, thread_id FROM outbox WHERE account_id = ?1", [account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    assert_eq!((draft.as_str(), message.as_str(), thread.as_str()), ("M3", "M3", "T3"));
}

fn count(conn: &Connection, sql: &str, account: i64) -> i64 {
    conn.query_row(sql, [account], |r| r.get(0)).unwrap()
}

#[test]
fn a_second_swap_finds_a_jmap_account_and_changes_nothing() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    adopt(&mut conn, account, &adoption());
    let tx = conn.transaction().unwrap();
    // The retry still carries the IMAP ids, which name nothing now.
    let again = adopt_jmap(&tx, account, &adoption()).unwrap();
    tx.commit().unwrap();
    assert_eq!(again, None);
    assert_eq!(ids(&conn, account), ["M1", "M2"]);
}

#[test]
fn a_receipt_follows_its_message_and_goes_with_a_dropped_one() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    conn.execute(
        "INSERT INTO receipts (account_id, message_id, answered, at) VALUES (?1, 'INBOX/7/1', 'sent', 1), (?1, 'Trips/9/1', 'declined', 2)",
        [account],
    )
    .unwrap();
    adopt(&mut conn, account, &adoption());
    let kept: Vec<String> = conn
        .prepare("SELECT message_id FROM receipts WHERE account_id = ?1")
        .unwrap()
        .query_map([account], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(kept, ["M1"]);
}

#[test]
fn two_reminders_on_threads_that_become_one_keep_the_earlier_time() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    conn.execute("INSERT INTO reminders (account_id, thread_id, subject, remind_at) VALUES (?1, 't3', 'Rome', 42)", [account]).unwrap();
    let mut joined = adoption();
    // The server threads Rome with Plans.
    joined.messages.push(Moved { from: "Trips/9/1".into(), to: "M3".into(), thread: "T1".into() });
    adopt(&mut conn, account, &joined);
    let (thread, at): (String, i64) =
        conn.query_row("SELECT thread_id, remind_at FROM reminders WHERE account_id = ?1", [account], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((thread.as_str(), at), ("T1", 42));
}

#[test]
fn what_the_store_kept_beside_a_dropped_message_goes_with_it() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    conn.execute_batch(&format!(
        "INSERT INTO invitations (account_id, uid, sequence, message_id, seen_at) VALUES ({account}, 'u1', 0, 'Trips/9/1', 1);
         INSERT INTO local_rules_ran (account_id, message_id, date) VALUES ({account}, 'Trips/9/1', 3000), ({account}, 'INBOX/7/1', 1000);
         INSERT INTO outbox (account_id, draft_id, subject, recipients, send_at, raw) VALUES ({account}, 'Trips/9/1', 'Rome', '[]', 5, x'00');"
    ))
    .unwrap();
    adopt(&mut conn, account, &adoption());
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM drafts WHERE account_id = ?1", account), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM invitations WHERE account_id = ?1", account), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM local_rules_ran WHERE account_id = ?1 AND message_id <> 'M1'", account), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM outbox WHERE account_id = ?1 AND draft_id IS NOT NULL", account), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM outbox WHERE account_id = ?1", account), 1, "the queued send stays");
}

#[test]
fn a_local_rule_into_a_mailbox_the_server_lacks_is_turned_off() {
    let mut conn = open_in_memory().unwrap();
    let account = imap_account(&conn);
    let into_trips = FilterAction { add: vec![MailSet::Mailbox(TRIPS.into())], ..FilterAction::default() };
    let rule = Filter {
        id: Some("r1".into()),
        criteria: FilterCriteria { from: Some("travel@example.com".into()), ..FilterCriteria::default() },
        action: into_trips.clone(),
        ..Filter::default()
    };
    local_rules::add(&conn, account, &rule).unwrap();
    let mut no_trips = adoption();
    no_trips.mailboxes.retain(|(old, _)| old != TRIPS);
    let adopted = adopt(&mut conn, account, &no_trips);
    assert_eq!(adopted.rules_off, 1);
    let rules = local_rules::list(&conn, account).unwrap();
    assert!(rules[0].off, "{rules:?}");
    assert_eq!(rules[0].action, into_trips, "the rule still says what the person wrote");
}
