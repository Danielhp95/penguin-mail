//! JSCalendar as the calendar copy's events, both ways. JSCalendar 2.0
//! (the revision draft-ietf-jmap-calendars-32 cites) is what gets written:
//! one `recurrenceRule`, and a participant's `calendarAddress`. RFC 8984's
//! `recurrenceRules` and `sendTo` are read too, for servers that still
//! send them. A series' changed occurrences become events of their own
//! under `occurrence_id`, and its cancelled ones `EXDATE` lines, which is
//! how the CalDAV adapter hands an iCalendar series to the copy.
//!
//! A change goes back as a patch of what the person changed and nothing
//! else: the server's event holds more than the copy shows (rooms, roles,
//! alerts after the start, a second location, HTML, a rule in another
//! calendar scale), and writing a property whole would lose the rest.

use chrono::{DateTime, Days, NaiveDate, NaiveDateTime, Utc};
use chrono_tz::Tz;
use mailrs_domain::EpochMillis;
use mailrs_domain::calendar::local::instant;
use mailrs_domain::calendar::{self as model, Guest, Reminder, ReminderMethod, Status, occurrence_id};
use mailrs_domain::invitation::Answer;
use mailrs_domain::translate::gettext;
use mailrs_jmap::pointer;
use serde_json::{Map, Value, json};

use crate::BackendError;

const DAY: i64 = 86_400_000;
/// The most changed or cancelled occurrences read from one object, so a
/// hostile series cannot make the copy build an unbounded list.
const MOST_OVERRIDES: usize = 1000;
/// The most alerts, participants and patch entries read from one object.
const MOST_LISTED: usize = 500;
/// The longest duration read. A longer one, for an event or an alert, is
/// read as no duration at all.
const LONGEST: i64 = 5 * 366 * DAY;
/// The most values one rule part lists.
const MOST_RULE_VALUES: usize = 400;
/// JSCalendar's LocalDateTime.
const LOCAL: &str = "%Y-%m-%dT%H:%M:%S";

/// The properties an occurrence's override cannot change.
const SERIES_ONLY: [&str; 7] = ["@type", "id", "uid", "recurrenceRule", "recurrenceRules", "recurrenceOverrides", "excludedRecurrenceRules"];

/// A patch: paths to new values, `null` removing one (RFC 8620 section 5.3).
type Patch = Map<String, Value>;

/// An ISO 8601 duration as JSCalendar writes one: days, which are nominal
/// (a day across a clock change lasts 23 or 25 hours), and an exact time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Length {
    days: i64,
    millis: i64,
}

fn length_of(text: &str) -> Option<Length> {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let (mut days, mut millis) = (0_i64, 0_i64);
    for (part, units) in [(date, &[('W', 7 * DAY), ('D', DAY)][..]), (time, &[('H', 3_600_000), ('M', 60_000), ('S', 1_000)][..])] {
        let mut number = String::new();
        for ch in part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                number.push(ch);
                continue;
            }
            let (unit, size) = units.iter().find(|(unit, _)| *unit == ch)?;
            let value: f64 = number.parse().ok()?;
            let amount = value * *size as f64;
            if !amount.is_finite() || amount > LONGEST as f64 {
                return None;
            }
            match unit {
                'W' | 'D' => {
                    let whole = value.trunc() * (*size / DAY) as f64;
                    days += whole as i64;
                    millis += (amount - whole * DAY as f64) as i64;
                }
                _ => millis += amount as i64,
            }
            number.clear();
        }
        if !number.is_empty() {
            return None;
        }
    }
    if days * DAY + millis > LONGEST {
        return None;
    }
    Some(Length { days: sign * days, millis: sign * millis })
}

/// An ISO 8601 duration in milliseconds, a day counted as 24 hours:
/// `PT1H30M`, `P1D`, `P1W`, `-PT10M`. Months and years have no fixed
/// length, and a duration past a few years is not read: both answer `None`.
pub(crate) fn duration_millis(text: &str) -> Option<i64> {
    length_of(text).map(|l| l.days * DAY + l.millis)
}

pub(crate) fn duration_text(millis: i64) -> String {
    let sign = if millis < 0 { "-" } else { "" };
    let mut left = millis.unsigned_abs() / 1000;
    let days = left / 86_400;
    left %= 86_400;
    let mut out = format!("{sign}P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if left > 0 || days == 0 {
        out.push_str(&time_text(left));
    }
    out
}

/// `T` and the hours, minutes and seconds of `seconds`, with `T0S` for none.
fn time_text(seconds: u64) -> String {
    let (hours, minutes, rest) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    let mut out = String::from("T");
    if hours > 0 {
        out.push_str(&format!("{hours}H"));
    }
    if minutes > 0 {
        out.push_str(&format!("{minutes}M"));
    }
    if rest > 0 || seconds == 0 {
        out.push_str(&format!("{rest}S"));
    }
    out
}

/// How a JSCalendar LocalDateTime becomes an instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clock {
    /// A zone the server names and chrono-tz knows.
    Zone(Tz),
    /// This computer's zone: a floating time, which means the same clock
    /// time wherever the reader is, or a zone name nothing here knows.
    Local,
    /// An all-day event's dates, at midnight UTC as the copy keeps them.
    Utc,
}

impl Clock {
    fn millis(self, naive: NaiveDateTime) -> Option<EpochMillis> {
        match self {
            Clock::Zone(tz) => instant(&tz, naive),
            Clock::Local => instant(&chrono::Local, naive),
            Clock::Utc => Some(naive.and_utc().timestamp_millis()),
        }
    }

    fn parse(self, local: &str) -> Option<EpochMillis> {
        self.millis(NaiveDateTime::parse_from_str(local, LOCAL).ok()?)
    }

    fn naive(self, at: EpochMillis) -> NaiveDateTime {
        let utc = DateTime::<Utc>::from_timestamp_millis(at).unwrap_or_default();
        match self {
            Clock::Zone(tz) => utc.with_timezone(&tz).naive_local(),
            Clock::Local => utc.with_timezone(&chrono::Local).naive_local(),
            Clock::Utc => utc.naive_utc(),
        }
    }

    fn text(self, at: EpochMillis) -> String {
        self.naive(at).format(LOCAL).to_string()
    }
}

/// The zone a JSCalendar `timeZone` names: its IANA name in any case, or a
/// Windows name.
fn named_zone(name: &str) -> Option<Tz> {
    let name = name.trim();
    model::named_zone(name).or_else(|| chrono_tz::TZ_VARIANTS.iter().copied().find(|tz| tz.name().eq_ignore_ascii_case(name)))
}

/// The clock an event of the copy is written in, which is the clock it was
/// read in while its zone stays the same.
fn clock_for(event: &model::Event) -> Clock {
    if event.all_day {
        return Clock::Utc;
    }
    named_zone(&event.zone).map_or(Clock::Local, Clock::Zone)
}

/// An event's properties: the object's own, or for a changed occurrence
/// its override's on top of its series'. A `null` reads as no value, and
/// in the override it hides the series' property.
#[derive(Clone, Copy)]
struct Fields<'a> {
    base: &'a Value,
    over: Option<&'a Map<String, Value>>,
}

impl<'a> Fields<'a> {
    fn of(object: &'a Value) -> Fields<'a> {
        Fields { base: object, over: None }
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        match self.over.and_then(|over| over.get(key)) {
            Some(value) => Some(value),
            None => self.base.get(key),
        }
        .filter(|value| !value.is_null())
    }

    fn text(&self, key: &str) -> Option<&'a str> {
        self.get(key).and_then(Value::as_str)
    }

    fn map(&self, key: &str) -> Option<&'a Map<String, Value>> {
        self.get(key).and_then(Value::as_object)
    }

    /// The value at a patch path, or `None` where there is none.
    fn at(&self, path: &str) -> Option<&'a Value> {
        let mut parts = path.split('/').map(unescape);
        let mut at = self.get(&parts.next()?)?;
        for part in parts {
            at = at.get(&part)?;
        }
        Some(at)
    }

    fn all_day(&self) -> bool {
        self.get("showWithoutTime").and_then(Value::as_bool).unwrap_or(false)
    }

    /// The clock the times are read in, and the zone name the copy keeps:
    /// an IANA name; for a name nothing here knows, the server's own, read
    /// in this computer's zone; empty for a floating time.
    fn clock(&self) -> (Clock, String) {
        if self.all_day() {
            return (Clock::Utc, "UTC".into());
        }
        match self.text("timeZone") {
            None => (Clock::Local, String::new()),
            Some(name) => match named_zone(name) {
                Some(tz) => (Clock::Zone(tz), tz.name().to_string()),
                None => (Clock::Local, name.chars().take(64).collect()),
            },
        }
    }
}

fn unescape(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}

fn answer_of(status: &str) -> Option<Answer> {
    match status {
        "accepted" => Some(Answer::Yes),
        "declined" => Some(Answer::No),
        "tentative" => Some(Answer::Maybe),
        _ => None,
    }
}

pub(crate) fn status_of(answer: Option<Answer>) -> &'static str {
    match answer {
        Some(Answer::Yes) => "accepted",
        Some(Answer::No) => "declined",
        Some(Answer::Maybe) => "tentative",
        None => "needs-action",
    }
}

/// The address of a `mailto:` URI in any case, its `?` query left off, or
/// a bare address.
fn mailto(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    let address = match text.get(..7) {
        Some(scheme) if scheme.eq_ignore_ascii_case("mailto:") => &text[7..],
        _ => text,
    };
    let address = address.split('?').next().unwrap_or_default().trim();
    Some(address.to_ascii_lowercase()).filter(|a| a.contains('@'))
}

/// A participant's address: JSCalendar 2.0's `calendarAddress`, RFC
/// 8984's `sendTo.imip`, or its plain `email`.
fn address_of(participant: &Value) -> Option<String> {
    participant
        .get("calendarAddress")
        .and_then(mailto)
        .or_else(|| participant.get("sendTo").and_then(|s| s.get("imip")).and_then(mailto))
        .or_else(|| participant.get("email").and_then(mailto))
}

fn has_role(participant: &Value, role: &str) -> bool {
    participant.get("roles").and_then(|r| r.get(role)).and_then(Value::as_bool).unwrap_or(false)
}

/// The participants with an address, by id. A room or a resource with no
/// address is no guest, and a patch never touches it.
fn people<'a>(fields: &Fields<'a>) -> impl Iterator<Item = (&'a str, &'a Value, String)> {
    fields
        .map("participants")
        .into_iter()
        .flat_map(|p| p.iter().take(MOST_LISTED))
        .filter_map(|(id, p)| Some((id.as_str(), p, address_of(p)?)))
}

/// The id of the participant who is the account, by any of `me`.
pub(crate) fn participant_for(object: &Value, me: &[String]) -> Option<String> {
    people(&Fields::of(object)).find_map(|(id, _, address)| me.iter().any(|m| m.eq_ignore_ascii_case(&address)).then(|| id.to_string()))
}

const FREQUENCIES: [&str; 7] = ["yearly", "monthly", "weekly", "daily", "hourly", "minutely", "secondly"];
const WEEKDAYS: [&str; 7] = ["mo", "tu", "we", "th", "fr", "sa", "su"];
const RULE_KEYS: [&str; 17] = [
    "@type", "frequency", "interval", "rscale", "skip", "firstDayOfWeek", "byDay", "byMonthDay", "byMonth", "byYearDay", "byWeekNo",
    "byHour", "byMinute", "bySecond", "bySetPosition", "count", "until",
];
/// The numbered rule parts: JSCalendar's name, iCalendar's, and the range
/// RFC 5545 allows. A signed part also counts back from the end, and
/// never takes 0.
const NUMBERED: [(&str, &str, i64, i64, bool); 7] = [
    ("byMonthDay", "BYMONTHDAY", 1, 31, true),
    ("byYearDay", "BYYEARDAY", 1, 366, true),
    ("byWeekNo", "BYWEEKNO", 1, 53, true),
    ("bySetPosition", "BYSETPOS", 1, 366, true),
    ("byHour", "BYHOUR", 0, 23, false),
    ("byMinute", "BYMINUTE", 0, 59, false),
    ("bySecond", "BYSECOND", 0, 60, false),
];

fn in_range(n: i64, low: i64, high: i64, signed: bool) -> bool {
    match signed {
        true => n != 0 && (low..=high).contains(&n.abs()),
        false => (low..=high).contains(&n),
    }
}

fn weekday(text: &str) -> Option<&'static str> {
    WEEKDAYS.iter().copied().find(|d| d.eq_ignore_ascii_case(text))
}

/// The values of one rule part, at least one and not too many.
fn listed(value: &Value) -> Option<&Vec<Value>> {
    value.as_array().filter(|v| !v.is_empty() && v.len() <= MOST_RULE_VALUES)
}

/// How a series repeats, as the copy reads it.
enum Repeat {
    Once,
    Rule(String),
    /// A rule the copy cannot write as an `RRULE` without changing it.
    Unread,
}

fn repeat_of(fields: &Fields, clock: Clock, all_day: bool) -> Repeat {
    let excluded = fields.get("excludedRecurrenceRules").and_then(Value::as_array).is_some_and(|r| !r.is_empty());
    let rule = match (fields.get("recurrenceRule"), fields.get("recurrenceRules")) {
        (Some(rule), _) => rule,
        (None, Some(Value::Array(rules))) if rules.is_empty() => return Repeat::Once,
        (None, Some(Value::Array(rules))) if rules.len() == 1 => &rules[0],
        (None, Some(_)) => return Repeat::Unread,
        (None, None) if excluded => return Repeat::Unread,
        (None, None) => return Repeat::Once,
    };
    match rrule_of(rule, clock, all_day) {
        Some(line) if !excluded => Repeat::Rule(line),
        _ => Repeat::Unread,
    }
}

/// The `RRULE` line for a JSCalendar rule, or `None` for one the copy
/// cannot write exactly: another calendar scale, a skip other than
/// `omit`, a leap month, a part it does not know, or any value outside
/// what RFC 5545 allows, which also keeps a value from adding parts of its
/// own to the line.
fn rrule_of(rule: &Value, clock: Clock, all_day: bool) -> Option<String> {
    let rule = rule.as_object()?;
    if rule.keys().any(|key| !RULE_KEYS.contains(&key.as_str())) {
        return None;
    }
    if rule.get("@type").is_some_and(|t| t != "RecurrenceRule") {
        return None;
    }
    if rule.get("rscale").is_some_and(|r| !r.as_str().is_some_and(|r| r.eq_ignore_ascii_case("gregorian"))) {
        return None;
    }
    if rule.get("skip").is_some_and(|s| s != "omit") {
        return None;
    }
    let frequency = rule.get("frequency")?.as_str()?;
    let frequency = FREQUENCIES.iter().find(|f| f.eq_ignore_ascii_case(frequency))?;
    let mut parts = vec![format!("FREQ={}", frequency.to_ascii_uppercase())];
    if let Some(interval) = rule.get("interval") {
        let interval = interval.as_u64().filter(|n| *n >= 1)?;
        if interval > 1 {
            parts.push(format!("INTERVAL={interval}"));
        }
    }
    if let Some(days) = rule.get("byDay") {
        let mut out = Vec::new();
        for day in listed(days)? {
            let day = day.as_object()?;
            if day.keys().any(|key| !["@type", "day", "nthOfPeriod"].contains(&key.as_str())) {
                return None;
            }
            let name = weekday(day.get("day")?.as_str()?)?.to_ascii_uppercase();
            out.push(match day.get("nthOfPeriod") {
                None => name,
                Some(nth) => format!("{}{name}", nth.as_i64().filter(|n| in_range(*n, 1, 53, true))?),
            });
        }
        parts.push(format!("BYDAY={}", out.join(",")));
    }
    if let Some(months) = rule.get("byMonth") {
        let months: Option<Vec<String>> = listed(months)?
            .iter()
            .map(|m| m.as_str()?.parse::<u8>().ok().filter(|m| (1..=12).contains(m)).map(|m| m.to_string()))
            .collect();
        parts.push(format!("BYMONTH={}", months?.join(",")));
    }
    for (key, name, low, high, signed) in NUMBERED {
        if let Some(values) = rule.get(key) {
            let values: Option<Vec<String>> =
                listed(values)?.iter().map(|n| n.as_i64().filter(|n| in_range(*n, low, high, signed)).map(|n| n.to_string())).collect();
            parts.push(format!("{name}={}", values?.join(",")));
        }
    }
    if let Some(first) = rule.get("firstDayOfWeek") {
        let first = weekday(first.as_str()?)?;
        if first != "mo" {
            parts.push(format!("WKST={}", first.to_ascii_uppercase()));
        }
    }
    if rule.contains_key("count") && rule.contains_key("until") {
        return None;
    }
    if let Some(count) = rule.get("count") {
        parts.push(format!("COUNT={}", count.as_u64().filter(|n| *n >= 1)?));
    }
    if let Some(until) = rule.get("until") {
        let utc = DateTime::<Utc>::from_timestamp_millis(clock.parse(until.as_str()?)?)?;
        parts.push(match all_day {
            true => format!("UNTIL={}", utc.format("%Y%m%d")),
            false => format!("UNTIL={}", utc.format("%Y%m%dT%H%M%SZ")),
        });
    }
    Some(format!("RRULE:{}", parts.join(";")))
}

fn day_object(text: &str) -> Option<Value> {
    let text = text.trim();
    let (nth, day) = text.split_at_checked(text.len().checked_sub(2)?)?;
    let day = weekday(day)?;
    match nth.trim_start_matches('+') {
        "" => Some(json!({ "@type": "NDay", "day": day })),
        nth => {
            let nth = nth.parse::<i64>().ok().filter(|n| in_range(*n, 1, 53, true))?;
            Some(json!({ "@type": "NDay", "day": day, "nthOfPeriod": nth }))
        }
    }
}

/// An `UNTIL` as JSCalendar's LocalDateTime on `clock`.
fn until_local(value: &str, clock: Clock, all_day: bool) -> Option<String> {
    if value.len() == 8 {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        let end_of_day = if all_day { date.and_hms_opt(0, 0, 0)? } else { date.and_hms_opt(23, 59, 59)? };
        return Some(end_of_day.format(LOCAL).to_string());
    }
    let naive = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
    let at = match value.ends_with('Z') {
        true => naive.and_utc().timestamp_millis(),
        false => clock.millis(naive)?,
    };
    Some(clock.text(at))
}

/// An `RRULE` line as a JSCalendar rule, or `None` for a part or a value
/// JSCalendar has no place for.
fn rule_object(line: &str, clock: Clock, all_day: bool) -> Option<Value> {
    let body = line.split_once(':').map_or(line, |(_, b)| b);
    let mut rule = Map::new();
    rule.insert("@type".into(), json!("RecurrenceRule"));
    for pair in body.split(';').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=')?;
        let key = key.to_ascii_uppercase();
        if let Some((name, _, low, high, signed)) = NUMBERED.iter().find(|n| n.1 == key) {
            let numbers: Option<Vec<i64>> =
                value.split(',').map(|n| n.trim().parse::<i64>().ok().filter(|n| in_range(*n, *low, *high, *signed))).collect();
            rule.insert((*name).into(), json!(numbers?));
            continue;
        }
        let (name, entry) = match key.as_str() {
            "FREQ" => ("frequency", json!(FREQUENCIES.iter().find(|f| f.eq_ignore_ascii_case(value))?)),
            "INTERVAL" => ("interval", json!(value.parse::<u64>().ok().filter(|n| *n >= 1)?)),
            "COUNT" => ("count", json!(value.parse::<u64>().ok().filter(|n| *n >= 1)?)),
            "BYDAY" => ("byDay", json!(value.split(',').map(day_object).collect::<Option<Vec<_>>>()?)),
            "BYMONTH" => {
                let months: Option<Vec<String>> =
                    value.split(',').map(|m| m.trim().parse::<u8>().ok().filter(|m| (1..=12).contains(m)).map(|m| m.to_string())).collect();
                ("byMonth", json!(months?))
            }
            "WKST" => ("firstDayOfWeek", json!(weekday(value)?)),
            "UNTIL" => ("until", json!(until_local(value, clock, all_day)?)),
            "RSCALE" if value.eq_ignore_ascii_case("GREGORIAN") => continue,
            "SKIP" if value.eq_ignore_ascii_case("OMIT") => continue,
            _ => return None,
        };
        rule.insert(name.into(), entry);
    }
    rule.contains_key("frequency").then_some(Value::Object(rule))
}

/// The `EXDATE` line for each occurrence the series leaves out.
fn exdates(fields: &Fields, clock: Clock, all_day: bool) -> Vec<String> {
    fields
        .map("recurrenceOverrides")
        .into_iter()
        .flat_map(|o| o.iter().take(MOST_OVERRIDES))
        .filter(|(_, patch)| patch.get("excluded").and_then(Value::as_bool) == Some(true))
        .filter_map(|(key, _)| {
            let utc = DateTime::<Utc>::from_timestamp_millis(clock.parse(key)?)?;
            Some(match all_day {
                true => format!("EXDATE;VALUE=DATE:{}", utc.format("%Y%m%d")),
                false => format!("EXDATE:{}", utc.format("%Y%m%dT%H%M%SZ")),
            })
        })
        .collect()
}

/// The instants the event's `EXDATE` lines leave out, a floating or
/// zoneless one read on `clock`.
fn excluded_instants(rules: &[String], clock: Clock) -> Vec<EpochMillis> {
    let mut keys = Vec::new();
    for line in rules.iter().filter(|l| l.to_ascii_uppercase().starts_with("EXDATE")) {
        let (head, values) = line.split_once(':').unwrap_or((line.as_str(), ""));
        let tzid = head.split(';').find_map(|p| p.strip_prefix("TZID=")).and_then(named_zone);
        for value in values.split(',').map(str::trim) {
            if value.len() == 8 {
                if let Some(midnight) = NaiveDate::parse_from_str(value, "%Y%m%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)) {
                    keys.push(midnight.and_utc().timestamp_millis());
                }
                continue;
            }
            let Ok(naive) = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S") else {
                continue;
            };
            let at = match (value.ends_with('Z'), tzid) {
                (true, _) => Some(naive.and_utc().timestamp_millis()),
                (false, Some(tz)) => instant(&tz, naive),
                (false, None) => clock.millis(naive),
            };
            keys.extend(at);
        }
    }
    keys
}

/// The alerts the copy shows as reminders, by id: an offset before the
/// start. An alert at a fixed time, after the start or relative to the
/// end has no reminder to stand for it and stays as it is.
fn reminder_alerts<'a>(fields: &Fields<'a>) -> impl Iterator<Item = (&'a str, Reminder)> {
    fields.map("alerts").into_iter().flat_map(|a| a.iter().take(MOST_LISTED)).filter_map(|(id, alert)| {
        let trigger = alert.get("trigger")?;
        if trigger.get("@type").is_some_and(|t| t != "OffsetTrigger") || trigger.get("relativeTo").is_some_and(|r| r != "start") {
            return None;
        }
        let before = -duration_millis(trigger.get("offset")?.as_str()?)?;
        let reminder = Reminder {
            minutes: u32::try_from(before / 60_000).ok()?,
            method: match alert.get("action").and_then(Value::as_str) {
                Some("email") => ReminderMethod::Email,
                _ => ReminderMethod::Notification,
            },
        };
        (before >= 0).then_some((id.as_str(), reminder))
    })
}

/// The reminders a calendar's default alerts stand for, as an event that
/// uses them would show.
pub(crate) fn default_reminders(alerts: &Map<String, Value>) -> Vec<Reminder> {
    let holder = json!({ "alerts": alerts });
    reminder_alerts(&Fields::of(&holder)).map(|(_, r)| r).collect()
}

fn reminders_of(fields: &Fields) -> Option<Vec<Reminder>> {
    if fields.get("useDefaultAlerts").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    fields.map("alerts")?;
    Some(reminder_alerts(fields).map(|(_, r)| r).collect())
}

/// CSS's named colours (CSS Color Module Level 4), which JSCalendar's
/// `color` may hold.
const CSS_COLOURS: [(&str, &str); 148] = [
    ("aliceblue", "#f0f8ff"), ("antiquewhite", "#faebd7"), ("aqua", "#00ffff"), ("aquamarine", "#7fffd4"), ("azure", "#f0ffff"),
    ("beige", "#f5f5dc"), ("bisque", "#ffe4c4"), ("black", "#000000"), ("blanchedalmond", "#ffebcd"), ("blue", "#0000ff"),
    ("blueviolet", "#8a2be2"), ("brown", "#a52a2a"), ("burlywood", "#deb887"), ("cadetblue", "#5f9ea0"),
    ("chartreuse", "#7fff00"), ("chocolate", "#d2691e"), ("coral", "#ff7f50"), ("cornflowerblue", "#6495ed"),
    ("cornsilk", "#fff8dc"), ("crimson", "#dc143c"), ("cyan", "#00ffff"), ("darkblue", "#00008b"), ("darkcyan", "#008b8b"),
    ("darkgoldenrod", "#b8860b"), ("darkgray", "#a9a9a9"), ("darkgreen", "#006400"), ("darkgrey", "#a9a9a9"),
    ("darkkhaki", "#bdb76b"), ("darkmagenta", "#8b008b"), ("darkolivegreen", "#556b2f"), ("darkorange", "#ff8c00"),
    ("darkorchid", "#9932cc"), ("darkred", "#8b0000"), ("darksalmon", "#e9967a"), ("darkseagreen", "#8fbc8f"),
    ("darkslateblue", "#483d8b"), ("darkslategray", "#2f4f4f"), ("darkslategrey", "#2f4f4f"), ("darkturquoise", "#00ced1"),
    ("darkviolet", "#9400d3"), ("deeppink", "#ff1493"), ("deepskyblue", "#00bfff"), ("dimgray", "#696969"),
    ("dimgrey", "#696969"), ("dodgerblue", "#1e90ff"), ("firebrick", "#b22222"), ("floralwhite", "#fffaf0"),
    ("forestgreen", "#228b22"), ("fuchsia", "#ff00ff"), ("gainsboro", "#dcdcdc"), ("ghostwhite", "#f8f8ff"), ("gold", "#ffd700"),
    ("goldenrod", "#daa520"), ("gray", "#808080"), ("green", "#008000"), ("greenyellow", "#adff2f"), ("grey", "#808080"),
    ("honeydew", "#f0fff0"), ("hotpink", "#ff69b4"), ("indianred", "#cd5c5c"), ("indigo", "#4b0082"), ("ivory", "#fffff0"),
    ("khaki", "#f0e68c"), ("lavender", "#e6e6fa"), ("lavenderblush", "#fff0f5"), ("lawngreen", "#7cfc00"),
    ("lemonchiffon", "#fffacd"), ("lightblue", "#add8e6"), ("lightcoral", "#f08080"), ("lightcyan", "#e0ffff"),
    ("lightgoldenrodyellow", "#fafad2"), ("lightgray", "#d3d3d3"), ("lightgreen", "#90ee90"), ("lightgrey", "#d3d3d3"),
    ("lightpink", "#ffb6c1"), ("lightsalmon", "#ffa07a"), ("lightseagreen", "#20b2aa"), ("lightskyblue", "#87cefa"),
    ("lightslategray", "#778899"), ("lightslategrey", "#778899"), ("lightsteelblue", "#b0c4de"), ("lightyellow", "#ffffe0"),
    ("lime", "#00ff00"), ("limegreen", "#32cd32"), ("linen", "#faf0e6"), ("magenta", "#ff00ff"), ("maroon", "#800000"),
    ("mediumaquamarine", "#66cdaa"), ("mediumblue", "#0000cd"), ("mediumorchid", "#ba55d3"), ("mediumpurple", "#9370db"),
    ("mediumseagreen", "#3cb371"), ("mediumslateblue", "#7b68ee"), ("mediumspringgreen", "#00fa9a"),
    ("mediumturquoise", "#48d1cc"), ("mediumvioletred", "#c71585"), ("midnightblue", "#191970"), ("mintcream", "#f5fffa"),
    ("mistyrose", "#ffe4e1"), ("moccasin", "#ffe4b5"), ("navajowhite", "#ffdead"), ("navy", "#000080"), ("oldlace", "#fdf5e6"),
    ("olive", "#808000"), ("olivedrab", "#6b8e23"), ("orange", "#ffa500"), ("orangered", "#ff4500"), ("orchid", "#da70d6"),
    ("palegoldenrod", "#eee8aa"), ("palegreen", "#98fb98"), ("paleturquoise", "#afeeee"), ("palevioletred", "#db7093"),
    ("papayawhip", "#ffefd5"), ("peachpuff", "#ffdab9"), ("peru", "#cd853f"), ("pink", "#ffc0cb"), ("plum", "#dda0dd"),
    ("powderblue", "#b0e0e6"), ("purple", "#800080"), ("rebeccapurple", "#663399"), ("red", "#ff0000"), ("rosybrown", "#bc8f8f"),
    ("royalblue", "#4169e1"), ("saddlebrown", "#8b4513"), ("salmon", "#fa8072"), ("sandybrown", "#f4a460"),
    ("seagreen", "#2e8b57"), ("seashell", "#fff5ee"), ("sienna", "#a0522d"), ("silver", "#c0c0c0"), ("skyblue", "#87ceeb"),
    ("slateblue", "#6a5acd"), ("slategray", "#708090"), ("slategrey", "#708090"), ("snow", "#fffafa"), ("springgreen", "#00ff7f"),
    ("steelblue", "#4682b4"), ("tan", "#d2b48c"), ("teal", "#008080"), ("thistle", "#d8bfd8"), ("tomato", "#ff6347"),
    ("turquoise", "#40e0d0"), ("violet", "#ee82ee"), ("wheat", "#f5deb3"), ("white", "#ffffff"), ("whitesmoke", "#f5f5f5"),
    ("yellow", "#ffff00"), ("yellowgreen", "#9acd32"),
];

/// A CSS colour as `#rrggbb`: a hex one in either case or length, or one
/// of CSS's named colours. Anything else, such as `rgb(...)`, is `None`.
fn colour_of(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return match hex.len() {
            6 => Some(format!("#{}", hex.to_ascii_lowercase())),
            3 => Some(hex.chars().fold(String::from("#"), |mut out, c| {
                out.push(c.to_ascii_lowercase());
                out.push(c.to_ascii_lowercase());
                out
            })),
            _ => None,
        };
    }
    CSS_COLOURS.iter().find(|(name, _)| name.eq_ignore_ascii_case(text)).map(|(_, hex)| (*hex).to_string())
}

/// The first location with a name, which the copy shows as the place.
fn place_of<'a>(fields: &Fields<'a>) -> Option<(&'a str, &'a str)> {
    fields.map("locations")?.iter().find_map(|(id, v)| Some((id.as_str(), v.get("name")?.as_str()?)))
}

fn conference_of<'a>(fields: &Fields<'a>) -> Option<(&'a str, &'a str)> {
    fields.map("virtualLocations")?.iter().find_map(|(id, v)| Some((id.as_str(), v.get("uri")?.as_str()?)))
}

/// One event from one JSCalendar object, or from an occurrence's view of
/// its series. Only a series reads the rule.
fn one_event(fields: Fields, calendar: &str, me: &[String], series: bool) -> Option<model::Event> {
    let id = fields.base.get("id")?.as_str()?.to_string();
    let all_day = fields.all_day();
    let (clock, zone) = fields.clock();
    let mut naive = NaiveDateTime::parse_from_str(fields.text("start")?, LOCAL).ok()?;
    if all_day {
        naive = naive.date().and_hms_opt(0, 0, 0)?;
    }
    let start = clock.millis(naive)?;
    let length = fields.text("duration").and_then(length_of).filter(|l| l.days >= 0 && l.millis >= 0).unwrap_or(Length { days: 0, millis: 0 });
    let end = match all_day {
        true => {
            let days = (length.days * DAY + length.millis + DAY - 1) / DAY;
            start + days.max(1) * DAY
        }
        false => naive
            .checked_add_days(Days::new(length.days.unsigned_abs()))
            .and_then(|later| clock.millis(later))
            .map_or(start, |later| later + length.millis)
            .max(start),
    };
    let text = |key: &str| fields.text(key).unwrap_or_default().to_string();
    let mine = |address: &str| me.iter().any(|m| m.eq_ignore_ascii_case(address));
    let guests: Vec<Guest> = people(&fields)
        .map(|(_, p, email)| Guest {
            me: mine(&email),
            name: p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string),
            answer: p.get("participationStatus").and_then(Value::as_str).and_then(answer_of),
            organizer: has_role(p, "owner"),
            email,
        })
        .collect();
    let organizer = guests
        .iter()
        .find(|g| g.organizer)
        .map(|g| g.email.clone())
        .or_else(|| fields.get("replyTo").and_then(|r| r.get("imip")).and_then(mailto));
    let my_answer = guests.iter().find(|g| g.me && !g.organizer).and_then(|g| g.answer);
    let (rules, unread_rule) = match series {
        false => (Vec::new(), false),
        true => match repeat_of(&fields, clock, all_day) {
            Repeat::Once => (Vec::new(), false),
            Repeat::Unread => (Vec::new(), true),
            Repeat::Rule(line) => {
                let mut rules = vec![line];
                rules.extend(exdates(&fields, clock, all_day));
                (rules, false)
            }
        },
    };
    Some(model::Event {
        calendar: calendar.to_string(),
        id,
        uid: text("uid"),
        etag: text("updated"),
        start,
        end,
        zone,
        all_day,
        title: text("title"),
        place: place_of(&fields).map(|(_, name)| name.to_string()).unwrap_or_default(),
        description: text("description"),
        color: fields.text("color").and_then(colour_of),
        busy: fields.text("freeBusyStatus") != Some("free"),
        status: Status::parse(fields.text("status").unwrap_or("confirmed")),
        private: matches!(fields.text("privacy"), Some("private" | "secret")),
        organizer,
        guests,
        my_answer,
        reminders: reminders_of(&fields),
        conference: conference_of(&fields).map(|(_, uri)| uri.to_string()),
        rules,
        unread_rule,
        sequence: fields.get("sequence").and_then(Value::as_i64).unwrap_or(0),
        ..model::Event::default()
    })
}

/// What an override changes, on top of its series: a copy of each series
/// property one of its entries reaches into, with the entries applied,
/// and the start. An override with no start of its own starts at the
/// occurrence's original instant, on the occurrence's clock, which is not
/// the series' when the override changes the zone.
fn overlay(object: &Value, patch: &Map<String, Value>, original: EpochMillis) -> Map<String, Value> {
    let mut over = Value::Object(Map::new());
    for (path, value) in patch.iter().take(MOST_LISTED) {
        let top = unescape(path.split('/').next().unwrap_or_default());
        if top.is_empty() || SERIES_ONLY.contains(&top.as_str()) || top == "excluded" {
            continue;
        }
        if let Some(fields) = over.as_object_mut()
            && !fields.contains_key(&top)
        {
            fields.insert(top.clone(), object.get(&top).map_or(Value::Null, bounded_copy));
        }
        set_path(&mut over, path, value.clone());
        if let Some(fields) = over.as_object_mut() {
            fields.entry(top).or_insert(Value::Null);
        }
    }
    let Value::Object(mut over) = over else {
        return Map::new();
    };
    if !patch.contains_key("start") {
        let (clock, _) = Fields { base: object, over: Some(&over) }.clock();
        over.insert("start".into(), json!(clock.text(original)));
    }
    over
}

/// A copy of one series property for an override. A map keeps at most
/// the entries the copy reads, so a series with thousands of
/// participants and a thousand overrides costs no more than reading it.
fn bounded_copy(value: &Value) -> Value {
    match value.as_object() {
        Some(map) if map.len() > MOST_LISTED => Value::Object(map.iter().take(MOST_LISTED).map(|(k, v)| (k.clone(), v.clone())).collect()),
        _ => value.clone(),
    }
}

/// The key the server holds for the occurrence that starts at `at`: the
/// override's own key when there is one, so a key inside a clock-change
/// gap (01:30, read as 02:30) goes back as the server wrote it, and the
/// time on `clock` otherwise.
fn server_key(fields: &Fields, clock: Clock, at: EpochMillis) -> String {
    fields
        .map("recurrenceOverrides")
        .and_then(|o| o.keys().take(MOST_OVERRIDES).find(|key| clock.parse(key) == Some(at)).cloned())
        .unwrap_or_else(|| clock.text(at))
}

/// The event `object` holds, then one per changed occurrence of it.
pub(crate) fn events_of(object: &Value, calendar: &str, me: &[String]) -> Vec<model::Event> {
    let fields = Fields::of(object);
    let Some(series) = one_event(fields, calendar, me, true) else {
        return Vec::new();
    };
    let (clock, _) = fields.clock();
    let mut events = vec![series];
    for (key, patch) in fields.map("recurrenceOverrides").into_iter().flatten().take(MOST_OVERRIDES) {
        let Some(patch) = patch.as_object() else {
            continue;
        };
        if patch.get("excluded").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(original) = clock.parse(key) else {
            continue;
        };
        let over = overlay(object, patch, original);
        if let Some(mut occurrence) = one_event(Fields { base: object, over: Some(&over) }, calendar, me, false) {
            occurrence.id = occurrence_id(&events[0], original);
            occurrence.series = Some(events[0].id.clone());
            occurrence.original_start = Some(original);
            events.push(occurrence);
        }
    }
    events
}

/// Applies one patch entry (RFC 8620 section 5.3): `path` names a key, or
/// keys inside objects joined by `/` (`~1` and `~0` escaped), and `null`
/// takes the key out.
pub(crate) fn set_path(target: &mut Value, path: &str, value: Value) {
    let parts: Vec<String> = path.split('/').map(unescape).collect();
    let Some((last, inner)) = parts.split_last() else {
        return;
    };
    let mut at = target;
    for part in inner {
        if !at.is_object() {
            *at = json!({});
        }
        let Some(fields) = at.as_object_mut() else {
            return;
        };
        at = fields.entry(part.clone()).or_insert_with(|| json!({}));
    }
    let Some(fields) = at.as_object_mut() else {
        return;
    };
    match value {
        Value::Null => {
            fields.remove(last);
        }
        value => {
            fields.insert(last.clone(), value);
        }
    }
}

/// JSCalendar's duration from `start` to `end` on `clock`: whole days
/// where the clock time repeats, since a JSCalendar day is nominal, then
/// the exact rest.
fn length_text(start: EpochMillis, end: EpochMillis, clock: Clock) -> String {
    let end = end.max(start);
    if clock == Clock::Utc {
        let days = ((end - start + DAY - 1) / DAY).max(1);
        return format!("P{days}D");
    }
    let (from, to) = (clock.naive(start), clock.naive(end));
    let mut days = (to.date() - from.date()).num_days();
    if to.time() < from.time() {
        days -= 1;
    }
    let pivot = from.checked_add_days(Days::new(days.max(0).unsigned_abs())).and_then(|n| clock.millis(n));
    match pivot {
        Some(pivot) if days > 0 && pivot <= end => {
            let rest = (end - pivot).unsigned_abs() / 1000;
            if rest == 0 { format!("P{days}D") } else { format!("P{days}D{}", time_text(rest)) }
        }
        _ => format!("P{}", time_text((end - start).unsigned_abs() / 1000)),
    }
}

fn alert_value(reminder: &Reminder) -> Value {
    let action = match reminder.method {
        ReminderMethod::Email => "email",
        ReminderMethod::Notification => "display",
    };
    json!({
        "@type": "Alert",
        "trigger": { "@type": "OffsetTrigger", "offset": duration_text(-i64::from(reminder.minutes) * 60_000), "relativeTo": "start" },
        "action": action
    })
}

fn participant_value(person: &Invited) -> Value {
    let Invited { email, name, owner, answer } = *person;
    let mut roles = Map::new();
    roles.insert("attendee".into(), json!(true));
    if owner {
        roles.insert("owner".into(), json!(true));
    }
    let mut participant = json!({
        "@type": "Participant",
        "calendarAddress": format!("mailto:{email}"),
        "email": email,
        "roles": roles,
        "participationStatus": status_of(answer),
        "expectReply": !owner
    });
    if let Some(name) = name {
        participant["name"] = json!(name);
    }
    participant
}

/// One person the event invites, as a new participant takes them.
#[derive(Clone, Copy)]
struct Invited<'a> {
    email: &'a str,
    name: Option<&'a str>,
    owner: bool,
    answer: Option<Answer>,
}

/// Every person the event invites, its organizer too.
fn invited(event: &model::Event) -> impl Iterator<Item = Invited<'_>> {
    let organizer = event.organizer.as_deref();
    let is_organizer = move |email: &str| organizer.is_some_and(|o| o.eq_ignore_ascii_case(email));
    let unlisted = organizer.filter(|o| !event.guests.iter().any(|g| g.email.eq_ignore_ascii_case(o)));
    event
        .guests
        .iter()
        .map(move |g| Invited { email: &g.email, name: g.name.as_deref(), owner: g.organizer || is_organizer(&g.email), answer: g.answer })
        .chain(unlisted.map(|email| Invited { email, name: None, owner: true, answer: Some(Answer::Yes) }))
}

fn invites(event: &model::Event, address: &str) -> bool {
    invited(event).any(|person| person.email.eq_ignore_ascii_case(address))
}

/// Patch entries for a map property such as `alerts`, one entry at a time
/// so the entries the copy does not show stay. An event with no such map
/// gets it whole.
struct Entries<'p, 'a> {
    patch: &'p mut Patch,
    fields: Fields<'a>,
    key: &'static str,
}

impl Entries<'_, '_> {
    /// Sets entry `id`, or with `None` removes it.
    fn put(&mut self, id: &str, value: Option<Value>) {
        if self.fields.map(self.key).is_some() {
            self.patch.insert(format!("{}/{}", self.key, pointer(id)), value.unwrap_or(Value::Null));
            return;
        }
        let Some(value) = value else {
            return;
        };
        if let Some(whole) = self.patch.entry(self.key).or_insert_with(|| json!({})).as_object_mut() {
            whole.insert(id.to_string(), value);
        }
    }

    /// An id no entry has, such as `a3`.
    fn fresh(&self, prefix: &str) -> String {
        let taken = |id: &str| {
            self.fields.map(self.key).is_some_and(|m| m.contains_key(id))
                || self.fields.base.get(self.key).is_some_and(|m| m.get(id).is_some())
                || self.patch.contains_key(&format!("{}/{}", self.key, pointer(id)))
                || self.patch.get(self.key).is_some_and(|whole| whole.get(id).is_some())
        };
        (1..).map(|n| format!("{prefix}{n}")).find(|id| !taken(id)).unwrap_or_default()
    }
}

/// The entries that turn `before`, as read from `fields`, into `after`:
/// only the properties whose value in the copy changed.
fn changes(before: &model::Event, after: &model::Event, fields: Fields) -> Patch {
    let mut patch = Patch::new();
    if after.title != before.title {
        patch.insert("title".into(), json!(after.title));
    }
    if after.description != before.description {
        patch.insert("description".into(), json!(after.description));
        // The editor writes plain text, or HTML once the notes hold more
        // than one line or a link. The type follows what it wrote.
        let wanted = if mailrs_mime::notes::text(&after.description) == after.description { "text/plain" } else { "text/html" };
        if fields.text("descriptionContentType").unwrap_or("text/plain") != wanted {
            patch.insert("descriptionContentType".into(), json!(wanted));
        }
    }
    if after.status != before.status {
        patch.insert("status".into(), json!(after.status.as_str()));
    }
    if after.busy != before.busy {
        patch.insert("freeBusyStatus".into(), json!(if after.busy { "busy" } else { "free" }));
    }
    if after.private != before.private {
        patch.insert("privacy".into(), json!(if after.private { "private" } else { "public" }));
    }
    if after.color != before.color {
        patch.insert("color".into(), after.color.as_ref().map_or(Value::Null, |c| json!(c)));
    }
    if after.place != before.place {
        let mut locations = Entries { patch: &mut patch, fields, key: "locations" };
        match (place_of(&fields), after.place.is_empty()) {
            (Some((id, _)), true) => locations.put(id, None),
            (Some((id, _)), false) => {
                locations.patch.insert(format!("locations/{}/name", pointer(id)), json!(after.place));
            }
            (None, true) => {}
            (None, false) => {
                let id = locations.fresh("l");
                locations.put(&id, Some(json!({ "@type": "Location", "name": after.place })));
            }
        }
    }
    if after.conference != before.conference {
        let mut links = Entries { patch: &mut patch, fields, key: "virtualLocations" };
        match (conference_of(&fields), &after.conference) {
            (Some((id, _)), None) => links.put(id, None),
            (Some((id, _)), Some(uri)) => {
                links.patch.insert(format!("virtualLocations/{}/uri", pointer(id)), json!(uri));
            }
            (None, None) => {}
            (None, Some(uri)) => {
                let id = links.fresh("v");
                links.put(&id, Some(json!({ "@type": "VirtualLocation", "uri": uri })));
            }
        }
    }
    if (after.start, after.end, after.all_day, &after.zone) != (before.start, before.end, before.all_day, &before.zone) {
        let clock = clock_for(after);
        patch.insert("start".into(), json!(clock.text(after.start)));
        patch.insert("duration".into(), json!(length_text(after.start, after.end, clock)));
        if after.all_day != before.all_day {
            patch.insert("showWithoutTime".into(), json!(after.all_day));
        }
        let read_as = if fields.all_day() { timed_clock(&fields) } else { fields.clock().0 };
        if !after.all_day && clock != read_as {
            patch.insert(
                "timeZone".into(),
                match clock {
                    Clock::Zone(tz) => json!(tz.name()),
                    _ => Value::Null,
                },
            );
        }
    }
    if after.reminders != before.reminders {
        match &after.reminders {
            None => {
                patch.insert("useDefaultAlerts".into(), json!(true));
            }
            Some(wanted) => {
                if before.reminders.is_none() {
                    patch.insert("useDefaultAlerts".into(), json!(false));
                }
                let mut left: Vec<Option<&Reminder>> = wanted.iter().map(Some).collect();
                let mut alerts = Entries { patch: &mut patch, fields, key: "alerts" };
                for (id, reminder) in reminder_alerts(&fields) {
                    match left.iter_mut().find(|w| **w == Some(&reminder)) {
                        Some(kept) => *kept = None,
                        None => alerts.put(id, None),
                    }
                }
                for reminder in left.into_iter().flatten() {
                    let id = alerts.fresh("a");
                    alerts.put(&id, Some(alert_value(reminder)));
                }
            }
        }
    }
    let mut participants = Entries { patch: &mut patch, fields, key: "participants" };
    for (id, _, address) in people(&fields) {
        if !invites(after, &address) {
            participants.put(id, None);
        }
    }
    for person in invited(after) {
        if !invites(before, person.email) {
            let id = participants.fresh("p");
            participants.put(&id, Some(participant_value(&person)));
        }
    }
    for (id, _, address) in people(&fields) {
        let named = |event: &model::Event| event.guests.iter().find(|g| g.email.eq_ignore_ascii_case(&address)).map(|g| g.name.clone());
        // A guest list the editor rebuilt carries no names; only a new
        // name goes, never a name taken away.
        if let (Some(was), Some(Some(name))) = (named(before), named(after))
            && was.as_deref() != Some(name.as_str())
        {
            patch.insert(format!("participants/{}/name", pointer(id)), json!(name));
        }
    }
    if after.my_answer != before.my_answer
        && let Some((id, _, _)) =
            people(&fields).find(|(_, _, address)| before.guests.iter().any(|g| g.me && !g.organizer && g.email.eq_ignore_ascii_case(address)))
    {
        patch.insert(format!("participants/{}/participationStatus", pointer(id)), json!(status_of(after.my_answer)));
    }
    patch
}

/// The clock an all-day event's times would be read in without its
/// all-day flag.
fn timed_clock(fields: &Fields) -> Clock {
    fields.text("timeZone").and_then(named_zone).map_or(Clock::Local, Clock::Zone)
}

/// A guest may change only what is theirs: their reminders, whether the
/// time shows as busy, the colour, and their own answer.
fn only_mine(patch: &mut Patch) {
    patch.retain(|path, _| {
        let top = path.split('/').next().unwrap_or_default();
        ["alerts", "useDefaultAlerts", "freeBusyStatus", "color"].contains(&top) || (top == "participants" && path.ends_with("/participationStatus"))
    });
}

fn unreadable_repeat() -> BackendError {
    BackendError::Refused(gettext("Penguin Mail cannot read how this event repeats, so it cannot change the repeat. Change it in the app that set it."))
}

fn unwritable_repeat() -> BackendError {
    BackendError::Refused(gettext("Penguin Mail cannot save this repeat to a JMAP calendar."))
}

/// A new event, without `calendarIds`, which the adapter adds. A repeat
/// JSCalendar cannot hold is refused rather than left off.
pub(crate) fn object_of(event: &model::Event) -> Result<Map<String, Value>, BackendError> {
    let clock = clock_for(event);
    let mut object = Map::new();
    object.insert("@type".into(), json!("Event"));
    // Both calendars drafts the gate accepts write JSCalendar 2.0, where an
    // event of its own must name its version. Cyrus refuses one without.
    object.insert("version".into(), json!("2.0"));
    if !event.uid.is_empty() {
        object.insert("uid".into(), json!(event.uid));
    }
    object.insert("title".into(), json!(event.title));
    object.insert("description".into(), json!(event.description));
    object.insert("start".into(), json!(clock.text(event.start)));
    if let Clock::Zone(tz) = clock {
        object.insert("timeZone".into(), json!(tz.name()));
    }
    object.insert("showWithoutTime".into(), json!(event.all_day));
    object.insert("duration".into(), json!(length_text(event.start, event.end, clock)));
    object.insert("status".into(), json!(event.status.as_str()));
    object.insert("freeBusyStatus".into(), json!(if event.busy { "busy" } else { "free" }));
    object.insert("privacy".into(), json!(if event.private { "private" } else { "public" }));
    if !event.place.is_empty() {
        object.insert("locations".into(), json!({ "l1": { "@type": "Location", "name": event.place } }));
    }
    if let Some(link) = &event.conference {
        object.insert("virtualLocations".into(), json!({ "v1": { "@type": "VirtualLocation", "uri": link } }));
    }
    if let Some(color) = &event.color {
        object.insert("color".into(), json!(color));
    }
    let participants: Map<String, Value> =
        invited(event).enumerate().map(|(n, person)| (format!("p{}", n + 1), participant_value(&person))).collect();
    if !participants.is_empty() {
        object.insert("participants".into(), Value::Object(participants));
    }
    if let Some(line) = event.rules.iter().find(|l| model::is_rule_line(l)) {
        object.insert("recurrenceRule".into(), rule_object(line, clock, event.all_day).ok_or_else(unwritable_repeat)?);
    }
    let excluded: Map<String, Value> =
        excluded_instants(&event.rules, clock).into_iter().map(|at| (clock.text(at), json!({ "excluded": true }))).collect();
    if !excluded.is_empty() {
        object.insert("recurrenceOverrides".into(), Value::Object(excluded));
    }
    match &event.reminders {
        None => {
            object.insert("useDefaultAlerts".into(), json!(true));
        }
        Some(reminders) => {
            object.insert("useDefaultAlerts".into(), json!(false));
            let alerts: Map<String, Value> = reminders.iter().enumerate().map(|(i, r)| (format!("a{}", i + 1), alert_value(r))).collect();
            object.insert("alerts".into(), Value::Object(alerts));
        }
    }
    Ok(object)
}

/// The patch that turns the server's event, `current`, into `event`. Only
/// what the person changed goes: a property the copy reads in part, such
/// as the participants or the alerts, changes one entry at a time, and the
/// rest of the server's event stays as it is. A change to a repeat the
/// copy cannot read is refused. A guest changes only what belongs to them.
pub(crate) fn patch_of(event: &model::Event, current: &Value, me: &[String]) -> Result<Patch, BackendError> {
    let fields = Fields::of(current);
    let before = one_event(fields, &event.calendar, me, true).ok_or(BackendError::NotFound)?;
    let mut patch = changes(&before, event, fields);
    let rule_before = before.rules.iter().find(|l| model::is_rule_line(l));
    let rule_after = event.rules.iter().find(|l| model::is_rule_line(l));
    if rule_before != rule_after {
        if before.unread_rule {
            return Err(unreadable_repeat());
        }
        let key = match (current.get("recurrenceRule"), current.get("recurrenceRules")) {
            (None, Some(_)) => "recurrenceRules",
            _ => "recurrenceRule",
        };
        let rule = match rule_after {
            None => Value::Null,
            Some(line) => rule_object(line, clock_for(event), event.all_day).ok_or_else(unwritable_repeat)?,
        };
        let rule = match (key, rule) {
            ("recurrenceRules", Value::Null) => Value::Null,
            ("recurrenceRules", rule) => json!([rule]),
            (_, rule) => rule,
        };
        patch.insert(key.into(), rule);
    }
    let (clock, _) = fields.clock();
    let had = excluded_instants(&before.rules, clock);
    let want = excluded_instants(&event.rules, clock);
    if before.unread_rule && want.iter().any(|at| !had.contains(at)) {
        return Err(unreadable_repeat());
    }
    let delta = event.start - before.start;
    let changed_held = fields
        .map("recurrenceOverrides")
        .is_some_and(|o| o.values().any(|v| v.get("excluded").and_then(Value::as_bool) != Some(true)));
    if delta != 0 && changed_held {
        let moved = moved_overrides(&fields, clock, clock_for(event), delta, &had, &want);
        patch.insert("recurrenceOverrides".into(), Value::Object(moved));
    } else {
        let mut overrides = Entries { patch: &mut patch, fields, key: "recurrenceOverrides" };
        for at in want.iter().filter(|at| !had.contains(at)) {
            overrides.put(&server_key(&fields, clock, *at), Some(json!({ "excluded": true })));
        }
        for at in had.iter().filter(|at| !want.contains(at)) {
            overrides.put(&server_key(&fields, clock, *at), None);
        }
    }
    if before.limited() {
        only_mine(&mut patch);
    }
    Ok(patch)
}

/// The series' `recurrenceOverrides` for a series moved by `delta`: every
/// key, read on `was` and written on `now`, moves by the same amount, as
/// the copy moves its `EXDATE` lines. A changed occurrence left under its
/// old key would be an occurrence of its own (RFC 8984 4.3.5), shown
/// beside the moved series' new one. The map goes whole, since a moved
/// key can land on another entry's old one. `had` and `want` are the
/// excluded instants before and after the edit, which add or drop an
/// exclusion the move did not carry.
fn moved_overrides(fields: &Fields, was: Clock, now: Clock, delta: EpochMillis, had: &[EpochMillis], want: &[EpochMillis]) -> Map<String, Value> {
    let mut moved = Map::new();
    for (key, value) in fields.map("recurrenceOverrides").into_iter().flatten() {
        let excluded = value.get("excluded").and_then(Value::as_bool) == Some(true);
        match was.parse(key) {
            Some(at) if excluded && !want.contains(&(at + delta)) && had.contains(&at) => {}
            Some(at) => {
                moved.insert(now.text(at + delta), value.clone());
            }
            None => {
                moved.insert(key.clone(), value.clone());
            }
        }
    }
    let shifted: Vec<EpochMillis> = had.iter().map(|at| at + delta).collect();
    for at in want.iter().filter(|at| !shifted.contains(at)) {
        moved.insert(now.text(*at), json!({ "excluded": true }));
    }
    moved
}

/// What one changed occurrence overrides in its series, `current`: the
/// override it had, with what the person changed now. A property that
/// matches the series again leaves the override.
pub(crate) fn override_of(event: &model::Event, current: &Value, me: &[String]) -> Result<Value, BackendError> {
    let original = event.original_start.ok_or(BackendError::NotFound)?;
    let key = override_key(current, original);
    let mut over: Map<String, Value> = current
        .get("recurrenceOverrides")
        .and_then(|o| o.get(&key))
        .and_then(Value::as_object)
        .filter(|o| o.get("excluded").and_then(Value::as_bool) != Some(true))
        .cloned()
        .unwrap_or_default();
    let view = overlay(current, &over, original);
    let fields = Fields { base: current, over: Some(&view) };
    let before = one_event(fields, &event.calendar, me, false).ok_or(BackendError::NotFound)?;
    let mut patch = changes(&before, event, fields);
    if before.limited() {
        only_mine(&mut patch);
    }
    let plain = overlay(current, &Map::new(), original);
    let series = Fields { base: current, over: Some(&plain) };
    for (path, value) in patch {
        override_set(&mut over, &path, value, series.at(&path));
    }
    Ok(Value::Object(over))
}

/// The occurrence of `current` that started at `original` as the server
/// holds it now, with its override if it has one: the event
/// [`override_of`] compares a change with. `None` for an excluded one.
pub(crate) fn occurrence_of(current: &Value, calendar: &str, me: &[String], original: EpochMillis) -> Option<model::Event> {
    let series = one_event(Fields::of(current), calendar, me, true)?;
    let over = match current.get("recurrenceOverrides").and_then(|o| o.get(override_key(current, original))) {
        Some(held) if held.get("excluded").and_then(Value::as_bool) == Some(true) => return None,
        Some(held) => held.as_object().cloned().unwrap_or_default(),
        None => Map::new(),
    };
    let view = overlay(current, &over, original);
    let mut occurrence = one_event(Fields { base: current, over: Some(&view) }, calendar, me, false)?;
    occurrence.id = occurrence_id(&series, original);
    occurrence.series = Some(series.id);
    occurrence.original_start = Some(original);
    Some(occurrence)
}

/// Puts one change into an override. Inside a property the override
/// already replaces, the change goes into that copy; otherwise the entry
/// goes, or leaves when the series holds the same.
fn override_set(over: &mut Map<String, Value>, path: &str, value: Value, series: Option<&Value>) {
    if let Some(parent) = over.keys().find(|k| path.starts_with(&format!("{k}/"))).cloned() {
        if let Some(holder) = over.get_mut(&parent) {
            set_path(holder, &path[parent.len() + 1..], value);
        }
        return;
    }
    over.retain(|k, _| !k.starts_with(&format!("{path}/")));
    let same = match series {
        Some(held) => *held == value,
        None => value.is_null(),
    };
    if same {
        over.remove(path);
    } else {
        over.insert(path.to_string(), value);
    }
}

/// The recurrence id of the occurrence of `current` that started at
/// `original`: the server's own key when it has an override there.
pub(crate) fn override_key(current: &Value, original: EpochMillis) -> String {
    let fields = Fields::of(current);
    server_key(&fields, fields.clock().0, original)
}

/// The recurrence id to mark excluded when the occurrence of `current`
/// that started at `original` is deleted. A series whose rule the copy
/// cannot read is refused: the copy does not know its occurrences.
pub(crate) fn exclusion_key(current: &Value, original: EpochMillis) -> Result<String, BackendError> {
    let fields = Fields::of(current);
    let (clock, _) = fields.clock();
    if matches!(repeat_of(&fields, clock, fields.all_day()), Repeat::Unread) {
        return Err(unreadable_repeat());
    }
    Ok(server_key(&fields, clock, original))
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};
    use mailrs_domain::calendar::{Guest, Reminder, ReminderMethod, Status};
    use mailrs_domain::invitation::Answer;
    use serde_json::json;

    use super::*;

    const ME: &str = "dana@fastmail.example";
    const INES: &str = "ines@allotment.example";

    fn me() -> Vec<String> {
        vec![ME.to_string()]
    }

    fn ines() -> Vec<String> {
        vec![INES.to_string()]
    }

    fn utc(text: &str) -> EpochMillis {
        DateTime::parse_from_rfc3339(text).unwrap().timestamp_millis()
    }

    fn local(text: &str) -> EpochMillis {
        let naive = NaiveDateTime::parse_from_str(text, LOCAL).unwrap();
        Local.from_local_datetime(&naive).earliest().unwrap().timestamp_millis()
    }

    fn keys(patch: &Map<String, Value>) -> Vec<&str> {
        let mut keys: Vec<&str> = patch.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    fn lisbon_weekly() -> Value {
        json!({
            "@type": "Event", "id": "ev-1", "uid": "rota@allotment.example", "updated": "2026-10-01T10:00:00Z",
            "title": "Allotment rota", "start": "2026-10-06T09:00:00", "timeZone": "Europe/Lisbon", "duration": "PT1H30M",
            "recurrenceRule": {"@type": "RecurrenceRule", "frequency": "weekly", "byDay": [{"@type": "NDay", "day": "tu"}]},
            "recurrenceOverrides": {
                "2026-10-13T09:00:00": {"excluded": true},
                "2026-10-20T09:00:00": {"start": "2026-10-20T10:00:00", "title": "Rota, late"}
            },
            "participants": {
                "p1": {"@type": "Participant", "name": "Ines", "calendarAddress": "mailto:ines@allotment.example", "roles": {"owner": true, "attendee": true}, "participationStatus": "accepted"},
                "p2": {"@type": "Participant", "calendarAddress": "mailto:dana@fastmail.example", "roles": {"attendee": true}, "participationStatus": "tentative"}
            },
            "locations": {"l1": {"@type": "Location", "name": "Shed"}},
            "alerts": {"a1": {"@type": "Alert", "trigger": {"@type": "OffsetTrigger", "offset": "-PT15M"}}}
        })
    }

    /// An event with what the copy cannot show: a room, roles, an answer
    /// it has no word for, a named colour, an HTML description, secret
    /// privacy, a second location and alerts that are not reminders.
    fn seed_swap() -> Value {
        json!({
            "@type": "Event", "id": "ev-9", "uid": "swap@allotment.example", "updated": "2026-10-01T10:00:00Z",
            "title": "Seed swap", "start": "2026-10-06T18:00:00", "timeZone": "Europe/Lisbon", "duration": "PT2H",
            "description": "<p>Bring <b>beans</b></p>", "descriptionContentType": "text/html",
            "color": "red", "privacy": "secret",
            "locations": {"l1": {"@type": "Location", "name": "Hall"}, "l2": {"@type": "Location", "name": "Car park"}},
            "participants": {
                "org": {"@type": "Participant", "name": "Ines", "calendarAddress": "mailto:ines@allotment.example", "roles": {"owner": true, "chair": true}, "participationStatus": "accepted"},
                "room": {"@type": "Participant", "name": "Hall A", "kind": "location", "roles": {"attendee": true}},
                "bo": {"@type": "Participant", "calendarAddress": "MAILTO:Bo@allotment.example?subject=swap", "roles": {"attendee": true, "optional": true}, "participationStatus": "delegated", "sendTo": {"imip": "mailto:bo@allotment.example"}}
            },
            "alerts": {
                "a1": {"@type": "Alert", "trigger": {"@type": "OffsetTrigger", "offset": "-PT30M"}},
                "end": {"@type": "Alert", "trigger": {"@type": "OffsetTrigger", "offset": "-PT5M", "relativeTo": "end"}},
                "at": {"@type": "Alert", "trigger": {"@type": "AbsoluteTrigger", "when": "2026-10-06T08:00:00Z"}},
                "after": {"@type": "Alert", "trigger": {"@type": "OffsetTrigger", "offset": "PT10M"}}
            }
        })
    }

    fn series_of(object: &Value, me: &[String]) -> model::Event {
        events_of(object, "cal-1", me).remove(0)
    }

    #[test]
    fn durations_read_and_write_back() {
        for (text, millis) in [("PT0S", 0), ("PT1H30M", 5_400_000), ("P1D", 86_400_000), ("P1DT2H", 93_600_000), ("-PT10M", -600_000), ("P1W", 604_800_000)] {
            assert_eq!(duration_millis(text), Some(millis), "{text}");
        }
        for millis in [0, 5_400_000, 86_400_000, 93_600_000, -600_000] {
            assert_eq!(duration_millis(&duration_text(millis)), Some(millis));
        }
        assert_eq!(duration_millis("P1M"), None, "a month has no fixed length");
    }

    #[test]
    fn a_duration_past_a_few_years_is_unreadable() {
        assert_eq!(duration_millis("P1000D"), Some(1000 * DAY));
        assert_eq!(duration_millis("P3000D"), None);
        assert_eq!(duration_millis("P99999999999999999999D"), None);
        assert_eq!(duration_millis("PT99999999999999999999999999H"), None);
        let object = json!({"id": "a", "start": "2026-10-06T09:00:00", "timeZone": "Europe/Lisbon", "duration": "P99999999999999999999D"});
        let event = series_of(&object, &[]);
        assert_eq!(event.end, event.start, "an unreadable length reads as none");
    }

    #[test]
    fn a_timed_event_reads_its_zone_length_and_people() {
        let series = series_of(&lisbon_weekly(), &me());
        // 09:00 in Lisbon in October is 08:00 UTC.
        assert_eq!(series.start, utc("2026-10-06T08:00:00Z"));
        assert_eq!(series.end - series.start, 5_400_000);
        assert_eq!(series.zone, "Europe/Lisbon");
        assert_eq!((series.place.as_str(), series.etag.as_str()), ("Shed", "2026-10-01T10:00:00Z"));
        assert_eq!(series.organizer.as_deref(), Some(INES));
        assert_eq!(series.my_answer, Some(Answer::Maybe));
        assert!(series.guests.iter().any(|g| g.me && g.email == ME));
        assert_eq!(series.reminders, Some(vec![Reminder { minutes: 15, method: ReminderMethod::Notification }]));
        assert!(!series.unread_rule);
    }

    #[test]
    fn an_override_becomes_an_occurrence_and_an_exclusion_an_exdate() {
        let events = events_of(&lisbon_weekly(), "cal-1", &me());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].rules, ["RRULE:FREQ=WEEKLY;BYDAY=TU", "EXDATE:20261013T080000Z"]);
        let moved = &events[1];
        let original = utc("2026-10-20T08:00:00Z");
        assert_eq!(moved.series.as_deref(), Some("ev-1"));
        assert_eq!(moved.original_start, Some(original));
        assert_eq!(moved.id, mailrs_domain::calendar::occurrence_id(&events[0], original));
        assert_eq!(moved.title, "Rota, late");
        assert_eq!(moved.start - original, 3_600_000, "an hour later");
        assert!(moved.rules.is_empty());
    }

    #[test]
    fn an_all_day_event_runs_midnight_to_midnight_utc() {
        let object = json!({"id": "ev-2", "title": "Seed fair", "start": "2026-10-17T00:00:00", "showWithoutTime": true, "duration": "P2D"});
        let event = series_of(&object, &[]);
        assert!(event.all_day);
        assert_eq!(event.zone, "UTC");
        assert_eq!(event.start, utc("2026-10-17T00:00:00Z"));
        assert_eq!(event.end - event.start, 2 * DAY);
    }

    #[test]
    fn an_all_day_event_across_a_clock_change_keeps_whole_days() {
        let object = json!({"id": "ev-2", "start": "2026-10-24T00:00:00", "timeZone": "Europe/Lisbon", "showWithoutTime": true, "duration": "P3D"});
        let event = series_of(&object, &[]);
        assert_eq!((event.start, event.end - event.start), (utc("2026-10-24T00:00:00Z"), 3 * DAY));
    }

    #[test]
    fn an_all_day_start_off_midnight_starts_at_midnight() {
        let object = json!({"id": "ev-2", "start": "2026-10-17T10:00:00", "showWithoutTime": true, "duration": "P1D"});
        let event = series_of(&object, &[]);
        assert_eq!((event.start, event.end), (utc("2026-10-17T00:00:00Z"), utc("2026-10-18T00:00:00Z")));
    }

    #[test]
    fn a_day_long_timed_event_keeps_its_clock_time_across_a_clock_change() {
        // Lisbon falls back on 25 October 2026: 09:00 to 09:00 is 25 hours.
        let object = json!({"id": "ev-4", "start": "2026-10-24T09:00:00", "timeZone": "Europe/Lisbon", "duration": "P1D"});
        let event = series_of(&object, &[]);
        assert_eq!((event.start, event.end), (utc("2026-10-24T08:00:00Z"), utc("2026-10-25T09:00:00Z")));
        let written = object_of(&event).unwrap();
        assert_eq!(written["duration"], "P1D");
    }

    #[test]
    fn rfc_8984_shapes_read_too() {
        let object = json!({
            "id": "ev-3", "title": "Old shape", "start": "2026-10-06T09:00:00", "timeZone": "Etc/UTC", "duration": "PT1H",
            "recurrenceRules": [{"@type": "RecurrenceRule", "frequency": "daily", "count": 3}],
            "participants": {"o": {"sendTo": {"imip": "mailto:ines@allotment.example"}, "roles": {"owner": true}}}
        });
        let event = series_of(&object, &[]);
        assert_eq!(event.rules, ["RRULE:FREQ=DAILY;COUNT=3"]);
        assert_eq!(event.organizer.as_deref(), Some(INES));
    }

    #[test]
    fn an_unknown_zone_reads_in_the_computer_s_zone_and_is_never_written() {
        let object = json!({"id": "ev-5", "title": "Mars time", "start": "2026-10-06T09:00:00", "timeZone": "Mars/Olympus", "duration": "PT1H"});
        let event = series_of(&object, &[]);
        assert_eq!(event.start, local("2026-10-06T09:00:00"));
        assert_eq!(event.zone, "Mars/Olympus", "the name stays as the server wrote it");
        let mut renamed = event.clone();
        renamed.title = "Olympus time".into();
        assert_eq!(keys(&patch_of(&renamed, &object, &[]).unwrap()), ["title"]);
        let mut later = event.clone();
        later.start += 3_600_000;
        later.end += 3_600_000;
        let patch = patch_of(&later, &object, &[]).unwrap();
        assert_eq!(patch["start"], "2026-10-06T10:00:00");
        assert!(!patch.contains_key("timeZone"));
    }

    #[test]
    fn a_windows_or_lowercase_zone_name_finds_its_zone() {
        for (name, zone) in [("W. Europe Standard Time", "Europe/Berlin"), ("europe/lisbon", "Europe/Lisbon")] {
            let object = json!({"id": "ev-5", "start": "2026-10-06T09:00:00", "timeZone": name});
            assert_eq!(series_of(&object, &[]).zone, zone, "{name}");
        }
    }

    #[test]
    fn a_floating_time_reads_in_the_computer_s_zone_and_stays_floating() {
        let object = json!({"id": "ev-6", "title": "Water the beans", "start": "2026-10-06T07:00:00", "duration": "PT30M"});
        let event = series_of(&object, &[]);
        assert_eq!(event.start, local("2026-10-06T07:00:00"));
        assert_eq!(event.zone, "");
        let mut later = event.clone();
        later.start += 3_600_000;
        later.end += 3_600_000;
        let patch = patch_of(&later, &object, &[]).unwrap();
        assert_eq!(keys(&patch), ["duration", "start"]);
        assert_eq!(patch["start"], "2026-10-06T08:00:00");
        let new = object_of(&event).unwrap();
        assert!(!new.contains_key("timeZone"), "a new floating event names no zone");
    }

    #[test]
    fn a_start_inside_a_clock_change_gap_moves_forward() {
        // Lisbon skips 01:00 to 02:00 on 29 March 2026; 01:30 reads as 02:30.
        let object = json!({"id": "ev-7", "start": "2026-03-29T01:30:00", "timeZone": "Europe/Lisbon", "duration": "PT1H"});
        let event = series_of(&object, &[]);
        assert_eq!(event.start, utc("2026-03-29T01:30:00Z"));
    }

    #[test]
    fn an_override_and_an_exclusion_inside_a_gap_stay() {
        let object = json!({
            "id": "ev-8", "start": "2026-03-28T01:30:00", "timeZone": "Europe/Lisbon", "duration": "PT1H",
            "recurrenceRule": {"@type": "RecurrenceRule", "frequency": "daily"},
            "recurrenceOverrides": {"2026-03-29T01:30:00": {"title": "Gap"}, "2026-03-30T01:30:00": {"excluded": true}}
        });
        let events = events_of(&object, "cal-1", &[]);
        assert_eq!(events.len(), 2, "the override in the gap is an occurrence");
        assert_eq!(events[1].original_start, Some(utc("2026-03-29T01:30:00Z")));
        let gap = json!({
            "id": "ev-8", "start": "2026-03-28T01:30:00", "timeZone": "Europe/Lisbon",
            "recurrenceRule": {"@type": "RecurrenceRule", "frequency": "daily"},
            "recurrenceOverrides": {"2026-03-29T01:30:00": {"excluded": true}}
        });
        assert_eq!(series_of(&gap, &[]).rules, ["RRULE:FREQ=DAILY", "EXDATE:20260329T013000Z"]);
    }

    #[test]
    fn an_override_that_changes_only_the_zone_keeps_its_instant() {
        let mut object = lisbon_weekly();
        object["recurrenceOverrides"]["2026-10-27T09:00:00"] = json!({"timeZone": "America/New_York"});
        let events = events_of(&object, "cal-1", &me());
        let moved = events.iter().find(|e| e.original_start == Some(utc("2026-10-27T09:00:00Z"))).unwrap();
        assert_eq!(moved.start, utc("2026-10-27T09:00:00Z"));
        assert_eq!(moved.zone, "America/New_York");
    }

    #[test]
    fn a_rule_the_app_cannot_write_is_flagged_and_left_alone() {
        for rule in [
            json!({"@type": "RecurrenceRule", "frequency": "yearly", "rscale": "hebrew"}),
            json!({"@type": "RecurrenceRule", "frequency": "monthly", "skip": "forward"}),
            json!({"@type": "RecurrenceRule", "frequency": "yearly", "byMonth": ["6L"]}),
            json!({"@type": "RecurrenceRule", "frequency": "weekly", "until": "next spring"}),
            json!({"@type": "RecurrenceRule", "frequency": "weekly", "byEaster": [0]}),
            json!({"@type": "RecurrenceRule", "frequency": "weekly", "byDay": [{"@type": "NDay", "day": "tu"}, {"day": "xx"}]}),
            json!({"@type": "RecurrenceRule", "frequency": "monthly", "byMonthDay": [0]}),
            json!("FREQ=WEEKLY"),
        ] {
            let mut object = lisbon_weekly();
            object["recurrenceRule"] = rule.clone();
            let series = series_of(&object, &ines());
            assert!(series.unread_rule, "{rule}");
            assert!(series.rules.is_empty(), "the series shows its first occurrence: {rule}");
            let mut renamed = series.clone();
            renamed.title = "Rota".into();
            let patch = patch_of(&renamed, &object, &ines()).unwrap();
            assert_eq!(keys(&patch), ["title"], "{rule}");
            let mut repeating = series.clone();
            repeating.rules = vec!["RRULE:FREQ=DAILY".into()];
            assert!(matches!(patch_of(&repeating, &object, &ines()), Err(BackendError::Refused(_))), "{rule}");
        }
    }

    #[test]
    fn rule_values_cannot_add_rule_parts() {
        for rule in [
            json!({"@type": "RecurrenceRule", "frequency": "weekly;COUNT=1"}),
            json!({"@type": "RecurrenceRule", "frequency": "weekly", "byDay": [{"@type": "NDay", "day": "tu;COUNT=1"}]}),
            json!({"@type": "RecurrenceRule", "frequency": "weekly", "firstDayOfWeek": "su;COUNT=1"}),
            json!({"@type": "RecurrenceRule", "frequency": "yearly", "byMonth": ["1;COUNT=1"]}),
        ] {
            let mut object = lisbon_weekly();
            object["recurrenceRule"] = rule.clone();
            let series = series_of(&object, &ines());
            assert!(series.unread_rule && series.rules.is_empty(), "{rule}");
        }
        let line = "RRULE:FREQ=WEEKLY;BYDAY=TU;X-NAME=1";
        assert_eq!(rule_object(line, Clock::Utc, false), None);
    }

    #[test]
    fn a_new_event_writes_jscalendar_2_and_reads_back_the_same() {
        let mut series = series_of(&lisbon_weekly(), &me());
        series.guests.retain(|g| g.organizer);
        let object = Value::Object(object_of(&series).unwrap());
        assert_eq!(object["recurrenceRule"]["frequency"], "weekly");
        assert!(object.get("recurrenceRules").is_none());
        assert_eq!(object["participants"]["p1"]["calendarAddress"], "mailto:ines@allotment.example");
        assert_eq!(object["timeZone"], "Europe/Lisbon");
        assert_eq!(object["recurrenceOverrides"]["2026-10-13T09:00:00"]["excluded"], true);
        let mut with_id = object.clone();
        with_id["id"] = json!("ev-1");
        let back = series_of(&with_id, &me());
        assert_eq!((back.start, back.end, &back.rules, back.status), (series.start, series.end, &series.rules, Status::Confirmed));
    }

    /// Cyrus 3.13 refuses a create without `version` (`invalidProperties`),
    /// though draft 32 has the server fill it; JSCalendar 2.0 makes it
    /// mandatory on an event of its own.
    #[test]
    fn a_new_event_names_jscalendar_2() {
        let object = object_of(&series_of(&lisbon_weekly(), &me())).unwrap();
        assert_eq!((&object["@type"], &object["version"]), (&json!("Event"), &json!("2.0")));
    }

    #[test]
    fn an_unchanged_event_patches_nothing() {
        let object = seed_swap();
        let series = series_of(&object, &ines());
        assert!(patch_of(&series, &object, &ines()).unwrap().is_empty());
    }

    #[test]
    fn a_title_edit_keeps_what_the_app_does_not_show() {
        let object = seed_swap();
        let mut series = series_of(&object, &ines());
        assert_eq!(series.description, "<p>Bring <b>beans</b></p>");
        assert_eq!(series.color.as_deref(), Some("#ff0000"), "a named colour reads as its hex");
        assert_eq!(series.reminders, Some(vec![Reminder { minutes: 30, method: ReminderMethod::Notification }]));
        series.title = "Seed and plant swap".into();
        let patch = patch_of(&series, &object, &ines()).unwrap();
        assert_eq!(Value::Object(patch), json!({"title": "Seed and plant swap"}));
    }

    #[test]
    fn a_place_edit_renames_the_one_location_it_came_from() {
        let object = seed_swap();
        let mut series = series_of(&object, &ines());
        series.place = "Barn".into();
        assert_eq!(Value::Object(patch_of(&series, &object, &ines()).unwrap()), json!({"locations/l1/name": "Barn"}));
    }

    #[test]
    fn a_colour_or_privacy_edit_writes_only_itself() {
        let object = seed_swap();
        let mut series = series_of(&object, &ines());
        series.color = Some("#336699".into());
        assert_eq!(Value::Object(patch_of(&series, &object, &ines()).unwrap()), json!({"color": "#336699"}));
        let mut public = series_of(&object, &ines());
        assert!(public.private, "secret reads as private");
        public.private = false;
        assert_eq!(Value::Object(patch_of(&public, &object, &ines()).unwrap()), json!({"privacy": "public"}));
    }

    #[test]
    fn a_reminder_edit_keeps_the_alerts_that_are_not_reminders() {
        let object = seed_swap();
        let mut series = series_of(&object, &ines());
        series.reminders = Some(vec![Reminder { minutes: 60, method: ReminderMethod::Notification }]);
        let patch = patch_of(&series, &object, &ines()).unwrap();
        assert_eq!(patch["alerts/a1"], Value::Null);
        let added: Vec<&str> = keys(&patch).into_iter().filter(|k| *k != "alerts/a1").collect();
        assert_eq!(added.len(), 1, "{patch:?}");
        assert!(added[0].starts_with("alerts/"));
        assert_eq!(patch[added[0]]["trigger"]["offset"], "-PT1H");
        for kept in ["alerts/end", "alerts/at", "alerts/after", "alerts", "useDefaultAlerts"] {
            assert!(!patch.contains_key(kept), "{kept}");
        }
    }

    #[test]
    fn an_alert_relative_to_the_end_is_not_read_as_before_the_start() {
        let object = json!({"id": "ev-9", "start": "2026-10-06T18:00:00", "duration": "PT2H",
            "alerts": {"end": {"@type": "Alert", "trigger": {"@type": "OffsetTrigger", "offset": "-PT5M", "relativeTo": "end"}}}});
        assert_eq!(series_of(&object, &[]).reminders, Some(vec![]));
    }

    #[test]
    fn guests_come_and_go_one_by_one() {
        let object = seed_swap();
        let mut series = series_of(&object, &ines());
        assert!(series.guests.iter().any(|g| g.email == "bo@allotment.example"), "mailto in any case, its query left off");
        assert_eq!(series.guests.len(), 2, "the room has no address and is no guest");
        series.guests.retain(|g| g.email != "bo@allotment.example");
        series.guests.push(Guest { email: "cy@allotment.example".into(), ..Guest::default() });
        let patch = patch_of(&series, &object, &ines()).unwrap();
        assert_eq!(patch["participants/bo"], Value::Null);
        let added: Vec<&str> = keys(&patch).into_iter().filter(|k| *k != "participants/bo").collect();
        assert_eq!(added.len(), 1, "{patch:?}");
        assert_eq!(patch[added[0]]["calendarAddress"], "mailto:cy@allotment.example");
        assert!(!patch.contains_key("participants/room") && !patch.contains_key("participants/org") && !patch.contains_key("participants"));
    }

    #[test]
    fn an_answer_the_app_has_no_word_for_is_not_overwritten() {
        let mut object = seed_swap();
        object["participants"]["bo"]["calendarAddress"] = json!("mailto:dana@fastmail.example");
        let mut series = series_of(&object, &me());
        assert_eq!(series.my_answer, None);
        series.reminders = Some(vec![]);
        let patch = patch_of(&series, &object, &me()).unwrap();
        assert!(patch.keys().all(|k| k.starts_with("alerts/")), "{patch:?}");
    }

    #[test]
    fn my_answer_patches_only_my_status() {
        let object = lisbon_weekly();
        let mut series = series_of(&object, &me());
        series.my_answer = Some(Answer::Yes);
        assert_eq!(Value::Object(patch_of(&series, &object, &me()).unwrap()), json!({"participants/p2/participationStatus": "accepted"}));
    }

    #[test]
    fn a_guest_patches_only_their_own_fields() {
        let object = lisbon_weekly();
        let mut series = series_of(&object, &me());
        assert!(series.limited());
        series.title = "Mine now".into();
        series.busy = false;
        let patch = patch_of(&series, &object, &me()).unwrap();
        assert_eq!(keys(&patch), ["freeBusyStatus"]);
    }

    #[test]
    fn an_organizer_s_patch_clears_what_the_event_lost() {
        let object = lisbon_weekly();
        let mut series = series_of(&object, &ines());
        series.guests.clear();
        series.organizer = None;
        series.place.clear();
        series.rules.retain(|l| !l.starts_with("EXDATE"));
        series.rules.push("EXDATE:20261027T090000Z".into());
        let patch = patch_of(&series, &object, &ines()).unwrap();
        assert_eq!(patch["locations/l1"], Value::Null);
        assert_eq!((&patch["participants/p1"], &patch["participants/p2"]), (&Value::Null, &Value::Null));
        assert_eq!(patch["recurrenceOverrides/2026-10-27T09:00:00"]["excluded"], true, "exclusions go one by one");
        assert_eq!(patch["recurrenceOverrides/2026-10-13T09:00:00"], Value::Null, "a restored occurrence comes back");
        assert!(!patch.contains_key("recurrenceOverrides/2026-10-20T09:00:00"), "the changed occurrences stay");
        assert!(!patch.contains_key("recurrenceRule"));
    }

    #[test]
    fn a_repeat_edit_writes_the_rule() {
        let object = lisbon_weekly();
        let mut series = series_of(&object, &ines());
        series.rules[0] = "RRULE:FREQ=WEEKLY;BYDAY=TU;COUNT=5".into();
        let patch = patch_of(&series, &object, &ines()).unwrap();
        assert_eq!(keys(&patch), ["recurrenceRule"]);
        assert_eq!(patch["recurrenceRule"]["count"], 5);
    }

    #[test]
    fn an_occurrence_edit_carries_only_what_differs_from_the_series() {
        let object = lisbon_weekly();
        let events = events_of(&object, "cal-1", &ines());
        let mut moved = events[1].clone();
        moved.reminders = Some(vec![Reminder { minutes: 5, method: ReminderMethod::Notification }]);
        let over = override_of(&moved, &object, &ines()).unwrap();
        assert_eq!(over["start"], "2026-10-20T10:00:00");
        assert_eq!(over["title"], "Rota, late");
        assert_eq!(over["alerts/a1"], Value::Null);
        for gone in ["description", "locations", "duration", "status"] {
            assert!(over.get(gone).is_none(), "{gone}: {over}");
        }
        let mut back = events[1].clone();
        back.title = "Allotment rota".into();
        let over = override_of(&back, &object, &ines()).unwrap();
        assert_eq!(over, json!({"start": "2026-10-20T10:00:00"}), "the series' own title needs no override");
    }

    #[test]
    fn an_occurrence_without_an_override_gets_one_with_its_change() {
        let object = lisbon_weekly();
        let series = series_of(&object, &ines());
        let original = utc("2026-10-27T09:00:00Z");
        let mut occurrence = series.clone();
        occurrence.id = occurrence_id(&series, original);
        occurrence.series = Some(series.id.clone());
        occurrence.original_start = Some(original);
        occurrence.start = original;
        occurrence.end = original + 5_400_000;
        occurrence.rules.clear();
        occurrence.title = "Rota, swapped".into();
        assert_eq!(override_key(&object, original), "2026-10-27T09:00:00");
        assert_eq!(override_of(&occurrence, &object, &ines()).unwrap(), json!({"title": "Rota, swapped"}));
    }

    #[test]
    fn my_participant_is_found_by_address() {
        assert_eq!(participant_for(&lisbon_weekly(), &[ME.to_uppercase()]).as_deref(), Some("p2"));
        assert_eq!(participant_for(&lisbon_weekly(), &["someone@else.example".into()]), None);
    }

    #[test]
    fn a_patch_path_reaches_into_nested_objects() {
        let mut object = lisbon_weekly();
        set_path(&mut object, "participants/p2/participationStatus", json!("accepted"));
        set_path(&mut object, "recurrenceOverrides/2026-10-27T09:00:00", json!({"excluded": true}));
        set_path(&mut object, "locations", Value::Null);
        assert_eq!(object["participants"]["p2"]["participationStatus"], "accepted");
        assert_eq!(object["recurrenceOverrides"]["2026-10-27T09:00:00"]["excluded"], true);
        assert!(object.get("locations").is_none());
    }

    #[test]
    fn odd_input_reads_safely() {
        let not_a_day = json!({"id": "b", "start": "2026-10-06T09:00:00", "timeZone": "Europe/Lisbon",
            "recurrenceRule": {"frequency": "weekly", "byDay": [{"day": "é"}, {"day": "ñ"}]}});
        assert!(series_of(&not_a_day, &[]).unread_rule);
        let no_start = json!({"id": "c", "start": 5, "participants": 3, "recurrenceOverrides": []});
        assert!(events_of(&no_start, "c", &[]).is_empty());
        let huge_alert = json!({"id": "d", "start": "2026-10-06T09:00:00", "alerts": {"a": {"trigger": {"offset": "-PT99999999999999999999H"}}}});
        assert_eq!(series_of(&huge_alert, &[]).reminders, Some(vec![]));
        assert_eq!(duration_millis("P1e5D"), None);
    }

    #[test]
    fn a_flood_of_overrides_is_cut_short() {
        let overrides: Map<String, Value> = (0..5000)
            .map(|n| (format!("2027-01-01T{:02}:{:02}:{:02}", n / 3600 % 24, n / 60 % 60, n % 60), json!({"title": "x"})))
            .collect();
        let object = json!({"id": "f", "start": "2026-10-06T09:00:00", "recurrenceOverrides": overrides});
        assert!(events_of(&object, "c", &[]).len() <= MOST_OVERRIDES + 1);
    }

    fn gap_series(overrides: Value) -> Value {
        json!({
            "id": "ev-8", "title": "Night watering", "start": "2026-03-28T01:30:00", "timeZone": "Europe/Lisbon", "duration": "PT1H",
            "recurrenceRule": {"@type": "RecurrenceRule", "frequency": "daily", "count": 5},
            "recurrenceOverrides": overrides
        })
    }

    #[test]
    fn an_override_copies_a_bounded_share_of_a_huge_map() {
        let participants: Map<String, Value> = (0..5000)
            .map(|n| (format!("p{n:05}"), json!({"calendarAddress": format!("mailto:g{n}@allotment.example"), "roles": {"attendee": true}})))
            .collect();
        let overrides: Map<String, Value> = (0..1000)
            .map(|n| (format!("2027-01-01T{:02}:{:02}:00", n / 60 % 24, n % 60), json!({"participants/p00001/participationStatus": "declined"})))
            .collect();
        let object = json!({"id": "f", "start": "2026-10-06T09:00:00", "participants": participants, "recurrenceOverrides": overrides});
        let patch = object["recurrenceOverrides"]["2027-01-01T00:00:00"].as_object().unwrap().clone();
        let over = overlay(&object, &patch, utc("2027-01-01T00:00:00Z"));
        assert!(over["participants"].as_object().unwrap().len() <= MOST_LISTED, "the copy stops where reading stops");
        assert_eq!(over["participants"]["p00001"]["participationStatus"], "declined");
        let events = events_of(&object, "c", &[]);
        assert_eq!(events.len(), 1001);
        assert!(events.iter().all(|e| e.guests.len() <= MOST_LISTED));
    }

    #[test]
    fn a_gap_exclusion_keeps_the_server_s_key() {
        let object = gap_series(json!({"2026-03-29T01:30:00": {"excluded": true}}));
        let mut series = series_of(&object, &[]);
        series.rules.retain(|l| model::is_rule_line(l));
        let patch = patch_of(&series, &object, &[]).unwrap();
        assert_eq!(Value::Object(patch), json!({"recurrenceOverrides/2026-03-29T01:30:00": null}));
        assert_eq!(exclusion_key(&object, utc("2026-03-29T01:30:00Z")).unwrap(), "2026-03-29T01:30:00");
    }

    #[test]
    fn a_gap_override_keeps_the_server_s_key() {
        let object = gap_series(json!({"2026-03-29T01:30:00": {"title": "Gap", "description": "Hose"}}));
        let events = events_of(&object, "cal-1", &[]);
        let original = utc("2026-03-29T01:30:00Z");
        assert_eq!(override_key(&object, original), "2026-03-29T01:30:00");
        let mut moved = events[1].clone();
        moved.title = "Gap night".into();
        assert_eq!(override_of(&moved, &object, &[]).unwrap(), json!({"title": "Gap night", "description": "Hose"}));
    }

    #[test]
    fn the_app_s_expander_leaves_out_each_exclusion_at_its_instant() {
        for (object, excluded) in [
            (lisbon_weekly(), utc("2026-10-13T08:00:00Z")),
            (gap_series(json!({"2026-03-29T01:30:00": {"excluded": true}})), utc("2026-03-29T01:30:00Z")),
        ] {
            let series = series_of(&object, &[]);
            let whole = model::Event { rules: series.rules.iter().filter(|l| model::is_rule_line(l)).cloned().collect(), ..series.clone() };
            let (from, to) = (series.start, series.start + 30 * DAY);
            let all: Vec<EpochMillis> = model::expand(&whole, from, to).into_iter().map(|(start, _)| start).collect();
            let kept: Vec<EpochMillis> = model::expand(&series, from, to).into_iter().map(|(start, _)| start).collect();
            let gone: Vec<EpochMillis> = all.iter().copied().filter(|s| !kept.contains(s)).collect();
            assert_eq!(gone, [excluded], "{all:?}");
        }
    }

    #[test]
    fn a_title_edit_through_the_series_editor_leaves_the_rule_alone() {
        use mailrs_domain::calendar::series::{Picked, RepeatScope, Step, change};
        let object = lisbon_weekly();
        let events = events_of(&object, "cal-1", &ines());
        let series = &events[0];
        let picked = Picked { original_start: series.start, start: series.start };
        let edited = model::Event { title: "Rota for all".into(), ..series.clone() };
        let steps = change(series, &events[1..], picked, edited, RepeatScope::All, "new");
        let [Step::Save(saved)] = steps.as_slice() else { panic!("{steps:?}") };
        assert_eq!(saved.rules, series.rules, "the lines stay byte for byte");
        let patch = patch_of(saved, &object, &ines()).unwrap();
        assert_eq!(keys(&patch), ["title"]);
    }

    #[test]
    fn an_occurrence_of_an_unread_rule_cannot_be_deleted() {
        let mut object = lisbon_weekly();
        object["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "yearly", "rscale": "hebrew"});
        let original = utc("2026-10-27T09:00:00Z");
        assert!(matches!(exclusion_key(&object, original), Err(BackendError::Refused(_))));
        let mut series = series_of(&object, &ines());
        series.rules.push("EXDATE:20261027T090000Z".into());
        assert!(matches!(patch_of(&series, &object, &ines()), Err(BackendError::Refused(_))));
    }

    #[test]
    fn a_description_edit_says_its_content_type() {
        let object = seed_swap();
        let mut plain = series_of(&object, &ines());
        plain.description = "Bring beans".into();
        assert_eq!(
            Value::Object(patch_of(&plain, &object, &ines()).unwrap()),
            json!({"description": "Bring beans", "descriptionContentType": "text/plain"})
        );
        let mut lines = series_of(&object, &ines());
        lines.description = "Bring beans<br>and seeds".into();
        assert_eq!(Value::Object(patch_of(&lines, &object, &ines()).unwrap()), json!({"description": "Bring beans<br>and seeds"}));
        let plain_object = lisbon_weekly();
        let mut first = series_of(&plain_object, &ines());
        first.description = "Bring gloves".into();
        assert_eq!(Value::Object(patch_of(&first, &plain_object, &ines()).unwrap()), json!({"description": "Bring gloves"}));
    }

    #[test]
    fn clearing_a_named_colour_sends_null() {
        let object = seed_swap();
        let mut series = series_of(&object, &ines());
        series.color = None;
        assert_eq!(Value::Object(patch_of(&series, &object, &ines()).unwrap()), json!({"color": null}));
    }

    #[test]
    fn a_guest_s_new_name_is_sent() {
        let object = seed_swap();
        let mut series = series_of(&object, &ines());
        let bo = series.guests.iter_mut().find(|g| g.email == "bo@allotment.example").unwrap();
        bo.name = Some("Bo Lin".into());
        assert_eq!(Value::Object(patch_of(&series, &object, &ines()).unwrap()), json!({"participants/bo/name": "Bo Lin"}));
    }

    /// The whole series moved an hour later through the editor's own step,
    /// as "All events" does.
    fn moved_an_hour(object: &Value) -> model::Event {
        use mailrs_domain::calendar::series::{self, Picked, RepeatScope, Step};
        let series = series_of(object, &[]);
        let picked = Picked { original_start: series.start, start: series.start };
        let edited = model::Event { start: series.start + 3_600_000, end: series.end + 3_600_000, ..series.clone() };
        match series::change(&series, &[], picked, edited, RepeatScope::All, "new").as_slice() {
            [Step::Save(moved)] => moved.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn moving_a_series_moves_its_changed_occurrences_with_it() {
        let object = lisbon_weekly();
        let patch = patch_of(&moved_an_hour(&object), &object, &[]).unwrap();
        assert_eq!(patch["start"], "2026-10-06T10:00:00");
        assert_eq!(
            patch["recurrenceOverrides"],
            json!({
                "2026-10-13T10:00:00": {"excluded": true},
                "2026-10-20T10:00:00": {"start": "2026-10-20T10:00:00", "title": "Rota, late"}
            })
        );
        assert!(!patch.keys().any(|k| k.starts_with("recurrenceOverrides/")), "{patch:?}");
    }

    #[test]
    fn a_moved_series_shows_each_changed_occurrence_once() {
        let mut object = lisbon_weekly();
        for (path, value) in patch_of(&moved_an_hour(&object), &object, &[]).unwrap() {
            set_path(&mut object, &path, value);
        }
        let events = events_of(&object, "cal-1", &[]);
        let series = &events[0];
        let late = &events[1];
        assert_eq!(late.original_start, Some(utc("2026-10-20T09:00:00Z")));
        let shown = model::expand(series, utc("2026-10-19T00:00:00Z"), utc("2026-10-21T00:00:00Z"));
        assert_eq!(shown.iter().map(|(start, _)| *start).collect::<Vec<_>>(), [utc("2026-10-20T09:00:00Z")]);
    }
}
