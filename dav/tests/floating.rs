//! A floating time (no TZID, no Z) means the same clock time wherever the
//! person is. These pin the computer's zone, so they hold whatever zone
//! the test machine runs in.

use chrono::{DateTime, NaiveDate, TimeZone, Timelike};
use chrono_tz::America::New_York;
use chrono_tz::Europe::Lisbon;
use chrono_tz::Tz;
use mailrs_dav::ical::{cancel_occurrence, read_resource, write_event};
use mailrs_domain::EpochMillis;
use mailrs_domain::calendar::{Event, expand, local};

const NOW: i64 = 1_790_000_000_000;

fn at(zone: Tz, m: u32, d: u32, h: u32) -> EpochMillis {
    let naive = NaiveDate::from_ymd_opt(2026, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap();
    zone.from_local_datetime(&naive).single().unwrap().timestamp_millis()
}

/// A weekly 09:00 swim on Wednesdays from 21 October, past the night the
/// clocks go back in Lisbon (25 October) and in New York (1 November).
fn swim(start: &str, extra: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VEVENT\r\nUID:swim@example.com\r\n\
         DTSTAMP:20261001T000000Z\r\n{start}\r\nDURATION:PT1H\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\n{extra}\
         SUMMARY:Swim\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn floating() -> String {
    swim("DTSTART:20261021T090000", "")
}

fn read(text: &str) -> Event {
    read_resource(text, "/cal/home/", "/cal/home/swim.ics", "e1", &[]).unwrap().events.remove(0)
}

fn hours(zone: Tz, event: &Event) -> Vec<u32> {
    expand(event, at(zone, 10, 1, 0), at(zone, 12, 1, 0))
        .iter()
        .map(|(start, _)| DateTime::from_timestamp_millis(*start).unwrap().with_timezone(&zone).hour())
        .collect()
}

#[test]
fn a_floating_start_reads_as_that_clock_time_here() {
    let event = local::pinned(Lisbon, || read(&floating()));
    assert_eq!(event.start, at(Lisbon, 10, 21, 9));
    assert_eq!(event.zone, "");
}

#[test]
fn a_floating_series_repeats_at_its_clock_time_across_the_change() {
    assert_eq!(local::pinned(Lisbon, || hours(Lisbon, &read(&floating()))), vec![9, 9, 9]);
    assert_eq!(local::pinned(New_York, || hours(New_York, &read(&floating()))), vec![9, 9, 9]);
}

#[test]
fn a_floating_exclusion_leaves_out_its_occurrence() {
    let text = swim("DTSTART:20261021T090000", "EXDATE:20261028T090000\r\n");
    let starts = local::pinned(Lisbon, || {
        let event = read(&text);
        expand(&event, at(Lisbon, 10, 1, 0), at(Lisbon, 12, 1, 0)).into_iter().map(|(s, _)| s).collect::<Vec<_>>()
    });
    assert_eq!(starts, vec![at(Lisbon, 10, 21, 9), at(Lisbon, 11, 4, 9)]);
}

#[test]
fn moving_a_floating_event_writes_it_back_floating() {
    let written = local::pinned(Lisbon, || {
        let mut event = read(&floating());
        event.start += 3_600_000;
        event.end += 3_600_000;
        write_event(Some(&floating()), &event, &[], NOW).unwrap()
    });
    assert!(written.contains("DTSTART:20261021T100000\r\n"), "{written}");
    assert!(written.contains("DTEND:20261021T110000\r\n"), "{written}");
}

#[test]
fn a_new_event_with_no_zone_is_written_floating() {
    let event = Event {
        id: "tea".into(),
        start: at(Lisbon, 10, 21, 16),
        end: at(Lisbon, 10, 21, 17),
        zone: String::new(),
        title: "Tea".into(),
        ..Event::default()
    };
    let written = local::pinned(Lisbon, || write_event(None, &event, &[], NOW).unwrap());
    assert!(written.contains("DTSTART:20261021T160000\r\n"), "{written}");
}

#[test]
fn cancelling_a_floating_occurrence_writes_a_floating_exdate() {
    let written = local::pinned(Lisbon, || cancel_occurrence(&floating(), at(Lisbon, 10, 28, 9), NOW).unwrap().unwrap());
    assert!(written.contains("EXDATE:20261028T090000\r\n"), "{written}");
}

#[test]
fn a_utc_start_stays_utc() {
    let event = local::pinned(New_York, || read(&swim("DTSTART:20261021T090000Z", "")));
    assert_eq!(event.zone, "UTC");
    assert_eq!(event.start, at(Tz::UTC, 10, 21, 9));
}

#[test]
fn a_zoned_start_stays_in_its_zone() {
    let event = local::pinned(New_York, || read(&swim("DTSTART;TZID=Europe/Lisbon:20261021T090000", "")));
    assert_eq!(event.zone, "Europe/Lisbon");
    assert_eq!(local::pinned(New_York, || hours(Lisbon, &event)), vec![9, 9, 9]);
}

fn once(start: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VEVENT\r\nUID:night@example.com\r\n\
         DTSTAMP:20261001T000000Z\r\n{start}\r\nDURATION:PT1H\r\nSUMMARY:Night\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

#[test]
fn a_floating_time_the_clocks_repeat_takes_the_first() {
    // 01:30 comes twice in Lisbon on 25 October 2026: first in summer time.
    let event = local::pinned(Lisbon, || read(&once("DTSTART:20261025T013000")));
    let utc = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap().and_hms_opt(0, 30, 0).unwrap().and_utc();
    assert_eq!(event.start, utc.timestamp_millis());
}

#[test]
fn a_floating_time_the_clocks_skip_moves_forward_by_the_gap() {
    // Lisbon skips 01:00 to 02:00 on 29 March 2026, so 01:30 is 02:30
    // summer time, which is 01:30 UTC (RFC 5545 section 3.3.5).
    let event = local::pinned(Lisbon, || read(&once("DTSTART:20260329T013000")));
    let utc = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap().and_hms_opt(1, 30, 0).unwrap().and_utc();
    assert_eq!(event.start, utc.timestamp_millis());
}
