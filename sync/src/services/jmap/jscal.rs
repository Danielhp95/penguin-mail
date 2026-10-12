//! JSCalendar as the calendar copy's events, both ways. JSCalendar 2.0
//! (the revision draft-ietf-jmap-calendars-32 cites) is what gets written:
//! one `recurrenceRule`, and a participant's `calendarAddress`. RFC 8984's
//! `recurrenceRules` and `sendTo` are read too, for servers that still
//! send them. A series' changed occurrences become events of their own
//! under `occurrence_id`, and its cancelled ones `EXDATE` lines, which is
//! how the CalDAV adapter hands an iCalendar series to the copy.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use mailrs_domain::EpochMillis;
use mailrs_domain::calendar::{self as model, Guest, Reminder, ReminderMethod, Status, occurrence_id};
use mailrs_domain::invitation::Answer;
use serde_json::{Map, Value, json};

const DAY: i64 = 86_400_000;
/// The most changed or cancelled occurrences read from one object, so a
/// hostile series cannot make the copy build an unbounded list.
const MOST_OVERRIDES: usize = 1000;
/// The most alerts and participants read from one object.
const MOST_LISTED: usize = 500;
/// JSCalendar's LocalDateTime.
const LOCAL: &str = "%Y-%m-%dT%H:%M:%S";

/// An ISO 8601 duration in milliseconds: `PT1H30M`, `P1D`, `P1W`,
/// `-PT10M`. Months and years have no fixed length and answer `None`.
pub(crate) fn duration_millis(text: &str) -> Option<i64> {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total: i64 = 0;
    for (part, units) in [(date, &[('W', 7 * DAY), ('D', DAY)][..]), (time, &[('H', 3_600_000), ('M', 60_000), ('S', 1_000)][..])] {
        let mut number = String::new();
        for ch in part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                number.push(ch);
                continue;
            }
            let (_, size) = units.iter().find(|(unit, _)| *unit == ch)?;
            let value: f64 = number.parse().ok()?;
            if !value.is_finite() {
                return None;
            }
            total = total.saturating_add((value * *size as f64) as i64);
            number.clear();
        }
        if !number.is_empty() {
            return None;
        }
    }
    Some(sign * total)
}

pub(crate) fn duration_text(millis: i64) -> String {
    let sign = if millis < 0 { "-" } else { "" };
    let mut left = millis.unsigned_abs() / 1000;
    let days = left / 86_400;
    left %= 86_400;
    let (hours, minutes, seconds) = (left / 3600, left % 3600 / 60, left % 60);
    let mut out = format!("{sign}P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if left > 0 || days == 0 {
        out.push('T');
        if hours > 0 {
            out.push_str(&format!("{hours}H"));
        }
        if minutes > 0 {
            out.push_str(&format!("{minutes}M"));
        }
        if seconds > 0 || left == 0 {
            out.push_str(&format!("{seconds}S"));
        }
    }
    out
}

fn zone_of(object: &Value) -> Option<Tz> {
    object.get("timeZone")?.as_str()?.parse().ok()
}

/// A LocalDateTime in `zone`; with none, a floating time, read as UTC.
fn local_millis(local: &str, zone: Option<Tz>) -> Option<EpochMillis> {
    let naive = NaiveDateTime::parse_from_str(local, LOCAL).ok()?;
    Some(match zone {
        Some(tz) => tz.from_local_datetime(&naive).earliest()?.timestamp_millis(),
        None => naive.and_utc().timestamp_millis(),
    })
}

fn local_text(at: EpochMillis, zone: Option<Tz>) -> String {
    let utc = DateTime::<Utc>::from_timestamp_millis(at).unwrap_or_default();
    match zone {
        Some(tz) => utc.with_timezone(&tz).format(LOCAL).to_string(),
        None => utc.format(LOCAL).to_string(),
    }
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

fn mailto(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let address = text.strip_prefix("mailto:").or_else(|| text.strip_prefix("MAILTO:")).unwrap_or(text);
    Some(address.trim().to_ascii_lowercase()).filter(|a| a.contains('@'))
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

/// The id of the participant who is the account, by any of `me`.
pub(crate) fn participant_for(object: &Value, me: &[String]) -> Option<String> {
    object.get("participants")?.as_object()?.iter().find_map(|(id, p)| {
        let address = address_of(p)?;
        me.iter().any(|m| m.eq_ignore_ascii_case(&address)).then(|| id.clone())
    })
}

fn rule_value(object: &Value) -> Option<&Value> {
    object
        .get("recurrenceRule")
        .filter(|r| r.is_object())
        .or_else(|| object.get("recurrenceRules").and_then(Value::as_array).and_then(|rules| rules.first()))
}

fn rrule_of(rule: &Value, zone: Option<Tz>, all_day: bool) -> Option<String> {
    let mut parts = vec![format!("FREQ={}", rule.get("frequency")?.as_str()?.to_ascii_uppercase())];
    if let Some(interval) = rule.get("interval").and_then(Value::as_u64).filter(|n| *n > 1) {
        parts.push(format!("INTERVAL={interval}"));
    }
    if let Some(days) = rule.get("byDay").and_then(Value::as_array).filter(|d| !d.is_empty()) {
        let days: Vec<String> = days
            .iter()
            .filter_map(|d| {
                let day = d.get("day")?.as_str()?.to_ascii_uppercase();
                Some(match d.get("nthOfPeriod").and_then(Value::as_i64) {
                    Some(nth) => format!("{nth}{day}"),
                    None => day,
                })
            })
            .collect();
        parts.push(format!("BYDAY={}", days.join(",")));
    }
    for (key, name) in [("byMonthDay", "BYMONTHDAY"), ("byYearDay", "BYYEARDAY"), ("byWeekNo", "BYWEEKNO"), ("bySetPosition", "BYSETPOS"), ("byHour", "BYHOUR"), ("byMinute", "BYMINUTE")] {
        if let Some(values) = rule.get(key).and_then(Value::as_array).filter(|v| !v.is_empty()) {
            let values: Vec<String> = values.iter().filter_map(Value::as_i64).map(|n| n.to_string()).collect();
            parts.push(format!("{name}={}", values.join(",")));
        }
    }
    if let Some(months) = rule.get("byMonth").and_then(Value::as_array).filter(|v| !v.is_empty()) {
        parts.push(format!("BYMONTH={}", months.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")));
    }
    if let Some(first) = rule.get("firstDayOfWeek").and_then(Value::as_str).filter(|d| !d.eq_ignore_ascii_case("mo")) {
        parts.push(format!("WKST={}", first.to_ascii_uppercase()));
    }
    if let Some(count) = rule.get("count").and_then(Value::as_u64) {
        parts.push(format!("COUNT={count}"));
    }
    if let Some(until) = rule.get("until").and_then(Value::as_str) {
        let utc = DateTime::<Utc>::from_timestamp_millis(local_millis(until, zone)?)?;
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
    let day = day.to_ascii_lowercase();
    match nth.trim_start_matches('+') {
        "" => Some(json!({ "@type": "NDay", "day": day })),
        nth => Some(json!({ "@type": "NDay", "day": day, "nthOfPeriod": nth.parse::<i64>().ok()? })),
    }
}

/// An `UNTIL` as JSCalendar's LocalDateTime in the event's zone.
fn until_local(value: &str, zone: Option<Tz>, all_day: bool) -> Option<String> {
    if value.len() == 8 {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        let end_of_day = if all_day { date.and_hms_opt(0, 0, 0)? } else { date.and_hms_opt(23, 59, 59)? };
        return Some(end_of_day.format(LOCAL).to_string());
    }
    let naive = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
    let at = match value.ends_with('Z') {
        true => naive.and_utc().timestamp_millis(),
        false => local_millis(&naive.format(LOCAL).to_string(), zone)?,
    };
    Some(local_text(at, if all_day { None } else { zone }))
}

fn rule_object(line: &str, zone: Option<Tz>, all_day: bool) -> Option<Value> {
    let body = line.split_once(':').map_or(line, |(_, b)| b);
    let mut rule = Map::new();
    rule.insert("@type".into(), json!("RecurrenceRule"));
    for pair in body.split(';') {
        let (key, value) = pair.split_once('=')?;
        let numbers = || value.split(',').filter_map(|n| n.trim().parse::<i64>().ok()).collect::<Vec<_>>();
        let (name, entry) = match key.to_ascii_uppercase().as_str() {
            "FREQ" => ("frequency", json!(value.to_ascii_lowercase())),
            "INTERVAL" => ("interval", json!(value.parse::<u64>().ok()?)),
            "COUNT" => ("count", json!(value.parse::<u64>().ok()?)),
            "BYDAY" => ("byDay", json!(value.split(',').filter_map(day_object).collect::<Vec<_>>())),
            "BYMONTHDAY" => ("byMonthDay", json!(numbers())),
            "BYYEARDAY" => ("byYearDay", json!(numbers())),
            "BYWEEKNO" => ("byWeekNo", json!(numbers())),
            "BYSETPOS" => ("bySetPosition", json!(numbers())),
            "BYHOUR" => ("byHour", json!(numbers())),
            "BYMINUTE" => ("byMinute", json!(numbers())),
            "BYMONTH" => ("byMonth", json!(value.split(',').collect::<Vec<_>>())),
            "WKST" => ("firstDayOfWeek", json!(value.to_ascii_lowercase())),
            "UNTIL" => ("until", json!(until_local(value, zone, all_day)?)),
            _ => continue,
        };
        rule.insert(name.into(), entry);
    }
    rule.contains_key("frequency").then_some(Value::Object(rule))
}

/// The `EXDATE` line for each occurrence the series leaves out.
fn exdates(object: &Value, zone: Option<Tz>, all_day: bool) -> Vec<String> {
    let Some(overrides) = object.get("recurrenceOverrides").and_then(Value::as_object) else {
        return Vec::new();
    };
    overrides
        .iter()
        .take(MOST_OVERRIDES)
        .filter(|(_, patch)| patch.get("excluded").and_then(Value::as_bool) == Some(true))
        .filter_map(|(key, _)| {
            let utc = DateTime::<Utc>::from_timestamp_millis(local_millis(key, zone)?)?;
            Some(match all_day {
                true => format!("EXDATE;VALUE=DATE:{}", utc.format("%Y%m%d")),
                false => format!("EXDATE:{}", utc.format("%Y%m%dT%H%M%SZ")),
            })
        })
        .collect()
}

/// The recurrence ids, as LocalDateTime in the event's zone, that the
/// event's `EXDATE` lines leave out.
fn excluded_keys(rules: &[String], zone: Option<Tz>, all_day: bool) -> Vec<String> {
    let mut keys = Vec::new();
    for line in rules.iter().filter(|l| l.to_ascii_uppercase().starts_with("EXDATE")) {
        let (head, values) = line.split_once(':').unwrap_or((line.as_str(), ""));
        let tzid: Option<Tz> = head.split(';').find_map(|p| p.strip_prefix("TZID=")).and_then(|t| t.parse().ok());
        for value in values.split(',').map(str::trim) {
            if value.len() == 8 {
                if let Ok(date) = NaiveDate::parse_from_str(value, "%Y%m%d") {
                    keys.push(format!("{}T00:00:00", date.format("%Y-%m-%d")));
                }
                continue;
            }
            let Ok(naive) = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S") else {
                continue;
            };
            let at = match (value.ends_with('Z'), tzid) {
                (true, _) => Some(naive.and_utc().timestamp_millis()),
                (false, Some(tz)) => tz.from_local_datetime(&naive).earliest().map(|d| d.timestamp_millis()),
                (false, None) => local_millis(&naive.format(LOCAL).to_string(), zone),
            };
            if let Some(at) = at {
                keys.push(local_text(at, if all_day { None } else { zone }));
            }
        }
    }
    keys
}

fn reminders_of(object: &Value) -> Option<Vec<Reminder>> {
    if object.get("useDefaultAlerts").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let alerts = object.get("alerts").and_then(Value::as_object)?;
    Some(
        alerts
            .values()
            .take(MOST_LISTED)
            .filter_map(|alert| {
                let before = -duration_millis(alert.get("trigger")?.get("offset")?.as_str()?)?;
                (before >= 0).then(|| Reminder {
                    minutes: u32::try_from(before / 60_000).unwrap_or(0),
                    method: match alert.get("action").and_then(Value::as_str) {
                        Some("email") => ReminderMethod::Email,
                        _ => ReminderMethod::Notification,
                    },
                })
            })
            .collect(),
    )
}

/// One event from one JSCalendar object, its overrides left out.
fn one_event(object: &Value, calendar: &str, me: &[String]) -> Option<model::Event> {
    let id = object.get("id")?.as_str()?.to_string();
    let all_day = object.get("showWithoutTime").and_then(Value::as_bool).unwrap_or(false);
    let zone = if all_day { None } else { zone_of(object) };
    let start = local_millis(object.get("start")?.as_str()?, zone)?;
    let length = object.get("duration").and_then(Value::as_str).and_then(duration_millis).unwrap_or(0).max(0);
    let end = start.saturating_add(if all_day { length.max(DAY) } else { length });
    let text = |key: &str| object.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let mine = |address: &str| me.iter().any(|m| m.eq_ignore_ascii_case(address));
    let guests: Vec<Guest> = object
        .get("participants")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|p| p.values().take(MOST_LISTED))
        .filter_map(|p| {
            let email = address_of(p)?;
            Some(Guest {
                me: mine(&email),
                name: p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string),
                answer: p.get("participationStatus").and_then(Value::as_str).and_then(answer_of),
                organizer: has_role(p, "owner"),
                email,
            })
        })
        .collect();
    let organizer = guests
        .iter()
        .find(|g| g.organizer)
        .map(|g| g.email.clone())
        .or_else(|| object.get("replyTo").and_then(|r| r.get("imip")).and_then(mailto));
    let my_answer = guests.iter().find(|g| g.me && !g.organizer).and_then(|g| g.answer);
    let place = object
        .get("locations")
        .and_then(Value::as_object)
        .and_then(|l| l.values().find_map(|v| v.get("name").and_then(Value::as_str)))
        .unwrap_or_default()
        .to_string();
    let conference = object
        .get("virtualLocations")
        .and_then(Value::as_object)
        .and_then(|v| v.values().find_map(|l| l.get("uri").and_then(Value::as_str)))
        .map(str::to_string);
    let mut rules: Vec<String> = rule_value(object).and_then(|r| rrule_of(r, zone, all_day)).into_iter().collect();
    rules.extend(exdates(object, zone, all_day));
    Some(model::Event {
        calendar: calendar.to_string(),
        id,
        uid: text("uid"),
        etag: text("updated"),
        start,
        end,
        zone: zone.map_or_else(|| "UTC".to_string(), |tz| tz.name().to_string()),
        all_day,
        title: text("title"),
        place,
        description: text("description"),
        color: object.get("color").and_then(Value::as_str).filter(|c| c.starts_with('#')).map(str::to_string),
        busy: object.get("freeBusyStatus").and_then(Value::as_str) != Some("free"),
        status: Status::parse(object.get("status").and_then(Value::as_str).unwrap_or("confirmed")),
        private: matches!(object.get("privacy").and_then(Value::as_str), Some("private" | "secret")),
        organizer,
        guests,
        my_answer,
        reminders: reminders_of(object),
        conference,
        rules,
        sequence: object.get("sequence").and_then(Value::as_i64).unwrap_or(0),
        ..model::Event::default()
    })
}

/// The event `object` holds, then one per changed occurrence of it.
pub(crate) fn events_of(object: &Value, calendar: &str, me: &[String]) -> Vec<model::Event> {
    let Some(series) = one_event(object, calendar, me) else {
        return Vec::new();
    };
    let zone = if series.all_day { None } else { zone_of(object) };
    let mut events = Vec::new();
    for (key, patch) in object.get("recurrenceOverrides").and_then(Value::as_object).into_iter().flatten().take(MOST_OVERRIDES) {
        if patch.get("excluded").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(original) = local_millis(key, zone) else {
            continue;
        };
        let mut merged = object.clone();
        if let Some(fields) = merged.as_object_mut() {
            for gone in ["recurrenceOverrides", "recurrenceRule", "recurrenceRules"] {
                fields.remove(gone);
            }
            fields.insert("start".into(), json!(key));
        }
        for (path, value) in patch.as_object().into_iter().flatten() {
            set_path(&mut merged, path, value.clone());
        }
        if let Some(mut occurrence) = one_event(&merged, calendar, me) {
            occurrence.id = occurrence_id(&series, original);
            occurrence.series = Some(series.id.clone());
            occurrence.original_start = Some(original);
            events.push(occurrence);
        }
    }
    events.insert(0, series);
    events
}

/// Applies one patch entry (RFC 8620 section 5.3): `path` names a key, or
/// keys inside objects joined by `/` (`~1` and `~0` escaped), and `null`
/// takes the key out.
pub(crate) fn set_path(target: &mut Value, path: &str, value: Value) {
    let parts: Vec<String> = path.split('/').map(|p| p.replace("~1", "/").replace("~0", "~")).collect();
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

fn zone_for(event: &model::Event) -> Option<Tz> {
    if event.all_day {
        return None;
    }
    event.zone.parse().ok()
}

fn participants_of(event: &model::Event) -> Value {
    let mut out = Map::new();
    let mut add = |email: &str, name: Option<&str>, owner: bool, answer: Option<Answer>| {
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
        let key = format!("p{}", out.len() + 1);
        out.insert(key, participant);
    };
    for guest in &event.guests {
        add(&guest.email, guest.name.as_deref(), guest.organizer, guest.answer);
    }
    if let Some(organizer) = &event.organizer
        && !event.guests.iter().any(|g| g.email.eq_ignore_ascii_case(organizer))
    {
        add(organizer, None, true, Some(Answer::Yes));
    }
    Value::Object(out)
}

/// Every field the app edits, as JSCalendar.
fn fields_of(event: &model::Event) -> Map<String, Value> {
    let zone = zone_for(event);
    let mut fields = Map::new();
    fields.insert("title".into(), json!(event.title));
    fields.insert("description".into(), json!(event.description));
    fields.insert("start".into(), json!(local_text(event.start, zone)));
    fields.insert(
        "timeZone".into(),
        match (event.all_day, zone) {
            (true, _) => Value::Null,
            (false, Some(tz)) => json!(tz.name()),
            (false, None) => json!("Etc/UTC"),
        },
    );
    fields.insert("showWithoutTime".into(), json!(event.all_day));
    fields.insert("duration".into(), json!(duration_text(event.end.saturating_sub(event.start).max(0))));
    fields.insert("status".into(), json!(event.status.as_str()));
    fields.insert("freeBusyStatus".into(), json!(if event.busy { "busy" } else { "free" }));
    fields.insert("privacy".into(), json!(if event.private { "private" } else { "public" }));
    if !event.place.is_empty() {
        fields.insert("locations".into(), json!({ "l1": { "@type": "Location", "name": event.place } }));
    }
    if let Some(link) = &event.conference {
        fields.insert("virtualLocations".into(), json!({ "v1": { "@type": "VirtualLocation", "uri": link } }));
    }
    if let Some(color) = &event.color {
        fields.insert("color".into(), json!(color));
    }
    if !event.guests.is_empty() || event.organizer.is_some() {
        fields.insert("participants".into(), participants_of(event));
    }
    if let Some(rule) = event.rules.iter().find(|l| model::is_rule_line(l)).and_then(|l| rule_object(l, zone, event.all_day)) {
        fields.insert("recurrenceRule".into(), rule);
    }
    let excluded: Map<String, Value> =
        excluded_keys(&event.rules, zone, event.all_day).into_iter().map(|key| (key, json!({ "excluded": true }))).collect();
    if !excluded.is_empty() {
        fields.insert("recurrenceOverrides".into(), Value::Object(excluded));
    }
    match &event.reminders {
        None => {
            fields.insert("useDefaultAlerts".into(), json!(true));
        }
        Some(reminders) => {
            fields.insert("useDefaultAlerts".into(), json!(false));
            let alerts: Map<String, Value> = reminders
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let action = match r.method {
                        ReminderMethod::Email => "email",
                        ReminderMethod::Notification => "display",
                    };
                    let offset = duration_text(-i64::from(r.minutes) * 60_000);
                    (format!("a{}", i + 1), json!({
                        "@type": "Alert",
                        "trigger": { "@type": "OffsetTrigger", "offset": offset, "relativeTo": "start" },
                        "action": action
                    }))
                })
                .collect();
            fields.insert("alerts".into(), Value::Object(alerts));
        }
    }
    fields
}

/// A new event, without `calendarIds`, which the adapter adds.
pub(crate) fn object_of(event: &model::Event) -> Map<String, Value> {
    let mut object = Map::new();
    object.insert("@type".into(), json!("Event"));
    if !event.uid.is_empty() {
        object.insert("uid".into(), json!(event.uid));
    }
    object.extend(fields_of(event));
    object
}

/// The patch that makes the server's event match `event`. Each field
/// replaces the server's; a field the event no longer has goes as null.
/// Exclusions are added one by one, so occurrences changed elsewhere stay.
/// A guest changes only what belongs to them.
pub(crate) fn patch_of(event: &model::Event) -> Map<String, Value> {
    let fields = fields_of(event);
    if event.limited() {
        return ["alerts", "useDefaultAlerts", "freeBusyStatus", "color"]
            .into_iter()
            .map(|key| (key.to_string(), fields.get(key).cloned().unwrap_or(Value::Null)))
            .collect();
    }
    let mut patch = fields;
    for key in ["locations", "virtualLocations", "participants", "recurrenceRule", "alerts", "color"] {
        patch.entry(key).or_insert(Value::Null);
    }
    if let Some(Value::Object(excluded)) = patch.remove("recurrenceOverrides") {
        for (key, value) in excluded {
            patch.insert(format!("recurrenceOverrides/{key}"), value);
        }
    }
    patch
}

/// What one changed occurrence overrides in its series.
pub(crate) fn override_of(event: &model::Event) -> Value {
    let zone = zone_for(event);
    let locations = match event.place.is_empty() {
        true => Value::Null,
        false => json!({ "l1": { "@type": "Location", "name": event.place } }),
    };
    json!({
        "start": local_text(event.start, zone),
        "duration": duration_text(event.end.saturating_sub(event.start).max(0)),
        "title": event.title,
        "description": event.description,
        "status": event.status.as_str(),
        "locations": locations
    })
}

/// The recurrence id of the occurrence that started at `original`, in the
/// series' zone.
pub(crate) fn override_key(event: &model::Event, original: EpochMillis) -> String {
    local_text(original, zone_for(event))
}

#[cfg(test)]
mod tests {
    use mailrs_domain::calendar::{Reminder, ReminderMethod, Status};
    use mailrs_domain::invitation::Answer;
    use serde_json::json;

    use super::*;

    const ME: &str = "dana@fastmail.example";

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
    fn a_timed_event_reads_its_zone_length_and_people() {
        let events = events_of(&lisbon_weekly(), "cal-1", &[ME.to_string()]);
        let series = &events[0];
        // 09:00 in Lisbon in October is 08:00 UTC.
        assert_eq!(series.start, chrono::DateTime::parse_from_rfc3339("2026-10-06T08:00:00Z").unwrap().timestamp_millis());
        assert_eq!(series.end - series.start, 5_400_000);
        assert_eq!(series.zone, "Europe/Lisbon");
        assert_eq!((series.place.as_str(), series.etag.as_str()), ("Shed", "2026-10-01T10:00:00Z"));
        assert_eq!(series.organizer.as_deref(), Some("ines@allotment.example"));
        assert_eq!(series.my_answer, Some(Answer::Maybe));
        assert!(series.guests.iter().any(|g| g.me && g.email == ME));
        assert_eq!(series.reminders, Some(vec![Reminder { minutes: 15, method: ReminderMethod::Notification }]));
    }

    #[test]
    fn an_override_becomes_an_occurrence_and_an_exclusion_an_exdate() {
        let events = events_of(&lisbon_weekly(), "cal-1", &[ME.to_string()]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].rules, ["RRULE:FREQ=WEEKLY;BYDAY=TU", "EXDATE:20261013T080000Z"]);
        let moved = &events[1];
        let original = chrono::DateTime::parse_from_rfc3339("2026-10-20T08:00:00Z").unwrap().timestamp_millis();
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
        let event = &events_of(&object, "cal-1", &[])[0];
        assert!(event.all_day);
        assert_eq!(event.zone, "UTC");
        assert_eq!(event.start, chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis());
        assert_eq!(event.end - event.start, 2 * 86_400_000);
    }

    #[test]
    fn rfc_8984_shapes_read_too() {
        let object = json!({
            "id": "ev-3", "title": "Old shape", "start": "2026-10-06T09:00:00", "timeZone": "Etc/UTC", "duration": "PT1H",
            "recurrenceRules": [{"@type": "RecurrenceRule", "frequency": "daily", "count": 3}],
            "participants": {"o": {"sendTo": {"imip": "mailto:ines@allotment.example"}, "roles": {"owner": true}}}
        });
        let event = &events_of(&object, "cal-1", &[])[0];
        assert_eq!(event.rules, ["RRULE:FREQ=DAILY;COUNT=3"]);
        assert_eq!(event.organizer.as_deref(), Some("ines@allotment.example"));
    }

    #[test]
    fn a_new_event_writes_jscalendar_2_and_reads_back_the_same() {
        let mut series = events_of(&lisbon_weekly(), "cal-1", &[ME.to_string()]).remove(0);
        series.guests.retain(|g| g.organizer);
        let object = Value::Object(object_of(&series));
        assert_eq!(object["recurrenceRule"]["frequency"], "weekly");
        assert!(object.get("recurrenceRules").is_none());
        assert_eq!(object["participants"]["p1"]["calendarAddress"], "mailto:ines@allotment.example");
        assert_eq!(object["timeZone"], "Europe/Lisbon");
        assert_eq!(object["recurrenceOverrides"]["2026-10-13T09:00:00"]["excluded"], true);
        let mut with_id = object.clone();
        with_id["id"] = json!("ev-1");
        let back = &events_of(&with_id, "cal-1", &[ME.to_string()])[0];
        assert_eq!((back.start, back.end, &back.rules, back.status), (series.start, series.end, &series.rules, Status::Confirmed));
    }

    #[test]
    fn a_guest_patches_only_their_own_fields() {
        let series = events_of(&lisbon_weekly(), "cal-1", &[ME.to_string()]).remove(0);
        assert!(series.limited());
        let patch = patch_of(&series);
        let mut keys: Vec<&str> = patch.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["alerts", "color", "freeBusyStatus", "useDefaultAlerts"]);
    }

    #[test]
    fn an_organizer_s_patch_clears_what_the_event_lost() {
        let mut series = events_of(&lisbon_weekly(), "cal-1", &[ME.to_string()]).remove(0);
        series.guests.clear();
        series.organizer = None;
        series.place.clear();
        let patch = patch_of(&series);
        assert_eq!(patch["locations"], Value::Null);
        assert_eq!(patch["participants"], Value::Null);
        assert_eq!(patch["recurrenceOverrides/2026-10-13T09:00:00"]["excluded"], true, "exclusions go one by one");
        assert!(!patch.contains_key("recurrenceOverrides"), "the changed occurrences stay");
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
}

#[cfg(test)]
mod odd_input {
    use serde_json::json;

    use super::*;

    #[test]
    fn odd_input_does_not_panic() {
        for object in [
            json!({"id": "a", "start": "2026-10-06T09:00:00", "timeZone": "Not/AZone", "duration": "P99999999999999999999D"}),
            json!({"id": "b", "start": "2026-03-29T01:30:00", "timeZone": "Europe/Lisbon", "recurrenceRule": {"frequency": "weekly", "byDay": [{"day": "é"}, {"day": "ñ"}]}}),
            json!({"id": "c", "start": 5, "participants": 3, "recurrenceOverrides": []}),
            json!({"id": "d", "start": "2026-10-06T09:00:00", "alerts": {"a": {"trigger": {"offset": "-PT99999999999999999999H"}}}}),
        ] {
            let _ = events_of(&object, "c", &[]);
        }
        assert_eq!(duration_millis("P1e5D"), None);
        assert!(duration_millis("PT99999999999999999999999999H").is_some());
    }

    #[test]
    fn a_flood_of_overrides_is_cut_short() {
        let overrides: Map<String, Value> = (0..5000)
            .map(|n| (format!("2027-01-01T{:02}:{:02}:{:02}", n / 3600 % 24, n / 60 % 60, n % 60), json!({"title": "x"})))
            .collect();
        let object = json!({"id": "f", "start": "2026-10-06T09:00:00", "recurrenceOverrides": overrides});
        assert!(events_of(&object, "c", &[]).len() <= MOST_OVERRIDES + 1);
    }
}
