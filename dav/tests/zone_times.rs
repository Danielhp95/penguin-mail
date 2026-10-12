//! Clock times with a TZID: in the hours the clocks repeat or skip, and in
//! a zone nothing here reads. These pin the computer's zone, so they hold
//! whatever zone the test machine runs in.

use chrono::{DateTime, NaiveDate, TimeZone, Timelike};
use chrono_tz::America::New_York;
use chrono_tz::Europe::Lisbon;
use chrono_tz::Tz;
use mailrs_dav::ical::{read_resource, write_event};
use mailrs_domain::EpochMillis;
use mailrs_domain::calendar::{Event, expand, local};

const NOW: i64 = 1_790_000_000_000;

fn at(zone: Tz, m: u32, d: u32, h: u32, min: u32) -> EpochMillis {
    let naive = NaiveDate::from_ymd_opt(2026, m, d).unwrap().and_hms_opt(h, min, 0).unwrap();
    zone.from_local_datetime(&naive).earliest().unwrap().timestamp_millis()
}

fn utc(m: u32, d: u32, h: u32, min: u32) -> EpochMillis {
    at(Tz::UTC, m, d, h, min)
}

/// One event, weekly three times, with `zones` (VTIMEZONE blocks) before it.
fn resource(zones: &str, start: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\n{zones}BEGIN:VEVENT\r\nUID:yoga@example.com\r\n\
         DTSTAMP:20261001T000000Z\r\n{start}\r\nDURATION:PT1H\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\n\
         SUMMARY:Yoga\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

/// A zone whose offsets (+05:17 and +06:17) no IANA zone has.
const CUSTOM: &str = "BEGIN:VTIMEZONE\r\nTZID:Custom Zone\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\n\
    TZOFFSETFROM:+0617\r\nTZOFFSETTO:+0517\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:19700329T020000\r\n\
    TZOFFSETFROM:+0517\r\nTZOFFSETTO:+0617\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\n";

fn read(text: &str) -> Event {
    read_resource(text, "/cal/home/", "/cal/home/yoga.ics", "e1", &[]).unwrap().events.remove(0)
}

fn hours(zone: Tz, event: &Event) -> Vec<u32> {
    expand(event, at(zone, 10, 1, 0, 0), at(zone, 12, 1, 0, 0))
        .iter()
        .map(|(start, _)| DateTime::from_timestamp_millis(*start).unwrap().with_timezone(&zone).hour())
        .collect()
}

#[test]
fn a_zoned_time_the_clocks_repeat_takes_the_first() {
    let event = read(&resource("", "DTSTART;TZID=Europe/Lisbon:20261025T013000"));
    assert_eq!(event.start, utc(10, 25, 0, 30));
}

#[test]
fn a_zoned_time_the_clocks_skip_moves_forward_by_the_gap() {
    let event = read(&resource("", "DTSTART;TZID=Europe/Lisbon:20260329T013000"));
    assert_eq!(event.start, utc(3, 29, 1, 30));
}

#[test]
fn a_zone_nobody_knows_reads_in_the_computers_zone_and_keeps_its_name() {
    let event = local::pinned(New_York, || read(&resource("", "DTSTART;TZID=Mars/Olympus_Mons:20261021T090000")));
    assert_eq!(event.start, at(New_York, 10, 21, 9, 0));
    assert_eq!(event.zone, "Mars/Olympus_Mons");
    assert_eq!(local::pinned(New_York, || hours(New_York, &event)), vec![9, 9, 9]);
}

#[test]
fn a_windows_zone_with_no_vtimezone_reads_in_its_own_zone() {
    let event = local::pinned(New_York, || read(&resource("", "DTSTART;TZID=GMT Standard Time:20261021T090000")));
    assert_eq!(event.start, at(Lisbon, 10, 21, 9, 0));
    assert_eq!(event.zone, "Europe/London");
}

#[test]
fn a_vtimezone_nobody_matches_reads_in_the_computers_zone() {
    let event = local::pinned(New_York, || read(&resource(CUSTOM, "DTSTART;TZID=Custom Zone:20261021T090000")));
    assert_eq!(event.start, at(New_York, 10, 21, 9, 0));
    assert_eq!(event.zone, "Custom Zone");
}

#[test]
fn moving_an_event_in_a_zone_nobody_knows_keeps_its_tzid_and_vtimezone() {
    let text = resource(CUSTOM, "DTSTART;TZID=Custom Zone:20261021T090000");
    let written = local::pinned(New_York, || {
        let mut event = read(&text);
        event.start += 3_600_000;
        event.end += 3_600_000;
        write_event(Some(&text), &event, &[], NOW).unwrap()
    });
    // calcard quotes a parameter value with a space; the text is the same.
    assert!(written.contains("DTSTART;TZID=\"Custom Zone\":20261021T100000\r\n"), "{written}");
    assert!(written.contains("TZOFFSETTO:+0517\r\n"), "{written}");
    let again = local::pinned(New_York, || read(&written));
    assert_eq!((again.zone.as_str(), again.start), ("Custom Zone", at(New_York, 10, 21, 10, 0)));
    assert_eq!(written.matches("BEGIN:VTIMEZONE").count(), 1, "{written}");
}

#[test]
fn a_tzid_that_needs_quotes_goes_back_quoted() {
    let text = resource("", "DTSTART;TZID=\"Office: HQ\":20261021T090000");
    let written = local::pinned(New_York, || {
        let mut event = read(&text);
        assert_eq!(event.zone, "Office: HQ");
        event.start += 3_600_000;
        event.end += 3_600_000;
        write_event(Some(&text), &event, &[], NOW).unwrap()
    });
    assert!(written.contains("DTSTART;TZID=\"Office: HQ\":20261021T100000\r\n"), "{written}");
}
