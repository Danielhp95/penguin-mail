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
async fn a_calendar_s_default_alerts_become_its_reminders() {
    use mailrs_domain::calendar::{Reminder, ReminderMethod};
    let h = calendared().await;
    h.fake.with(|s| {
        let alert = |offset: &str, action: &str| json!({"@type": "Alert", "trigger": {"@type": "OffsetTrigger", "offset": offset}, "action": action});
        s.calendars[0].default_alerts_with_time = serde_json::from_value(json!({"k1": alert("-PT10M", "display")})).ok();
        s.calendars[0].default_alerts_without_time = serde_json::from_value(json!({"k2": alert("-PT9H", "email"), "k3": alert("PT9H", "display")})).ok();
    });
    let listed = calendar(&h).calendars().await.unwrap();
    assert_eq!(listed[0].reminders, [Reminder { minutes: 10, method: ReminderMethod::Notification }]);
    assert_eq!(listed[0].all_day_reminders, Some(vec![Reminder { minutes: 540, method: ReminderMethod::Email }]), "an alert after the start has no reminder");
}

#[tokio::test]
async fn a_calendar_without_default_alerts_reminds_of_nothing() {
    let h = calendared().await;
    let listed = calendar(&h).calendars().await.unwrap();
    assert!(listed[0].reminders.is_empty());
    assert_eq!(listed[0].all_day_reminders, Some(Vec::new()));
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
    h.fake.set_calendar_event(&mailrs_jmap::EventWrite::Update { id: id.clone(), patch }, false, None).await.unwrap();
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
    h.fake.set_calendar_event(&mailrs_jmap::EventWrite::Destroy(id.clone()), false, None).await.unwrap();
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

use mailrs_domain::calendar::{Event, Notify, occurrence_id};
use mailrs_domain::invitation::Answer;

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;

async fn first_event(h: &Harness) -> Event {
    calendar(h).event_changes("cal-1", None, None, 0).await.unwrap().events.remove(0)
}

/// Changes event `id` the way another app would, straight on the server.
async fn elsewhere(h: &Harness, id: &str, key: &str, value: Value) {
    let mut patch = Map::new();
    patch.insert(key.into(), value);
    h.fake.set_calendar_event(&mailrs_jmap::EventWrite::Update { id: id.into(), patch }, false, None).await.unwrap();
}

fn invited() -> Value {
    json!({
        "@type": "Event", "uid": "plan@allotment.example", "title": "Planting plan", "calendarIds": {"cal-1": true},
        "start": "2026-10-08T18:00:00", "timeZone": "Europe/Lisbon", "duration": "PT1H",
        "participants": {
            "p1": {"@type": "Participant", "calendarAddress": "mailto:ines@allotment.example", "roles": {"owner": true, "attendee": true}, "participationStatus": "accepted"},
            "p2": {"@type": "Participant", "calendarAddress": "mailto:me@example.com", "roles": {"attendee": true}, "participationStatus": "needs-action"}
        }
    })
}

#[tokio::test]
async fn a_new_event_goes_up_as_jscalendar_and_comes_back_under_the_servers_id() {
    let h = calendared().await;
    let new = Event {
        calendar: "cal-1".into(),
        uid: "fair@example.com".into(),
        title: "Seed fair".into(),
        start: 1_791_000_000_000,
        end: 1_791_003_600_000,
        zone: "Europe/Lisbon".into(),
        busy: true,
        ..Event::default()
    };
    let made = calendar(&h).put_event(&new, None, true, Notify::Nobody).await.unwrap();
    assert!(made.id.starts_with("ev-"));
    assert_eq!((made.title.as_str(), made.start, made.end), ("Seed fair", new.start, new.end));
    let stored = h.fake.event(&made.id).unwrap();
    assert_eq!(stored["calendarIds"]["cal-1"], true);
    assert_eq!(stored["@type"], "Event");
    assert!(h.fake.with(|s| s.scheduled.is_empty()), "Nobody means nobody is mailed");
}

#[tokio::test]
async fn a_change_patches_and_a_later_change_goes_from_the_new_version() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let mut event = first_event(&h).await;
    let held = event.etag.clone();
    event.title = "Rota, moved".into();
    let changed = calendar(&h).put_event(&event, Some(&held), false, Notify::Guests).await.unwrap();
    assert_eq!(changed.title, "Rota, moved");
    assert_eq!(h.fake.event(&id).unwrap()["title"], "Rota, moved");
    let mut again = changed.clone();
    again.title = "Rota, moved twice".into();
    let twice = calendar(&h).put_event(&again, Some(&changed.etag), false, Notify::Guests).await.unwrap();
    assert_eq!((twice.title.as_str(), twice.kept_theirs.len()), ("Rota, moved twice", 0));
}

#[tokio::test]
async fn a_title_changed_elsewhere_and_a_time_moved_here_both_stay() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let mut event = first_event(&h).await;
    let held = event.etag.clone();
    elsewhere(&h, &id, "title", json!("Rota, by the shed")).await;
    event.start += HOUR;
    event.end += HOUR;
    let saved = calendar(&h).put_event(&event, Some(&held), false, Notify::Nobody).await.unwrap();
    let stored = h.fake.event(&id).unwrap();
    assert_eq!((stored["title"].as_str(), stored["start"].as_str()), (Some("Rota, by the shed"), Some("2026-10-06T10:00:00")));
    assert_eq!((saved.title.as_str(), saved.start), ("Rota, by the shed", event.start));
    assert!(saved.kept_theirs.is_empty());
}

#[tokio::test]
async fn a_time_moved_on_both_sides_keeps_the_servers_and_says_so() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let mut event = first_event(&h).await;
    let held = event.etag.clone();
    elsewhere(&h, &id, "start", json!("2026-10-06T11:00:00")).await;
    event.start += HOUR;
    event.end += HOUR;
    event.title = "Rota, with tea".into();
    let saved = calendar(&h).put_event(&event, Some(&held), false, Notify::Nobody).await.unwrap();
    let stored = h.fake.event(&id).unwrap();
    assert_eq!((stored["title"].as_str(), stored["start"].as_str()), (Some("Rota, with tea"), Some("2026-10-06T11:00:00")));
    assert_eq!(saved.kept_theirs, ["the time"]);
}

#[tokio::test]
async fn an_etag_without_a_fingerprint_is_refused_once_the_event_changed() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let mut event = first_event(&h).await;
    let bare = event.etag.split('|').next().unwrap().to_string();
    event.title = "Rota, moved".into();
    let saved = calendar(&h).put_event(&event, Some(&bare), false, Notify::Nobody).await.unwrap();
    assert_eq!(saved.title, "Rota, moved", "unchanged since, so nothing is lost");
    elsewhere(&h, &id, "title", json!("Elsewhere")).await;
    event.title = "Rota, moved again".into();
    let refused = calendar(&h).put_event(&event, Some(&bare), false, Notify::Nobody).await.unwrap_err();
    assert!(matches!(refused, BackendError::Changed));
    assert_eq!(h.fake.event(&id).unwrap()["title"], "Elsewhere");
}

#[tokio::test]
async fn a_write_whose_state_moved_meanwhile_is_tried_once_more() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let mut event = first_event(&h).await;
    let held = event.etag.clone();
    event.title = "Rota, retried".into();
    h.fake.with(|s| s.state_mismatches = 1);
    let mut saved = calendar(&h).put_event(&event, Some(&held), false, Notify::Nobody).await.unwrap();
    assert_eq!(h.fake.event(&id).unwrap()["title"], "Rota, retried");
    let held = saved.etag.clone();
    saved.title = "Rota, given up".into();
    h.fake.with(|s| s.state_mismatches = 2);
    let refused = calendar(&h).put_event(&saved, Some(&held), false, Notify::Nobody).await.unwrap_err();
    assert!(matches!(refused, BackendError::Changed), "a second mismatch is not retried");
    assert_eq!(h.fake.event(&id).unwrap()["title"], "Rota, retried");
}

#[tokio::test]
async fn a_changed_occurrence_patches_its_override() {
    let h = calendared().await;
    let mut weekly = rota("cal-1");
    weekly["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "weekly"});
    let id = h.fake.add_event(weekly);
    let series = first_event(&h).await;
    let original = series.start + 7 * DAY;
    let mut one = series.clone();
    one.series = Some(series.id.clone());
    one.original_start = Some(original);
    one.id = occurrence_id(&series, original);
    one.etag = String::new();
    one.rules.clear();
    one.start = original + HOUR;
    one.end = one.start + HOUR;
    one.title = "Rota, an hour late".into();
    let back = calendar(&h).put_event(&one, None, false, Notify::Nobody).await.unwrap();
    assert_eq!((back.id.as_str(), back.title.as_str()), (one.id.as_str(), "Rota, an hour late"));
    assert_eq!(h.fake.event(&id).unwrap()["recurrenceOverrides"]["2026-10-13T09:00:00"]["start"], "2026-10-13T10:00:00");
    // The override exists now; a second change goes into it.
    let mut later = back.clone();
    later.title = "Rota, later still".into();
    calendar(&h).put_event(&later, Some(&back.etag), false, Notify::Nobody).await.unwrap();
    let over = &h.fake.event(&id).unwrap()["recurrenceOverrides"]["2026-10-13T09:00:00"];
    assert_eq!((over["title"].as_str(), over["start"].as_str()), (Some("Rota, later still"), Some("2026-10-13T10:00:00")));
}

#[tokio::test]
async fn deleting_one_occurrence_excludes_it() {
    let h = calendared().await;
    let mut weekly = rota("cal-1");
    weekly["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "weekly"});
    let id = h.fake.add_event(weekly);
    let series = first_event(&h).await;
    let occurrence = occurrence_id(&series, series.start + 7 * DAY);
    calendar(&h).remove_event("cal-1", &occurrence, None, Notify::Nobody).await.unwrap();
    assert_eq!(h.fake.event(&id).unwrap()["recurrenceOverrides"]["2026-10-13T09:00:00"]["excluded"], true);
    calendar(&h).remove_event("cal-1", &id, None, Notify::Nobody).await.unwrap();
    assert!(h.fake.event(&id).is_none());
}

#[tokio::test]
async fn an_occurrence_of_a_repeat_the_app_cannot_read_is_not_deleted() {
    let h = calendared().await;
    let mut odd = rota("cal-1");
    odd["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "monthly", "rscale": "hebrew"});
    let id = h.fake.add_event(odd);
    let series = first_event(&h).await;
    let occurrence = occurrence_id(&series, series.start + 30 * DAY);
    let refused = calendar(&h).remove_event("cal-1", &occurrence, None, Notify::Nobody).await.unwrap_err();
    let BackendError::Refused(words) = refused else { panic!("expected a refusal in words, got {refused:?}") };
    assert!(words.starts_with("Penguin Mail cannot read how this event repeats"), "{words}");
    assert!(h.fake.event(&id).unwrap().get("recurrenceOverrides").is_none());
}

#[tokio::test]
async fn deleting_an_event_changed_elsewhere_is_refused() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let event = first_event(&h).await;
    elsewhere(&h, &id, "title", json!("Elsewhere")).await;
    let refused = calendar(&h).remove_event("cal-1", &id, Some(&event.etag), Notify::Nobody).await.unwrap_err();
    assert!(matches!(refused, BackendError::Changed));
    assert!(h.fake.event(&id).is_some());
}

#[tokio::test]
async fn a_move_changes_the_calendar_ids() {
    let h = calendared().await;
    h.fake.with(|s| {
        let mut second = s.calendars[0].clone();
        second.id = "cal-2".into();
        s.calendars.push(second);
    });
    let id = h.fake.add_event(rota("cal-1"));
    let event = first_event(&h).await;
    let moved = calendar(&h).move_event(&event, "cal-2", Notify::Nobody).await.unwrap();
    assert_eq!(moved.calendar, "cal-2");
    assert_eq!(h.fake.event(&id).unwrap()["calendarIds"], json!({"cal-2": true}));
}

#[tokio::test]
async fn an_import_finds_its_uid_and_changes_that_event() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let mut file = first_event(&h).await;
    file.id = String::new();
    file.title = "Rota from a file".into();
    let imported = calendar(&h).import_event(&file).await.unwrap();
    assert_eq!(imported.id, id, "the same UID updates, never doubles");
    assert_eq!(h.fake.with(|s| s.events.len()), 1);
    assert_eq!(h.fake.event(&id).unwrap()["title"], "Rota from a file");
}

#[tokio::test]
async fn an_answer_saves_my_status_and_the_server_mails_it() {
    let h = calendared().await;
    let id = h.fake.add_event(invited());
    let answered = calendar(&h).answer_event("cal-1", &id, "me@example.com", Answer::Yes, None).await.unwrap();
    assert_eq!(answered.my_answer, Some(Answer::Yes));
    assert_eq!(h.fake.event(&id).unwrap()["participants"]["p2"]["participationStatus"], "accepted");
    assert_eq!(h.fake.with(|s| s.scheduled.clone()), [id]);
    assert!(h.fake.with(|s| s.sent.is_empty()), "the server mails the REPLY, not the app");
}

#[tokio::test]
async fn an_answer_the_server_cannot_schedule_goes_by_mail() {
    let h = calendared().await;
    h.fake.with(|s| s.schedules = false);
    let id = h.fake.add_event(invited());
    calendar(&h).answer_event("cal-1", &id, "me@example.com", Answer::No, None).await.unwrap();
    assert_eq!(h.fake.event(&id).unwrap()["participants"]["p2"]["participationStatus"], "declined");
    let sent = h.fake.with(|s| s.sent.clone());
    assert_eq!(sent.len(), 1, "one REPLY");
    assert_eq!(sent[0].rcpt_to, ["ines@allotment.example"]);
    assert!(String::from_utf8_lossy(&sent[0].raw).contains("REPLY"));
}

#[tokio::test]
async fn an_answer_to_one_occurrence_goes_into_its_override() {
    let h = calendared().await;
    let mut weekly = invited();
    weekly["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "weekly"});
    let id = h.fake.add_event(weekly);
    let series = first_event(&h).await;
    let occurrence = occurrence_id(&series, series.start + 7 * DAY);
    let answered = calendar(&h).answer_event("cal-1", &occurrence, "me@example.com", Answer::Maybe, None).await.unwrap();
    assert_eq!((answered.id.as_str(), answered.my_answer), (occurrence.as_str(), Some(Answer::Maybe)));
    let stored = h.fake.event(&id).unwrap();
    assert_eq!(stored["recurrenceOverrides"]["2026-10-15T18:00:00"]["participants/p2/participationStatus"], "tentative");
    assert_eq!(stored["participants"]["p2"]["participationStatus"], "needs-action", "the series keeps its answer");
}

#[tokio::test]
async fn an_event_i_am_not_invited_to_cannot_be_answered() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    assert!(matches!(
        calendar(&h).answer_event("cal-1", &id, "me@example.com", Answer::Yes, None).await,
        Err(BackendError::Refused(_))
    ));
}

#[tokio::test]
async fn files_and_list_edits_are_refused_in_words() {
    let h = calendared().await;
    let calendar = calendar(&h);
    assert!(matches!(calendar.share_file("f", "a@x.example").await, Err(BackendError::Refused(_))));
    let edit = mailrs_domain::calendar::list::ListEdit::Rename { name: "New".into() };
    assert!(matches!(calendar.edit_list("cal-1", &edit).await, Err(BackendError::Refused(_))));
}

#[tokio::test]
async fn a_save_that_kept_the_servers_time_says_so_through_the_copy() {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::calendar_copy::CalendarCopy;
    use crate::tests::Connected;

    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let copy = CalendarCopy::new(Arc::new(Connected(HashMap::from([(h.account_id, Arc::clone(&h.sync))]))), h.db.clone());
    copy.refresh(h.account_id, 1_790_000_000_000).await.unwrap();
    let (account, key) = (h.account_id, id.clone());
    let mut event = h.db.read(move |c| mailrs_store::calendar::event(c, account, "cal-1", &key)).await.unwrap().unwrap();
    elsewhere(&h, &id, "start", json!("2026-10-06T11:00:00")).await;
    event.start += HOUR;
    event.end += HOUR;
    event.title = "Rota, with tea".into();
    copy.save(h.account_id, event).await.unwrap();
    let said = copy.send(h.account_id).await.unwrap();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!((said[0].title.as_str(), said[0].kept_theirs.as_slice(), said[0].reason.as_deref()), ("Rota, with tea", ["the time".to_string()].as_slice(), None));
    let (account, key) = (h.account_id, id.clone());
    let stored = h.db.read(move |c| mailrs_store::calendar::event(c, account, "cal-1", &key)).await.unwrap().unwrap();
    assert_eq!((stored.title.as_str(), stored.pending), ("Rota, with tea", false));
    assert_eq!(h.fake.event(&id).unwrap()["start"], "2026-10-06T11:00:00");
}

#[tokio::test]
async fn an_occurrence_deleted_elsewhere_does_not_come_back() {
    let h = calendared().await;
    let mut weekly = rota("cal-1");
    weekly["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "weekly"});
    weekly["recurrenceOverrides"] = json!({"2026-10-13T09:00:00": {"title": "Rota, swapped"}});
    let id = h.fake.add_event(weekly);
    let page = calendar(&h).event_changes("cal-1", None, None, 0).await.unwrap();
    let mut one = page.events.into_iter().find(|e| e.series.is_some()).unwrap();
    let held = one.etag.clone();
    elsewhere(&h, &id, "recurrenceOverrides/2026-10-13T09:00:00", json!({"excluded": true})).await;
    one.title = "Rota, swapped back".into();
    let refused = calendar(&h).put_event(&one, Some(&held), false, Notify::Nobody).await.unwrap_err();
    assert!(matches!(refused, BackendError::NotFound), "{refused:?}");
    assert_eq!(h.fake.event(&id).unwrap()["recurrenceOverrides"]["2026-10-13T09:00:00"], json!({"excluded": true}));
}

/// The one-occurrence change the editor makes of `series` at `original`,
/// through the domain's own step.
fn renamed_occurrence(series: &Event, original: i64, title: &str) -> Event {
    use mailrs_domain::calendar::series::{Picked, RepeatScope, Step, change};
    let picked = Picked { original_start: original, start: original };
    let edited = Event { title: title.into(), start: original, end: original + (series.end - series.start), ..series.clone() };
    let steps = change(series, &[], picked, edited, RepeatScope::This, "pmnew");
    let [Step::Save(one)] = steps.as_slice() else { panic!("{steps:?}") };
    one.clone()
}

fn weekly() -> Value {
    let mut weekly = rota("cal-1");
    weekly["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "weekly"});
    weekly
}

#[tokio::test]
async fn an_occurrence_renamed_here_follows_its_series_moved_elsewhere() {
    let h = calendared().await;
    let id = h.fake.add_event(weekly());
    let series = first_event(&h).await;
    let one = renamed_occurrence(&series, series.start + 7 * DAY, "Rota, renamed");
    elsewhere(&h, &id, "start", json!("2026-10-06T10:00:00")).await;
    let saved = calendar(&h).put_event(&one, None, false, Notify::Nobody).await.unwrap();
    let overrides = h.fake.event(&id).unwrap()["recurrenceOverrides"].clone();
    assert_eq!(overrides, json!({"2026-10-13T10:00:00": {"title": "Rota, renamed"}}), "no second event at 09:00");
    assert_eq!((saved.title.as_str(), saved.start), ("Rota, renamed", series.start + 7 * DAY + HOUR));
    assert!(saved.kept_theirs.is_empty());
}

#[tokio::test]
async fn an_occurrence_of_a_series_moved_to_another_day_is_refused() {
    let h = calendared().await;
    let id = h.fake.add_event(weekly());
    let series = first_event(&h).await;
    let one = renamed_occurrence(&series, series.start + 7 * DAY, "Rota, renamed");
    elsewhere(&h, &id, "start", json!("2026-10-07T09:00:00")).await;
    let refused = calendar(&h).put_event(&one, None, false, Notify::Nobody).await.unwrap_err();
    assert!(matches!(refused, BackendError::Changed), "{refused:?}");
    let no_series_etag = Event { series_etag: String::new(), ..one };
    let refused = calendar(&h).put_event(&no_series_etag, None, false, Notify::Nobody).await.unwrap_err();
    assert!(matches!(refused, BackendError::Changed), "{refused:?}");
    assert!(h.fake.event(&id).unwrap().get("recurrenceOverrides").is_none());
}

#[tokio::test]
async fn an_occurrence_change_carries_only_what_the_person_changed() {
    let h = calendared().await;
    let id = h.fake.add_event(weekly());
    let series = first_event(&h).await;
    let one = renamed_occurrence(&series, series.start + 7 * DAY, "Rota, renamed");
    elsewhere(&h, &id, "locations", json!({"l1": {"@type": "Location", "name": "The shed"}})).await;
    calendar(&h).put_event(&one, None, false, Notify::Nobody).await.unwrap();
    let overrides = h.fake.event(&id).unwrap()["recurrenceOverrides"].clone();
    assert_eq!(overrides, json!({"2026-10-13T09:00:00": {"title": "Rota, renamed"}}), "the stale empty place stays out");
}

#[tokio::test]
async fn deleting_an_occurrence_of_a_moved_series_is_refused() {
    let h = calendared().await;
    let id = h.fake.add_event(weekly());
    let series = first_event(&h).await;
    let occurrence = occurrence_id(&series, series.start + 7 * DAY);
    elsewhere(&h, &id, "start", json!("2026-10-06T10:00:00")).await;
    let refused = calendar(&h).remove_event("cal-1", &occurrence, None, Notify::Nobody).await.unwrap_err();
    assert!(matches!(refused, BackendError::Changed), "{refused:?}");
    assert!(h.fake.event(&id).unwrap().get("recurrenceOverrides").is_none());
}

#[tokio::test]
async fn a_guest_added_here_keeps_a_name_changed_elsewhere() {
    let h = calendared().await;
    let mut mine = rota("cal-1");
    mine["participants"] = json!({
        "p1": {"@type": "Participant", "calendarAddress": "mailto:me@example.com", "roles": {"owner": true, "attendee": true}, "participationStatus": "accepted"},
        "p2": {"@type": "Participant", "name": "Inês", "calendarAddress": "mailto:ines@allotment.example", "roles": {"attendee": true}}
    });
    let id = h.fake.add_event(mine);
    let mut event = first_event(&h).await;
    let held = event.etag.clone();
    elsewhere(&h, &id, "participants/p2/name", json!("Inês Costa")).await;
    event.guests.push(mailrs_domain::calendar::Guest { email: "bea@allotment.example".into(), name: Some("Bea".into()), ..Default::default() });
    let saved = calendar(&h).put_event(&event, Some(&held), false, Notify::Nobody).await.unwrap();
    let stored = h.fake.event(&id).unwrap();
    assert_eq!(stored["participants"]["p2"]["name"], "Inês Costa");
    let addresses: Vec<&str> = stored["participants"].as_object().unwrap().values().filter_map(|p| p["calendarAddress"].as_str()).collect();
    assert!(addresses.contains(&"mailto:bea@allotment.example"), "{addresses:?}");
    assert!(saved.kept_theirs.is_empty());
}

#[tokio::test]
async fn a_delete_reads_the_parts_when_the_server_keeps_no_stamp() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let calm = h.fake.add_event(json!({"@type": "Event", "uid": "calm@example.com", "title": "Calm", "calendarIds": {"cal-1": true}, "start": "2026-10-07T09:00:00", "duration": "PT1H"}));
    h.fake.with(|s| {
        for event in s.events.values_mut() {
            event.as_object_mut().unwrap().remove("updated");
        }
    });
    let page = calendar(&h).event_changes("cal-1", None, None, 0).await.unwrap();
    let etag = |id: &str| page.events.iter().find(|e| e.id == id).unwrap().etag.clone();
    h.fake.with(|s| s.events.get_mut(&id).unwrap()["title"] = json!("Elsewhere"));
    let refused = calendar(&h).remove_event("cal-1", &id, Some(&etag(&id)), Notify::Nobody).await.unwrap_err();
    assert!(matches!(refused, BackendError::Changed), "{refused:?}");
    assert!(h.fake.event(&id).is_some());
    calendar(&h).remove_event("cal-1", &calm, Some(&etag(&calm)), Notify::Nobody).await.unwrap();
    assert!(h.fake.event(&calm).is_none());
}

fn invited_weekly(me: &str) -> Value {
    let mut weekly = invited();
    weekly["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "weekly"});
    let mine = weekly["participants"]["p2"].take();
    weekly["participants"].as_object_mut().unwrap().remove("p2");
    weekly["participants"][me] = mine;
    weekly
}

#[tokio::test]
async fn an_answer_escapes_a_participant_id_with_a_slash() {
    let h = calendared().await;
    let mut weekly = invited_weekly("p/2");
    weekly["recurrenceOverrides"] = json!({"2026-10-15T18:00:00": {"title": "Planting plan, outside"}});
    let id = h.fake.add_event(weekly);
    let series = first_event(&h).await;
    let occurrence = occurrence_id(&series, series.start + 7 * DAY);
    calendar(&h).answer_event("cal-1", &occurrence, "me@example.com", Answer::Maybe, None).await.unwrap();
    let over = &h.fake.event(&id).unwrap()["recurrenceOverrides"]["2026-10-15T18:00:00"];
    assert_eq!(over["participants/p~12/participationStatus"], "tentative", "{over}");
}

#[tokio::test]
async fn an_answer_goes_inside_a_participant_the_override_holds_whole() {
    let h = calendared().await;
    let mut weekly = invited_weekly("p2");
    weekly["recurrenceOverrides"] = json!({"2026-10-15T18:00:00": {
        "participants/p2": {"@type": "Participant", "calendarAddress": "mailto:me@example.com", "roles": {"attendee": true}, "participationStatus": "needs-action"}
    }});
    let id = h.fake.add_event(weekly);
    let series = first_event(&h).await;
    let occurrence = occurrence_id(&series, series.start + 7 * DAY);
    calendar(&h).answer_event("cal-1", &occurrence, "me@example.com", Answer::Yes, None).await.unwrap();
    let over = h.fake.event(&id).unwrap()["recurrenceOverrides"]["2026-10-15T18:00:00"].clone();
    assert_eq!(over["participants/p2"]["participationStatus"], "accepted", "{over}");
    assert!(over.get("participants/p2/participationStatus").is_none(), "no key inside another key: {over}");
}

#[tokio::test]
async fn an_imported_occurrence_becomes_an_override_of_its_series() {
    let h = calendared().await;
    let id = h.fake.add_event(weekly());
    let series = first_event(&h).await;
    let original = series.start + 7 * DAY;
    let file = Event {
        calendar: "cal-1".into(),
        uid: series.uid.clone(),
        title: "Rota, from a file".into(),
        start: original + HOUR,
        end: original + 2 * HOUR,
        zone: "Europe/Lisbon".into(),
        busy: true,
        original_start: Some(original),
        ..Event::default()
    };
    let imported = calendar(&h).import_event(&file).await.unwrap();
    let stored = h.fake.event(&id).unwrap();
    assert_eq!(stored["recurrenceRule"]["frequency"], "weekly", "the series stays a series");
    assert_eq!(stored["title"], "Allotment rota");
    let over = &stored["recurrenceOverrides"]["2026-10-13T09:00:00"];
    assert_eq!((over["title"].as_str(), over["start"].as_str()), (Some("Rota, from a file"), Some("2026-10-13T10:00:00")));
    assert_eq!(imported.id, occurrence_id(&series, original));
}

#[tokio::test]
async fn a_title_edit_while_the_notes_changed_elsewhere_keeps_both_quietly() {
    let h = calendared().await;
    let id = h.fake.add_event(rota("cal-1"));
    let mut event = first_event(&h).await;
    let held = event.etag.clone();
    elsewhere(&h, &id, "description", json!("Bring gloves")).await;
    event.title = "Rota, with tea".into();
    let saved = calendar(&h).put_event(&event, Some(&held), false, Notify::Nobody).await.unwrap();
    let stored = h.fake.event(&id).unwrap();
    assert_eq!((stored["title"].as_str(), stored["description"].as_str()), (Some("Rota, with tea"), Some("Bring gloves")));
    assert!(saved.kept_theirs.is_empty());
}

#[tokio::test]
async fn what_the_editor_tidies_does_not_count_as_the_person_s_change() {
    let h = calendared().await;
    let id = h.fake.add_event(json!({
        "@type": "Event", "uid": "fair@allotment.example", "title": "Seed fair ", "calendarIds": {"cal-1": true},
        "start": "2026-10-10T00:00:00", "showWithoutTime": true, "duration": "P1D", "timeZone": "Europe/Lisbon"
    }));
    let mut event = first_event(&h).await;
    let held = event.etag.clone();
    elsewhere(&h, &id, "start", json!("2026-10-11T00:00:00")).await;
    elsewhere(&h, &id, "title", json!("Seed fair, moved")).await;
    // The editor trims the title and gives an all-day event the zone UTC.
    event.title = event.title.trim().to_string();
    event.zone = "UTC".into();
    event.busy = false;
    let saved = calendar(&h).put_event(&event, Some(&held), false, Notify::Nobody).await.unwrap();
    let stored = h.fake.event(&id).unwrap();
    assert_eq!((stored["title"].as_str(), stored["start"].as_str()), (Some("Seed fair, moved"), Some("2026-10-11T00:00:00")));
    assert_eq!(stored["freeBusyStatus"], "free");
    assert!(saved.kept_theirs.is_empty(), "{:?}", saved.kept_theirs);
}

#[tokio::test]
async fn an_occurrence_moved_here_while_its_series_moved_elsewhere_keeps_the_servers_time() {
    let h = calendared().await;
    let id = h.fake.add_event(weekly());
    let series = first_event(&h).await;
    let original = series.start + 7 * DAY;
    let mut one = renamed_occurrence(&series, original, &series.title);
    one.start = original + 2 * HOUR;
    one.end = one.start + HOUR;
    elsewhere(&h, &id, "start", json!("2026-10-06T10:00:00")).await;
    let saved = calendar(&h).put_event(&one, None, false, Notify::Nobody).await.unwrap();
    assert_eq!((saved.start, saved.kept_theirs.as_slice()), (original + HOUR, ["the time".to_string()].as_slice()));
    assert!(h.fake.event(&id).unwrap().get("recurrenceOverrides").is_none(), "nothing to write, so no empty override");
}

/// A new event made on this computer, with a guest, as the editor queues
/// it: a client-made id and no version yet.
fn new_meeting() -> Event {
    Event {
        calendar: "cal-1".into(),
        id: crate::calendar_copy::new_event_id(),
        title: "Seed swap".into(),
        start: 1_791_000_000_000,
        end: 1_791_003_600_000,
        zone: "Europe/Lisbon".into(),
        busy: true,
        organizer: Some("me@example.com".into()),
        guests: vec![mailrs_domain::calendar::Guest { email: "ines@allotment.example".into(), ..Default::default() }],
        ..Event::default()
    }
}

/// The account's calendar copy, with the calendar list read.
async fn copy_of(h: &Harness) -> crate::calendar_copy::CalendarCopy<crate::tests::Connected> {
    use std::collections::HashMap;
    use std::sync::Arc;
    let copy = crate::calendar_copy::CalendarCopy::new(Arc::new(crate::tests::Connected(HashMap::from([(h.account_id, Arc::clone(&h.sync))]))), h.db.clone());
    copy.refresh(h.account_id, 1_790_000_000_000).await.unwrap();
    copy
}

#[tokio::test]
async fn a_create_whose_answer_was_lost_makes_one_event_and_one_invitation() {
    let h = calendared().await;
    let copy = copy_of(&h).await;
    copy.apply(h.account_id, vec![mailrs_domain::calendar::series::Step::Save(new_meeting())]).await.unwrap();
    h.fake.with(|s| s.lose_event_answer = true);
    assert!(copy.send(h.account_id).await.is_err(), "a lost answer keeps the change queued");
    copy.send(h.account_id).await.unwrap();
    let (events, scheduled) = h.fake.with(|s| (s.events.len(), s.scheduled.len()));
    assert_eq!((events, scheduled), (1, 1));
    let account = h.account_id;
    assert!(h.db.read(move |c| mailrs_store::calendar::queued(c, account)).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_create_whose_read_back_failed_makes_one_event_and_one_invitation() {
    let h = calendared().await;
    let copy = copy_of(&h).await;
    copy.apply(h.account_id, vec![mailrs_domain::calendar::series::Step::Save(new_meeting())]).await.unwrap();
    h.fake.with(|s| s.failing_event_gets = 1);
    assert!(copy.send(h.account_id).await.is_err(), "a failed read keeps the change queued");
    copy.send(h.account_id).await.unwrap();
    let (events, scheduled) = h.fake.with(|s| (s.events.len(), s.scheduled.len()));
    assert_eq!((events, scheduled), (1, 1));
}

#[tokio::test]
async fn a_new_event_carries_its_uid_in_the_copy_and_the_queue() {
    let h = calendared().await;
    let copy = copy_of(&h).await;
    let new = new_meeting();
    let (account, id) = (h.account_id, new.id.clone());
    copy.apply(account, vec![mailrs_domain::calendar::series::Step::Save(new)]).await.unwrap();
    let key = id.clone();
    let stored = h.db.read(move |c| mailrs_store::calendar::event(c, account, "cal-1", &key)).await.unwrap().unwrap();
    let queued = h.db.read(move |c| mailrs_store::calendar::queued(c, account)).await.unwrap();
    assert_eq!(stored.uid, format!("{id}@penguin-mail"));
    assert_eq!(queued[0].body.as_ref().map(|b| b.uid.as_str()), Some(stored.uid.as_str()));
}

#[tokio::test]
async fn a_create_queued_without_a_uid_still_goes_up_with_one() {
    let h = calendared().await;
    let new = new_meeting();
    h.fake.with(|s| s.lose_event_answer = true);
    calendar(&h).put_event(&new, None, true, Notify::Guests).await.unwrap_err();
    let made = calendar(&h).put_event(&new, None, true, Notify::Guests).await.unwrap();
    assert_eq!(made.uid, format!("{}@penguin-mail", new.id));
    assert_eq!(h.fake.with(|s| (s.events.len(), s.scheduled.len())), (1, 1));
}
