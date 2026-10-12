//! An account whose calendars move between CalDAV and JMAP Calendars, as
//! when its server starts or stops offering JMAP Calendars. Changes queued
//! under one server's ids must never go to the other under those ids: each
//! waits until the new server's calendars are read, then goes onto the
//! same event there, found by its UID, or stays and is named once.

use std::sync::{Arc, Mutex};

use mailrs_dav::Kind;
use mailrs_dav::fake::FakeDav;
use mailrs_domain::AccountId;
use mailrs_domain::calendar::series::Step;
use mailrs_domain::calendar::{Event, Notify};
use mailrs_store::calendar as store;
use serde_json::json;

use super::jmap::{Harness, over};
use crate::calendar_copy::{CalendarCopy, TurnedDown};
use crate::fake::FakeJmap;
use crate::services::CalDav;
use crate::{AccountServices, AccountSync, Accounts, AnyCalendar};

const WORK: &str = "/cal/work/";
const NOW: i64 = 1_790_000_000_000;

/// One account whose services a test swaps, as a reconnect does.
struct Switching {
    sync: Arc<AccountSync>,
    services: Mutex<AccountServices>,
}

impl Accounts for Switching {
    fn account(&self, _: AccountId) -> Option<Arc<AccountSync>> {
        Some(Arc::clone(&self.sync))
    }

    fn services(&self, _: AccountId) -> Option<AccountServices> {
        Some(self.services.lock().unwrap().clone())
    }
}

struct World {
    h: Harness,
    dav: Arc<FakeDav>,
    accounts: Arc<Switching>,
    copy: CalendarCopy<Switching>,
}

fn ics(uid: &str, title: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTART:20261006T080000Z\r\nDTEND:20261006T090000Z\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn jscal(uid: &str, title: &str) -> serde_json::Value {
    json!({
        "@type": "Event", "uid": uid, "title": title, "calendarIds": {"cal-1": true},
        "start": "2026-10-06T09:00:00", "timeZone": "Europe/Lisbon", "duration": "PT1H"
    })
}

/// A server holding the same two events over CalDAV and over JMAP, except
/// that "Seed swap" is only on CalDAV and "Compost day" only on JMAP.
async fn world() -> World {
    let h = over(FakeJmap::new().with_calendars()).await;
    h.fake.add_event(jscal("rota@allotment.example", "Allotment rota"));
    h.fake.add_event(jscal("compost@allotment.example", "Compost day"));
    let dav = Arc::new(FakeDav::new());
    dav.add_collection(WORK, Kind::Calendar, "Work", Some("#3a87ad"));
    dav.put_resource(&format!("{WORK}rota.ics"), &ics("rota@allotment.example", "Allotment rota"));
    dav.put_resource(&format!("{WORK}swap.ics"), &ics("swap@allotment.example", "Seed swap"));
    let services = h.sync.services().clone();
    let accounts = Arc::new(Switching { sync: Arc::clone(&h.sync), services: Mutex::new(services) });
    let copy = CalendarCopy::new(Arc::clone(&accounts), h.db.clone());
    World { h, dav, accounts, copy }
}

impl World {
    fn caldav(&self) -> AccountServices {
        let jmap = self.h.sync.services().clone();
        let dav = Arc::new(crate::AnyDav::from(Arc::clone(&self.dav)));
        let mail = jmap.mail.clone();
        AccountServices { calendar: None, ..jmap }.with_calendar(AnyCalendar::Dav(CalDav::new(dav, mail, vec!["me@example.com".into()])))
    }

    fn jmap(&self) -> AccountServices {
        self.h.sync.services().clone()
    }

    fn serve(&self, services: AccountServices) {
        *self.accounts.services.lock().unwrap() = services;
    }

    async fn stored(&self) -> Vec<Event> {
        let account = self.h.account_id;
        let calendars = self.h.db.read(move |c| store::calendars(c, account)).await.unwrap();
        let mut events = Vec::new();
        for calendar in calendars {
            let id = calendar.id.clone();
            let found = self
                .h
                .db
                .read(move |c| {
                    let ids: Vec<String> = {
                        let mut stmt = c.prepare("SELECT id FROM events WHERE account_id = ?1 AND calendar = ?2").unwrap();
                        stmt.query_map(rusqlite::params![account, id], |row| row.get(0)).unwrap().map(Result::unwrap).collect()
                    };
                    let mut out = Vec::new();
                    for event in ids {
                        out.extend(store::event(c, account, &id, &event)?);
                    }
                    Ok(out)
                })
                .await
                .unwrap();
            events.extend(found);
        }
        events
    }

    async fn titled(&self, title: &str) -> Event {
        self.stored().await.into_iter().find(|e| e.title == title).unwrap_or_else(|| panic!("no {title}"))
    }

    async fn queued(&self) -> Vec<store::QueuedChange> {
        let account = self.h.account_id;
        self.h.db.read(move |c| store::queued(c, account)).await.unwrap()
    }

    /// Renames the event titled `from` and deletes the one titled `gone`,
    /// queued and unsent.
    async fn edit_offline(&self, from: &str, to: &str, gone: &str) {
        let mut renamed = self.titled(from).await;
        renamed.title = to.into();
        let deleted = self.titled(gone).await;
        let steps = vec![Step::Save(renamed), Step::Remove { calendar: deleted.calendar.clone(), id: deleted.id.clone() }];
        self.copy.apply_with(self.h.account_id, steps, Notify::Nobody).await.unwrap();
    }
}

fn not_saved(said: &[TurnedDown]) -> Vec<&str> {
    said.iter().filter(|t| t.reason.is_some()).map(|t| t.title.as_str()).collect()
}

#[tokio::test]
async fn changes_queued_over_caldav_wait_for_jmap_then_go_onto_the_same_events() {
    let w = world().await;
    w.serve(w.caldav());
    w.copy.refresh(w.h.account_id, NOW).await.unwrap();
    w.edit_offline("Allotment rota", "Rota, by the shed", "Seed swap").await;

    w.serve(w.jmap());
    let writes = || w.h.fake.with(|s| s.calls.iter().filter(|c| *c == "CalendarEvent/set").count());
    let said = w.copy.send(w.h.account_id).await.unwrap();
    assert!(said.is_empty(), "{said:?}");
    assert_eq!(writes(), 0, "nothing goes to JMAP under CalDAV's ids");

    w.copy.refresh(w.h.account_id, NOW).await.unwrap();
    let said = w.copy.send(w.h.account_id).await.unwrap();
    let rota = w.h.fake.with(|s| s.events.values().find(|e| e["uid"] == "rota@allotment.example").cloned()).unwrap();
    assert_eq!(rota["title"], "Rota, by the shed");
    assert_eq!(w.h.fake.with(|s| s.events.len()), 2, "no event made or deleted");
    assert_eq!(not_saved(&said), ["Seed swap"], "the delete JMAP has no event for is named");
    assert_eq!(w.copy.send(w.h.account_id).await.unwrap(), [], "and named once");
    assert_eq!(w.queued().await.len(), 1, "the delete stays queued");
}

#[tokio::test]
async fn changes_queued_over_jmap_wait_for_caldav_then_go_onto_the_same_events() {
    let w = world().await;
    w.serve(w.jmap());
    w.copy.refresh(w.h.account_id, NOW).await.unwrap();
    w.edit_offline("Allotment rota", "Rota, by the shed", "Compost day").await;

    w.serve(w.caldav());
    let puts = w.dav.puts();
    let said = w.copy.send(w.h.account_id).await.unwrap();
    assert!(said.is_empty(), "{said:?}");
    assert_eq!(w.dav.puts(), puts, "nothing goes to CalDAV under JMAP's ids");

    w.copy.refresh(w.h.account_id, NOW).await.unwrap();
    let said = w.copy.send(w.h.account_id).await.unwrap();
    assert!(w.dav.body(&format!("{WORK}rota.ics")).unwrap().contains("by the shed"));
    assert!(w.dav.body(&format!("{WORK}swap.ics")).is_some(), "the other event stays");
    assert_eq!(not_saved(&said), ["Compost day"]);
    assert_eq!(w.copy.send(w.h.account_id).await.unwrap(), []);
}

#[tokio::test]
async fn a_change_kept_after_a_switch_goes_out_once_the_old_server_is_back() {
    let w = world().await;
    w.serve(w.caldav());
    w.copy.refresh(w.h.account_id, NOW).await.unwrap();
    w.edit_offline("Allotment rota", "Rota, by the shed", "Seed swap").await;
    w.serve(w.jmap());
    w.copy.send(w.h.account_id).await.unwrap();
    w.copy.refresh(w.h.account_id, NOW).await.unwrap();
    w.copy.send(w.h.account_id).await.unwrap();

    w.serve(w.caldav());
    w.copy.send(w.h.account_id).await.unwrap();
    assert!(w.dav.body(&format!("{WORK}swap.ics")).is_none(), "the kept delete reached CalDAV");
    assert!(w.queued().await.is_empty());
}

#[tokio::test]
async fn an_answer_queued_over_caldav_reaches_the_jmap_event() {
    let w = world().await;
    let mut invite = jscal("plan@allotment.example", "Planting plan");
    invite["participants"] = json!({
        "p1": {"@type": "Participant", "calendarAddress": "mailto:ines@allotment.example", "roles": {"owner": true, "attendee": true}, "participationStatus": "accepted"},
        "p2": {"@type": "Participant", "calendarAddress": "mailto:me@example.com", "roles": {"attendee": true}, "participationStatus": "needs-action"}
    });
    let id = w.h.fake.add_event(invite);
    let ics = ics("plan@allotment.example", "Planting plan").replace(
        "END:VEVENT",
        "ORGANIZER:mailto:ines@allotment.example\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT",
    );
    w.dav.put_resource(&format!("{WORK}plan.ics"), &ics);
    w.serve(w.caldav());
    w.copy.refresh(w.h.account_id, NOW).await.unwrap();
    let plan = w.titled("Planting plan").await;
    let answer = store::QueuedAnswer {
        me: "me@example.com".into(),
        answer: mailrs_domain::invitation::Answer::Yes,
        note: None,
        title: plan.title.clone(),
    };
    let account = w.h.account_id;
    w.h.db.write(move |c| store::enqueue_answer(c, account, &plan.calendar, &plan.id, &answer)).await.unwrap();

    w.serve(w.jmap());
    w.copy.send(account).await.unwrap();
    w.copy.refresh(account, NOW).await.unwrap();
    w.copy.send(account).await.unwrap();
    assert_eq!(w.h.fake.event(&id).unwrap()["participants"]["p2"]["participationStatus"], "accepted");
    assert!(w.queued().await.is_empty());
}
