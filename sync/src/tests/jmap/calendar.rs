//! A JMAP account's calendar over `FakeJmap`, through the calendar service
//! the calendar copy reads.

use mailrs_domain::calendar::Access;
use serde_json::{Map, Value, json};

use super::{Harness, over};
use crate::fake::FakeJmap;
use crate::{AnyCalendar, BackendError, CalendarService, JmapApi};

async fn calendared() -> Harness {
    over(FakeJmap::new().with_calendars()).await
}

async fn state(fake: &FakeJmap) -> String {
    fake.calendar_events(&[]).await.unwrap().state
}

fn calendar(h: &Harness) -> AnyCalendar {
    h.sync.services().calendar.clone().expect("the fake offers calendars")
}

fn rota(calendar: &str) -> Value {
    json!({
        "@type": "Event", "uid": "rota@allotment.example", "title": "Allotment rota", "calendarIds": {calendar: true},
        "start": "2026-10-06T09:00:00", "timeZone": "Europe/Lisbon", "duration": "PT1H"
    })
}

#[tokio::test]
async fn calendars_list_with_the_default_first() {
    let h = calendared().await;
    let listed = calendar(&h).calendars().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].primary);
    assert_eq!((listed[0].access, listed[0].zone.as_str(), listed[0].color.as_str()), (Access::Owner, "Europe/Lisbon", "#e66100"));
}

#[tokio::test]
async fn an_account_without_jmap_calendars_has_no_jmap_calendar() {
    let h = over(FakeJmap::new()).await;
    assert!(h.sync.services().calendar.is_none());
}

#[tokio::test]
async fn a_first_read_brings_the_events_and_ends_with_the_state() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let page = calendar(&h).event_changes("cal-1", None, None, 0).await.unwrap();
    assert_eq!(page.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), [id.as_str()]);
    assert_eq!(page.whole_series, [id]);
    assert_eq!(page.next_sync, Some(state(&h.fake).await));
    assert!(page.next_page.is_none());
}

#[tokio::test]
async fn two_calendars_share_one_changes_call_and_a_move_crosses() {
    let h = calendared().await;
    h.fake.with(|s| {
        let mut second = s.calendars[0].clone();
        second.id = "cal-2".into();
        second.is_default = false;
        s.calendars.push(second);
    });
    let id = h.fake.add_event(rota("cal-1"));
    let since = state(&h.fake).await;
    let mut patch = Map::new();
    patch.insert("calendarIds".into(), json!({"cal-2": true}));
    h.fake.set_calendar_event(&mailrs_jmap::EventWrite::Update { id: id.clone(), patch }, false).await.unwrap();
    let calendar = calendar(&h);
    let first = calendar.event_changes("cal-1", Some(&since), None, 0).await.unwrap();
    let second = calendar.event_changes("cal-2", Some(&since), None, 0).await.unwrap();
    assert_eq!(first.removed, std::slice::from_ref(&id));
    assert!(first.events.is_empty());
    assert_eq!(second.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), [id.as_str()]);
    assert_eq!(first.next_sync, second.next_sync);
    let calls = h.fake.with(|s| s.calls.iter().filter(|c| *c == "CalendarEvent/changes").count());
    assert_eq!(calls, 1, "one changes call for both calendars");
}

#[tokio::test]
async fn a_deleted_event_leaves() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let since = state(&h.fake).await;
    h.fake.set_calendar_event(&mailrs_jmap::EventWrite::Destroy(id.clone()), false).await.unwrap();
    let page = calendar(&h).event_changes("cal-1", Some(&since), None, 0).await.unwrap();
    assert!(page.removed.contains(&id));
}

#[tokio::test]
async fn a_forgotten_state_reads_the_calendar_whole_again() {
    let h = calendared().await;
    h.fake.add_event(rota("cal-1"));
    h.fake.forget_changes();
    let refused = calendar(&h).event_changes("cal-1", Some("0"), None, 0).await.unwrap_err();
    assert!(matches!(refused, BackendError::StateLost));
}

#[tokio::test]
async fn a_range_read_carries_no_token() {
    let h = calendared().await;
    h.fake.add_event(rota("cal-1"));
    let page = calendar(&h).event_range("cal-1", 0, i64::MAX / 2, None).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert!(page.next_sync.is_none());
}

#[tokio::test]
async fn the_account_offers_moves_and_quiet_changes_and_no_files() {
    let h = calendared().await;
    let offers = h.sync.services().offers();
    assert!(offers.calendar && offers.moves_events && offers.quiet_changes);
    assert!(!offers.event_files && !offers.calendar_list && !offers.subscriptions);
}
