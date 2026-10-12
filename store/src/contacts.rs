//! People to suggest as recipients. Two sources feed the list: the address
//! book each account reads from Google, and the addresses in stored mail.
//! A contact comes first, whatever the mail says; among people known only
//! from mail, someone you wrote to ranks above someone who only wrote to
//! you.

use std::collections::{HashMap, HashSet};

use mailrs_domain::{AccountId, Address, EpochMillis};
use rusqlite::Connection;

use crate::Result;
use crate::address_book;

/// Weight of one message you sent to someone, against one they sent you.
const SENT_WEIGHT: i64 = 5;

/// Someone found in stored mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correspondent {
    pub name: Option<String>,
    pub email: String,
    pub score: i64,
    pub last_seen: EpochMillis,
}

/// One row of the recipient suggestions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub name: Option<String>,
    pub email: String,
    /// The contact's employer, when the address book names one.
    pub organization: Option<String>,
    /// The file under the photo directory holding this person's photo.
    pub photo_file: Option<String>,
    /// The accounts whose address books hold this address, in id order.
    /// Empty for someone known only from mail. Contacts rank first.
    pub accounts: Vec<AccountId>,
    pub score: i64,
    pub last_seen: EpochMillis,
}

impl Suggestion {
    /// Whether an address book holds this address.
    pub fn known(&self) -> bool {
        !self.accounts.is_empty()
    }

    pub fn address(&self) -> Address {
        Address {
            name: self.name.clone(),
            email: self.email.clone(),
        }
    }

    /// The name to show, falling back to the address.
    pub fn display(&self) -> &str {
        self.name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(&self.email)
    }
}

/// Everyone worth suggesting, best first: the contacts in every account's
/// address book, then the correspondents mail turned up.
pub fn suggestions(conn: &Connection) -> Result<Vec<Suggestion>> {
    let mut found: HashMap<String, Suggestion> = HashMap::new();
    for contact in address_book::list(conn)? {
        for email in &contact.emails {
            let key = email.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            let held = found.entry(key).or_insert_with(|| Suggestion {
                name: contact.name.clone(),
                email: email.clone(),
                organization: contact.organization.clone(),
                photo_file: contact.photo_file.clone(),
                accounts: Vec::new(),
                score: 0,
                last_seen: 0,
            });
            if let Err(at) = held.accounts.binary_search(&contact.account_id) {
                held.accounts.insert(at, contact.account_id);
            }
        }
    }
    for person in list_correspondents(conn)? {
        let key = person.email.trim().to_lowercase();
        match found.get_mut(&key) {
            // Mail says how often this contact comes up, which orders the
            // contacts among themselves. Google's name wins over the one
            // a sender put in a header.
            Some(known) => {
                known.score += person.score;
                known.last_seen = known.last_seen.max(person.last_seen);
                if known.name.is_none() {
                    known.name = person.name;
                }
            }
            None => {
                found.insert(
                    key,
                    Suggestion {
                        name: person.name,
                        email: person.email,
                        organization: None,
                        photo_file: None,
                        accounts: Vec::new(),
                        score: person.score,
                        last_seen: person.last_seen,
                    },
                );
            }
        }
    }
    let mut all: Vec<Suggestion> = found.into_values().collect();
    all.sort_by(|a, b| {
        b.known()
            .cmp(&a.known())
            .then(b.score.cmp(&a.score))
            .then(b.last_seen.cmp(&a.last_seen))
            .then_with(|| a.email.cmp(&b.email))
    });
    Ok(all)
}

/// Every correspondent across all accounts, best first. Leaves out the
/// accounts' own addresses and automated senders.
///
/// Reading every message costs tens of milliseconds on a large store, and
/// the app reads this list again after any new mail. So the store keeps
/// the tally (`correspondents`), counted up to message row `counted_row`,
/// and this read adds the rows after it. A kept tally marked stale, after
/// a change to a row it counted, is passed over for a read of every row
/// until [`catch_up`] counts again.
pub fn list_correspondents(conn: &Connection) -> Result<Vec<Correspondent>> {
    // The mark, the kept tally and the rows after it, as of one moment.
    let _snapshot = match conn.is_autocommit() {
        true => Some(conn.unchecked_transaction()?),
        false => None,
    };
    let own: HashSet<String> = conn
        .prepare_cached("SELECT lower(email) FROM accounts")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let (stale, counted_row) = marks(conn)?;
    let mut tally = Tally::default();
    if !stale {
        let mut stmt = conn.prepare_cached("SELECT key, email, name, score, last_seen FROM correspondents")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Correspondent { email: row.get(1)?, name: row.get(2)?, score: row.get(3)?, last_seen: row.get(4)? },
            ))
        })?;
        tally.found = rows.collect::<rusqlite::Result<_>>()?;
        tally.last_row = counted_row;
    }
    tally.read(conn)?;
    Ok(tally.ranked(&own))
}

/// Brings the kept tally up to date: counts the message rows after the
/// last one it counted, or every row once it is stale. Runs inside the
/// caller's transaction; the store's writer calls it when idle. The count
/// keys on the messages' implicit rowids, which a VACUUM or a rebuilt
/// table renumbers: either must mark the tally stale first.
pub fn catch_up(conn: &Connection) -> Result<()> {
    let (stale, last) = marks(conn)?;
    if stale {
        // Counted in memory and written once: an upsert per address of
        // every message costs several times as long. The map goes when
        // this returns.
        let mut tally = Tally::default();
        tally.read(conn)?;
        conn.execute("DELETE FROM correspondents", [])?;
        let mut insert = conn.prepare_cached(
            "INSERT INTO correspondents (key, email, name, score, last_seen) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (key, person) in &tally.found {
            insert.execute(rusqlite::params![key, person.email, person.name, person.score, person.last_seen])?;
        }
        conn.prepare_cached("UPDATE contact_marks SET stale = 0, counted_row = ?1")?
            .execute([tally.last_row])?;
        return Ok(());
    }
    let mut upsert = conn.prepare_cached(
        "INSERT INTO correspondents (key, email, name, score, last_seen) VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (key) DO UPDATE SET score = score + excluded.score, \
         name = CASE WHEN excluded.last_seen >= last_seen OR name IS NULL \
             THEN coalesce(excluded.name, name) ELSE name END, \
         last_seen = CASE WHEN excluded.last_seen >= last_seen OR name IS NULL \
             THEN max(last_seen, excluded.last_seen) ELSE last_seen END",
    )?;
    let counted = each_address(conn, last, |address, weight, date| {
        if let Some((key, email, name)) = counted(address) {
            upsert.execute(rusqlite::params![key, email, name, weight, date])?;
        }
        Ok(())
    })?;
    conn.prepare_cached("UPDATE contact_marks SET stale = 0, counted_row = ?1")?
        .execute([counted.unwrap_or(last)])?;
    Ok(())
}

/// Whether the kept tally is stale, and the last message row it counted.
fn marks(conn: &Connection) -> Result<(bool, i64)> {
    Ok(conn
        .prepare_cached("SELECT stale, counted_row FROM contact_marks")?
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?)
}

/// Hands each address of the message rows after `after` to `note`, in row
/// order, the order the name kept depends on: a sent message's recipients
/// with [`SENT_WEIGHT`], any other message's sender with 1. Returns the
/// last row read, or `None` when there were none.
fn each_address(
    conn: &Connection,
    after: i64,
    mut note: impl FnMut(Address, i64, EpochMillis) -> Result<()>,
) -> Result<Option<i64>> {
    // Only sent mail needs its recipients, so only sent mail hands over
    // the JSON that holds them.
    let mut stmt = conn.prepare_cached(
        "SELECT m.rowid, m.from_name, m.from_addr, \
         CASE WHEN s.message_id IS NULL THEN NULL ELSE m.to_addrs END, \
         CASE WHEN s.message_id IS NULL THEN NULL ELSE m.cc_addrs END, m.date \
         FROM messages m LEFT JOIN message_mailboxes s \
         ON s.account_id = m.account_id AND s.message_id = m.id \
         AND s.mailbox IN (SELECT key FROM mailboxes WHERE role = 'sent') \
         WHERE m.rowid > ?1 ORDER BY m.rowid",
    )?;
    let mut rows = stmt.query([after])?;
    let mut last = None;
    while let Some(row) = rows.next()? {
        let date: EpochMillis = row.get(5)?;
        match (row.get::<_, Option<String>>(3)?, row.get::<_, Option<String>>(4)?) {
            (Some(to), Some(cc)) => {
                let to: Vec<Address> = serde_json::from_str(&to).unwrap_or_default();
                let cc: Vec<Address> = serde_json::from_str(&cc).unwrap_or_default();
                for address in to.into_iter().chain(cc) {
                    note(address, SENT_WEIGHT, date)?;
                }
            }
            _ => {
                if let Some(email) = row.get::<_, Option<String>>(2)? {
                    note(Address { name: row.get(1)?, email }, 1, date)?;
                }
            }
        }
        last = Some(row.get(0)?);
    }
    Ok(last)
}

/// The tally's key, address and name for `address`, or `None` for one not
/// worth suggesting.
fn counted(address: Address) -> Option<(String, String, Option<String>)> {
    let key = address.email.trim().to_lowercase();
    if key.is_empty() || !key.contains('@') || is_automated(&key) {
        return None;
    }
    let name = address
        .name
        .filter(|n| !n.trim().is_empty() && !n.contains('@'));
    Some((key, address.email.trim().to_string(), name))
}

/// Correspondents counted from the message rows up to `last_row`, by
/// lower-case address.
#[derive(Default)]
struct Tally {
    last_row: i64,
    found: HashMap<String, Correspondent>,
}

impl Tally {
    /// Counts the message rows after `last_row`, as [`catch_up`] does,
    /// and moves `last_row` to the last of them.
    fn read(&mut self, conn: &Connection) -> Result<()> {
        let found = &mut self.found;
        let last = each_address(conn, self.last_row, |address, weight, date| {
            let Some((key, email, name)) = counted(address) else {
                return Ok(());
            };
            let entry = found.entry(key).or_insert_with(|| Correspondent {
                name: None,
                email,
                score: 0,
                last_seen: date,
            });
            entry.score += weight;
            if date >= entry.last_seen || entry.name.is_none() {
                entry.name = name.or(entry.name.take());
                entry.last_seen = entry.last_seen.max(date);
            }
            Ok(())
        })?;
        self.last_row = last.unwrap_or(self.last_row);
        Ok(())
    }

    /// The correspondents but the accounts' own addresses, best first.
    fn ranked(self, own: &HashSet<String>) -> Vec<Correspondent> {
        let mut contacts: Vec<Correspondent> = self
            .found
            .into_iter()
            .filter(|(key, _)| !own.contains(key))
            .map(|(_, person)| person)
            .collect();
        contacts.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(b.last_seen.cmp(&a.last_seen))
                .then_with(|| a.email.cmp(&b.email))
        });
        contacts
    }
}

/// What [`list_correspondents`] answers when it reads every message.
#[cfg(test)]
fn whole_read(conn: &Connection) -> Result<Vec<Correspondent>> {
    let own: HashSet<String> = conn
        .prepare("SELECT lower(email) FROM accounts")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut tally = Tally::default();
    tally.read(conn)?;
    Ok(tally.ranked(&own))
}

/// Addresses nobody writes to: no-reply senders and notification robots.
pub fn is_automated(email: &str) -> bool {
    let local = email.split('@').next().unwrap_or(email);
    [
        "noreply",
        "no-reply",
        "no_reply",
        "donotreply",
        "do-not-reply",
        "mailer-daemon",
        "bounce",
    ]
    .iter()
    .any(|p| local.contains(p))
        || local == "notifications"
        || local == "notification"
}

#[cfg(test)]
mod tests {
    use mailrs_domain::MessageMeta;
    use mailrs_gmail::labels::{self as gmail, set_label_ids};

    use super::*;
    use crate::testing::list_gmail_roles;
    use crate::{accounts, messages, open_in_memory};

    fn message(
        account_id: mailrs_domain::AccountId,
        id: &str,
        from: (&str, &str),
        to: &[(&str, &str)],
        label: &str,
    ) -> MessageMeta {
        let mut meta = MessageMeta {
            account_id,
            id: id.into(),
            thread_id: id.into(),
            rfc822_msgid: None,
            from: Some(Address {
                name: Some(from.0.into()),
                email: from.1.into(),
            }),
            to: to
                .iter()
                .map(|(name, email)| Address {
                    name: Some((*name).into()),
                    email: (*email).into(),
                })
                .collect(),
            cc: Vec::new(),
            subject: "Hello".into(),
            date: 1_000,
            snippet: String::new(),
            size: 10,
            has_attachments: false,
            held: Default::default(),
            roles: vec![],
            list_unsubscribe: None,
            one_click: false,
        };
        set_label_ids(&mut meta, &[label.into()]);
        meta
    }

    /// Each change that can move a correspondent leaves the list what a
    /// read of every message gives, both before the kept tally catches up
    /// and after.
    #[test]
    fn the_list_kept_between_reads_matches_a_read_of_every_message() {
        use mailrs_domain::{MailboxKind, Membership, RemoteMailbox, Role};
        let conn = open_in_memory().unwrap();
        let id = accounts::insert_account(&conn, "dana@example.com", 0).unwrap();
        list_gmail_roles(&conn, id);
        let put = |meta: MessageMeta| {
            let upsert = messages::Change::Upsert { meta: Box::new(meta), generation: 1 };
            messages::apply(&conn, id, &[upsert]).unwrap();
        };
        let same = |step: &str| {
            assert_eq!(list_correspondents(&conn).unwrap(), whole_read(&conn).unwrap(), "after {step}");
            catch_up(&conn).unwrap();
            assert_eq!(list_correspondents(&conn).unwrap(), whole_read(&conn).unwrap(), "caught up after {step}");
        };
        let dated = |mut meta: MessageMeta, date: i64| {
            meta.date = date;
            meta
        };
        put(message(id, "m1", ("Theo Lang", "theo@example.org"), &[("Dana", "dana@example.com")], gmail::INBOX));
        put(dated(message(id, "m2", ("Dana", "dana@example.com"), &[("Mara", "mara@example.org")], gmail::SENT), 2_000));
        same("the first read");
        put(dated(message(id, "m3", ("Theo L.", "theo@example.org"), &[("Dana", "dana@example.com")], gmail::INBOX), 3_000));
        same("new mail");
        same("nothing new");
        put(dated(message(id, "m4", ("Dana", "dana@example.com"), &[("Ana", "ana@example.org")], gmail::SENT), 500));
        same("new sent mail");
        put(dated(message(id, "m4", ("Dana", "dana@example.com"), &[("Ana", "ana@example.org")], gmail::SENT), 500));
        same("sent mail read again");
        messages::apply(&conn, id, &[messages::Change::Erase { message_id: "m2".into() }]).unwrap();
        same("a deletion");
        let into_sent = messages::Change::of("m1", Membership::Mailbox(gmail::SENT.into()), true);
        messages::apply(&conn, id, &[into_sent]).unwrap();
        same("a message filed in Sent");
        let out_of_sent = messages::Change::of("m1", Membership::Mailbox(gmail::SENT.into()), false);
        messages::apply(&conn, id, &[out_of_sent]).unwrap();
        same("a message taken out of Sent");
        put(message(id, "m6", ("Dana", "dana@example.com"), &[("Cy", "cy@example.org")], gmail::INBOX));
        same("mail to Cy in the Inbox");
        // Switching an account to JMAP moves filings in place.
        conn.execute(
            "UPDATE message_mailboxes SET mailbox = (SELECT key FROM mailboxes WHERE account_id = ?1 AND id = 'SENT') \
             WHERE account_id = ?1 AND message_id = 'm6'",
            [id],
        )
        .unwrap();
        same("a filing moved to Sent");
        messages::apply(&conn, id, &[messages::Change::Erase { message_id: "m1".into() }]).unwrap();
        same("a deletion outside Sent");
        put(message(id, "m5", ("Dana", "dana@example.com"), &[("Bo", "bo@example.org")], "Label_1"));
        same("mail in a plain label");
        let sent_folder = RemoteMailbox {
            id: "Label_1".into(),
            name: "Sent Items".into(),
            kind: MailboxKind::Label,
            role: Some(Role::Sent),
            color: None,
            hidden: false,
        };
        crate::mailboxes::upsert(&conn, id, &sent_folder).unwrap();
        same("a label taking the Sent role");
        crate::mailboxes::delete(&conn, id, "Label_1").unwrap();
        same("a Sent label deleted");
        put(dated(message(id, "m3", ("Theodore", "theo@example.org"), &[("Dana", "dana@example.com")], gmail::INBOX), 4_000));
        same("a message read again with new headers");
        accounts::insert_account(&conn, "theo@example.org", 0).unwrap();
        same("an account for a correspondent's address");
        let other = accounts::insert_account(&conn, "eve@example.net", 0).unwrap();
        list_gmail_roles(&conn, other);
        let upsert = messages::Change::Upsert {
            meta: Box::new(message(other, "o1", ("Kai", "kai@example.org"), &[("Eve", "eve@example.net")], gmail::INBOX)),
            generation: 1,
        };
        messages::apply(&conn, other, &[upsert]).unwrap();
        same("mail in a second account");
        accounts::delete_account(&conn, other).unwrap();
        same("an account removed");
    }

    /// Mail filed in Sent as it arrives, and Sent mail read again as it
    /// was, leave the kept tally standing: the next read counts the new
    /// rows alone.
    #[test]
    fn sent_mail_new_or_read_again_keeps_the_tally() {
        let conn = open_in_memory().unwrap();
        let id = accounts::insert_account(&conn, "dana@example.com", 0).unwrap();
        list_gmail_roles(&conn, id);
        let sent = message(id, "m1", ("Dana", "dana@example.com"), &[("Mara", "mara@example.org")], gmail::SENT);
        let put = |meta: &MessageMeta| {
            let upsert = messages::Change::Upsert { meta: Box::new(meta.clone()), generation: 1 };
            messages::apply(&conn, id, &[upsert]).unwrap();
        };
        put(&sent);
        catch_up(&conn).unwrap();
        put(&sent);
        put(&message(id, "m2", ("Dana", "dana@example.com"), &[("Bo", "bo@example.org")], gmail::SENT));
        let stale: bool = conn.query_row("SELECT stale FROM contact_marks", [], |row| row.get(0)).unwrap();
        assert!(!stale);
    }

    #[test]
    fn contacts_come_before_addresses_seen_only_in_mail() {
        let conn = open_in_memory().unwrap();
        let id = accounts::insert_account(&conn, "dana@example.com", 0).unwrap();
        list_gmail_roles(&conn, id);
        // Ten messages from Theo, none from Mara, who is a contact.
        for n in 0..10 {
            messages::apply(
                &conn,
                id,
                &[messages::Change::Upsert {
                    meta: Box::new(message(
                        id,
                        &format!("m{n}"),
                        ("Theo Lang", "theo@example.org"),
                        &[("Dana", "dana@example.com")],
                        gmail::INBOX,
                    )),
                    generation: 1,
                }],
            )
            .unwrap();
        }
        address_book::save(
            &conn,
            &[address_book::Contact {
                account_id: id,
                resource: "people/c1".into(),
                name: Some("Mara Okafor".into()),
                emails: vec!["mara@example.org".into()],
                organization: Some("Fernwood".into()),
                photo_file: Some("1-c1.jpg".into()),
                ..address_book::Contact::default()
            }],
        )
        .unwrap();

        let all = suggestions(&conn).unwrap();
        let emails: Vec<&str> = all.iter().map(|s| s.email.as_str()).collect();
        assert_eq!(emails, ["mara@example.org", "theo@example.org"]);
        assert!(all[0].known());
        assert_eq!(all[0].organization.as_deref(), Some("Fernwood"));
        assert_eq!(all[0].photo_file.as_deref(), Some("1-c1.jpg"));
        assert!(!all[1].known());
        assert_eq!(all[1].score, 10);
    }

    #[test]
    fn a_suggestion_names_each_account_whose_address_book_holds_it() {
        let conn = open_in_memory().unwrap();
        let home = accounts::insert_account(&conn, "dana@example.com", 0).unwrap();
        let work = accounts::insert_account(&conn, "dana@work.example", 0).unwrap();
        list_gmail_roles(&conn, home);
        messages::apply(
            &conn,
            home,
            &[messages::Change::Upsert {
                meta: Box::new(message(
                    home,
                    "m1",
                    ("Theo Lang", "theo@example.org"),
                    &[("Dana", "dana@example.com")],
                    gmail::INBOX,
                )),
                generation: 1,
            }],
        )
        .unwrap();
        let contact = |account_id, resource: &str, email: &str| address_book::Contact {
            account_id,
            resource: resource.into(),
            name: Some("Mara Okafor".into()),
            emails: vec![email.into()],
            ..address_book::Contact::default()
        };
        address_book::save(
            &conn,
            &[
                contact(work, "people/w1", "Mara@Example.org"),
                contact(home, "people/h1", "mara@example.org"),
                contact(work, "people/w2", "ana@work.example"),
            ],
        )
        .unwrap();

        let all = suggestions(&conn).unwrap();
        let accounts_of = |email: &str| -> Vec<mailrs_domain::AccountId> {
            all.iter().find(|s| s.email.eq_ignore_ascii_case(email)).unwrap().accounts.clone()
        };
        assert_eq!(accounts_of("mara@example.org"), [home, work]);
        assert_eq!(accounts_of("ana@work.example"), [work]);
        assert!(accounts_of("theo@example.org").is_empty(), "Theo is known only from mail");
        assert!(!all.iter().find(|s| s.email == "theo@example.org").unwrap().known());
    }

    #[test]
    fn mail_orders_the_contacts_and_google_names_them() {
        let conn = open_in_memory().unwrap();
        let id = accounts::insert_account(&conn, "dana@example.com", 0).unwrap();
        list_gmail_roles(&conn, id);
        messages::apply(
            &conn,
            id,
            &[messages::Change::Upsert {
                meta: Box::new(message(
                    id,
                    "m1",
                    ("Dana", "dana@example.com"),
                    &[("t", "theo@example.org")],
                    gmail::SENT,
                )),
                generation: 1,
            }],
        )
        .unwrap();
        messages::apply(
            &conn,
            id,
            &[messages::Change::Upsert {
                meta: Box::new(message(
                    id,
                    "m2",
                    ("M. O.", "mara@example.org"),
                    &[("Dana", "dana@example.com")],
                    gmail::INBOX,
                )),
                generation: 1,
            }],
        )
        .unwrap();
        let contact = |resource: &str, name: &str, email: &str| address_book::Contact {
            account_id: id,
            resource: resource.into(),
            name: Some(name.into()),
            emails: vec![email.into()],
            ..address_book::Contact::default()
        };
        address_book::save(
            &conn,
            &[
                contact("people/c1", "Mara Okafor", "mara@example.org"),
                contact("people/c2", "Theo Lang", "theo@example.org"),
            ],
        )
        .unwrap();

        let all = suggestions(&conn).unwrap();
        assert!(all.iter().all(|s| s.known()));
        // Writing to Theo weighs five times a message from Mara.
        assert_eq!(all[0].email, "theo@example.org");
        assert_eq!(all[0].name.as_deref(), Some("Theo Lang"));
        assert_eq!(all[1].name.as_deref(), Some("Mara Okafor"));
    }
}
