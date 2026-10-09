//! The demo's calendar account: a Nextcloud server Dana signed in to for
//! its calendars alone, served by the in-memory CalDAV server. It has no
//! mail, so it shows in the calendar and nowhere in the mail window.
//!
//! The demo adds it only when `MAILRS_DEMO_CALENDAR_ACCOUNT` is set, so
//! the screenshots and the tour keep the accounts they were drawn with.
//! Set to `refused`, the server stops taking the password once the demo
//! is seeded, which puts Sign In Again under the account's heading.

use std::sync::Arc;

use mailrs_domain::{AccountId, EpochMillis};
use mailrs_store::services::{FoundService, ServiceKind};
use mailrs_store::{Db, StoreError, accounts};
use mailrs_sync::fake::{DavKind, FakeDav};
use mailrs_sync::{AccountServices, AccountSync, AnyCalendar, AnyDav, AnyMail, CalDav, NoMail, SyncError};

use super::{at_week, week_monday};

/// The variable that asks for the account.
const WANTED: &str = "MAILRS_DEMO_CALENDAR_ACCOUNT";
/// Its value for a server that refuses the password.
const REFUSED: &str = "refused";

/// The account's name: Dana's user on the server's host.
pub const NAME: &str = "dana@cloud.reyes.example";
const SERVER: &str = "https://cloud.reyes.example/remote.php/dav/";
const USER: &str = "dana";
/// The address invitations name her by on that server.
const ADDRESS: &str = "dana@reyes.example";
const HOUSEHOLD: &str = "/cal/household/";

/// How the demo was asked to show the account, if it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// Signed in, with its calendar.
    SignedIn,
    /// Seeded, then refused by its server at the first read.
    Refused,
}

/// What `MAILRS_DEMO_CALENDAR_ACCOUNT` asks for.
pub fn wanted() -> Option<Wanted> {
    let value = std::env::var_os(WANTED)?;
    Some(match value.to_str() {
        Some(REFUSED) => Wanted::Refused,
        _ => Wanted::SignedIn,
    })
}

/// The account's services: the calendar on its server, and no mail.
pub fn services(dav: &Arc<FakeDav>) -> AccountServices {
    let api = Arc::new(AnyDav::from(Arc::clone(dav)));
    AccountServices::calendar_only(AnyCalendar::Dav(CalDav::new(api, AnyMail::NoMail(NoMail), Vec::new())))
}

/// Adds the calendar account with a server holding one calendar and this
/// week's events on it. The caller reads them into the store with the
/// calendar copy, as it does every other account's.
pub async fn seed(
    db: &Db,
    now: EpochMillis,
) -> std::result::Result<(AccountId, Arc<FakeDav>, Arc<AccountSync>), SyncError> {
    let account_id = db
        .write(move |c| {
            // The store is new, so nothing else holds the name.
            let id = accounts::insert_caldav_account(c, NAME, now)?.ok_or(StoreError::Corrupt {
                column: "accounts.provider",
                value: NAME.to_string(),
            })?;
            mailrs_store::services::save(
                c,
                id,
                &FoundService {
                    kind: ServiceKind::CalDav,
                    url: SERVER.into(),
                    user_name: USER.into(),
                    confirmed: true,
                    source: "typed".into(),
                },
            )?;
            Ok(id)
        })
        .await?;
    let dav = Arc::new(FakeDav::new());
    dav.add_collection(HOUSEHOLD, DavKind::Calendar, "Household", Some("#26a269"));
    // Nextcloud mails a guest's answer itself.
    dav.set_auto_schedule(true);
    dav.with(|server| server.addresses = vec![ADDRESS.to_string()]);
    for (href, body) in household(now) {
        dav.put_resource(&href, &body);
    }
    // Nobody listens yet: the window reads the store once it opens.
    let (events, _) = async_channel::unbounded();
    let sync = Arc::new(AccountSync::new(account_id, services(&dav), db.clone(), events));
    Ok((account_id, dav, sync))
}

/// The household's week: a piano lesson every Tuesday, the recycling on
/// Thursday morning, and dinner on Friday, as a CalDAV server holds them.
fn household(now: EpochMillis) -> Vec<(String, String)> {
    let monday = week_monday(now);
    let stamp = |at: EpochMillis| {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at)
            .unwrap_or_default()
            .format("%Y%m%dT%H%M%SZ")
            .to_string()
    };
    let event = |uid: &str, title: &str, start: EpochMillis, minutes: i64, rule: &str| {
        (
            format!("{HOUSEHOLD}{uid}.ics"),
            format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Nextcloud//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:{}\r\n\
                 DTSTART:{}\r\nDTEND:{}\r\n{rule}SUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                stamp(now),
                stamp(start),
                stamp(start + minutes * 60_000),
            ),
        )
    };
    vec![
        event("piano", "Piano lesson", at_week(monday, 1, 16, 30), 45, "RRULE:FREQ=WEEKLY\r\n"),
        event("recycling", "Recycling goes out", at_week(monday, 3, 7, 30), 15, ""),
        event("dinner-marta", "Dinner at Marta's", at_week(monday, 4, 19, 30), 150, ""),
    ]
}

/// Makes the server stop taking the password, as one does after the
/// person changes it elsewhere.
pub fn refuse(dav: &FakeDav) {
    dav.with(|server| server.refuse_login = true);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use mailrs_domain::Provider;
    use mailrs_store::calendar as store;
    use mailrs_sync::calendar_copy::CalendarCopy;
    use mailrs_sync::now_millis;

    use super::*;

    /// The one account, for the calendar copy to read.
    struct Alone(AccountId, Arc<AccountSync>);

    impl mailrs_sync::Accounts for Alone {
        fn account(&self, account_id: AccountId) -> Option<Arc<AccountSync>> {
            (account_id == self.0).then(|| Arc::clone(&self.1))
        }
    }

    #[tokio::test]
    async fn the_calendar_account_has_a_calendar_with_this_weeks_events_and_no_mail() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("demo.db")).unwrap();
        let now = now_millis();
        let (id, dav, sync) = seed(&db, now).await.unwrap();

        let account = db.read(move |c| accounts::account(c, id)).await.unwrap().expect("the account");
        assert_eq!((account.email.as_str(), account.provider), (NAME, Provider::CalDav));
        assert!(!services(&dav).serves_mail());

        let copy = CalendarCopy::new(Arc::new(Alone(id, sync)), db.clone());
        copy.refresh(id, now).await.unwrap();
        let calendars = db.read(move |c| store::calendars(c, id)).await.unwrap();
        assert_eq!(calendars.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Household"]);
        let week = 7 * 24 * 3_600_000;
        let titles: HashMap<String, usize> = db
            .read(move |c| store::occurrences(c, &[id], now - week, now + week, store::CalendarScope::All))
            .await
            .unwrap()
            .into_iter()
            .fold(HashMap::new(), |mut count, o| {
                *count.entry(o.event.title.clone()).or_default() += 1;
                count
            });
        for title in ["Piano lesson", "Recycling goes out", "Dinner at Marta's"] {
            assert!(titles.contains_key(title), "{title} is on the calendar: {titles:?}");
        }
    }

    #[tokio::test]
    async fn a_refused_server_leaves_the_account_needing_to_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("demo.db")).unwrap();
        let now = now_millis();
        let (id, dav, sync) = seed(&db, now).await.unwrap();
        refuse(&dav);
        let copy = CalendarCopy::new(Arc::new(Alone(id, sync)), db.clone());
        // The read fails, and what it leaves behind is the point.
        let _ = copy.refresh(id, now).await;
        let state = db.read(move |c| accounts::account(c, id)).await.unwrap().expect("the account").state;
        assert_eq!(state, mailrs_domain::AccountState::NeedsReauth);
    }
}
