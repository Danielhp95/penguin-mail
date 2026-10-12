//! The calendar of a JMAP account whose session offers JMAP Calendars in
//! draft 32's shape, over JSCalendar. JMAP's change feed covers the whole
//! account while the calendar copy reads one calendar at a time, so every
//! calendar's token is the account's CalendarEvent state, and one
//! `/changes` answer serves each calendar that asks from that state. An
//! event that left a calendar is a removal there and an event in the
//! calendar it went to; the copy removes by calendar and id.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mailrs_domain::EpochMillis;
use mailrs_domain::calendar::{self as model, Access, Notify};
use mailrs_domain::invitation::Answer;
use mailrs_jmap::JmapCalendar;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Jmap, JmapApi, backend, jscal};
use crate::BackendError;
use crate::services::CalendarService;

/// The colour a calendar without one gets: GNOME's blue, as for CalDAV.
const DEFAULT_COLOR: &str = "#3584e4";
/// Events per query page and per `/get` of changed events.
pub(super) const PAGE: usize = 200;
const CHANGES_PER_CALL: usize = 500;
/// Rounds of `/changes` one read follows while the server has more.
const ROUNDS: usize = 20;
/// `updated` stamps of this adapter's own writes kept, newest last.
#[expect(dead_code, reason = "Q6 trims its written stamps to this")]
pub(super) const WRITTEN_KEPT: usize = 256;

/// One read of the account's event changes since a state.
#[derive(Debug, Default)]
pub(crate) struct EventChanges {
    since: String,
    new_state: String,
    objects: Vec<Value>,
    destroyed: Vec<String>,
}

/// Where a whole read stands between pages: the next position, and the
/// state its first page was read at, which becomes the calendar's token.
#[derive(Serialize, Deserialize)]
struct Cursor {
    position: u64,
    #[serde(default)]
    state: String,
}

/// Year 1 and the end of year 9999, the span `chrono` and a server's date
/// parser both take, so an open-ended range still reads as one.
const EARLIEST: EpochMillis = -62_135_596_800_000;
const LATEST: EpochMillis = 253_402_300_799_000;

fn utc(at: EpochMillis) -> String {
    DateTime::<Utc>::from_timestamp_millis(at.clamp(EARLIEST, LATEST))
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn access_of(calendar: &JmapCalendar) -> Access {
    let rights = &calendar.my_rights;
    if rights.may_admin {
        Access::Owner
    } else if rights.may_write_all || rights.may_write_own {
        Access::Writer
    } else if rights.may_read_items {
        Access::Reader
    } else {
        Access::FreeBusy
    }
}

impl<J: JmapApi> Jmap<J> {
    /// The account's own addresses, in lower case, for telling its
    /// participant apart.
    pub(super) fn me(&self) -> Vec<String> {
        let mut me = vec![self.settings().address.to_ascii_lowercase()];
        me.extend(self.known().identities.iter().map(|i| i.email.to_ascii_lowercase()));
        me
    }

    /// One event's JSCalendar object as the server holds it now.
    #[expect(dead_code, reason = "Q6 reads the current object before a patch")]
    pub(super) async fn event_object(&self, id: &str) -> Result<Value, BackendError> {
        self.api()
            .calendar_events(&[id.to_string()])
            .await
            .map_err(backend)?
            .list
            .into_iter()
            .next()
            .ok_or(BackendError::NotFound)
    }

    async fn page_of(&self, calendar: &str, filter: &Value, position: u64, state: Option<String>, with_token: bool) -> Result<model::EventPage, BackendError> {
        let page = self.api().calendar_events_page(filter, position, PAGE).await.map_err(backend)?;
        let state = state.unwrap_or(page.state);
        let me = self.me();
        let mut out = model::EventPage::default();
        for object in &page.events {
            if let Some(id) = object.get("id").and_then(Value::as_str) {
                out.whole_series.push(id.to_string());
            }
            out.events.extend(jscal::events_of(object, calendar, &me));
        }
        let next = position + page.events.len() as u64;
        if !page.events.is_empty() && page.total.is_some_and(|total| next < total) {
            out.next_page = serde_json::to_string(&Cursor { position: next, state }).ok();
        } else if with_token {
            out.next_sync = Some(state);
        }
        Ok(out)
    }

    async fn changes_since(&self, since: &str) -> Result<Arc<EventChanges>, BackendError> {
        let kept = self.known().calendar_changes.clone();
        if let Some(kept) = kept.filter(|k| k.since == since) {
            return Ok(kept);
        }
        let (mut at, mut ids, mut destroyed) = (since.to_string(), Vec::new(), Vec::new());
        for _ in 0..ROUNDS {
            let changes = self.api().calendar_event_changes(&at, CHANGES_PER_CALL).await.map_err(backend)?;
            ids.extend(changes.created);
            ids.extend(changes.updated);
            destroyed.extend(changes.destroyed);
            at = changes.new_state;
            if !changes.has_more_changes {
                break;
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let mut objects = Vec::new();
        for chunk in ids.chunks(PAGE) {
            objects.extend(self.api().calendar_events(chunk).await.map_err(backend)?.list);
        }
        let read = Arc::new(EventChanges { since: since.to_string(), new_state: at, objects, destroyed });
        self.known().calendar_changes = Some(Arc::clone(&read));
        Ok(read)
    }

    async fn changed_in(&self, calendar: &str, since: &str) -> Result<model::EventPage, BackendError> {
        let changes = self.changes_since(since).await?;
        let me = self.me();
        let mut page = model::EventPage::default();
        for object in &changes.objects {
            let Some(id) = object.get("id").and_then(Value::as_str) else {
                continue;
            };
            match object.get("calendarIds").and_then(|c| c.get(calendar)).and_then(Value::as_bool) == Some(true) {
                true => {
                    page.whole_series.push(id.to_string());
                    page.events.extend(jscal::events_of(object, calendar, &me));
                }
                // In another calendar now; removing an id this calendar
                // never held changes nothing.
                false => page.removed.push(id.to_string()),
            }
        }
        page.removed.extend(changes.destroyed.iter().cloned());
        page.next_sync = Some(changes.new_state.clone());
        Ok(page)
    }
}

impl<J: JmapApi> CalendarService for Jmap<J> {
    async fn calendars(&self) -> Result<Vec<model::Calendar>, BackendError> {
        let listed = self.api().calendars().await.map_err(backend)?;
        let names_a_default = listed.iter().any(|c| c.is_default);
        Ok(listed
            .into_iter()
            .enumerate()
            .map(|(index, c)| model::Calendar {
                access: access_of(&c),
                zone: c.time_zone.clone().unwrap_or_default(),
                color: c.color.clone().filter(|color| color.starts_with('#')).unwrap_or_else(|| DEFAULT_COLOR.to_string()),
                primary: if names_a_default { c.is_default } else { index == 0 },
                shown: true,
                hidden: false,
                reminders: Vec::new(),
                name: c.name,
                id: c.id,
            })
            .collect())
    }

    async fn event_changes(&self, calendar: &str, token: Option<&str>, page: Option<&str>, from: EpochMillis) -> Result<model::EventPage, BackendError> {
        let filter = json!({ "inCalendar": calendar, "after": utc(from) });
        if let Some(page) = page {
            let cursor: Cursor = serde_json::from_str(page).map_err(|_| BackendError::StateLost)?;
            return self.page_of(calendar, &filter, cursor.position, Some(cursor.state), true).await;
        }
        match token {
            Some(since) => self.changed_in(calendar, since).await,
            None => self.page_of(calendar, &filter, 0, None, true).await,
        }
    }

    async fn event_range(&self, calendar: &str, from: EpochMillis, to: EpochMillis, page: Option<&str>) -> Result<model::EventPage, BackendError> {
        let filter = json!({ "inCalendar": calendar, "after": utc(from), "before": utc(to) });
        let position = match page {
            Some(page) => serde_json::from_str::<Cursor>(page).map_err(|_| BackendError::StateLost)?.position,
            None => 0,
        };
        self.page_of(calendar, &filter, position, None, false).await
    }

    // The writes come in Q6. Nothing attaches a JMAP calendar to an
    // account before Q7, so no queued change meets these.
    async fn put_event(&self, _event: &model::Event, _etag: Option<&str>, _create: bool, _notify: Notify) -> Result<model::Event, BackendError> {
        Err(BackendError::Unsupported)
    }

    async fn remove_event(&self, _calendar: &str, _id: &str, _etag: Option<&str>, _notify: Notify) -> Result<(), BackendError> {
        Err(BackendError::Unsupported)
    }

    async fn import_event(&self, _event: &model::Event) -> Result<model::Event, BackendError> {
        Err(BackendError::Unsupported)
    }

    async fn upload_attachment(&self, _file: &model::Attachment, _sent: Arc<std::sync::atomic::AtomicU64>) -> Result<model::Attachment, BackendError> {
        Err(BackendError::Unsupported)
    }

    async fn share_file(&self, _file_id: &str, _email: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported)
    }

    async fn move_event(&self, _event: &model::Event, _destination: &str, _notify: Notify) -> Result<model::Event, BackendError> {
        Err(BackendError::Unsupported)
    }

    async fn answer_event(&self, _calendar: &str, _id: &str, _me: &str, _answer: Answer, _note: Option<&str>) -> Result<model::Event, BackendError> {
        Err(BackendError::Unsupported)
    }

    async fn edit_list(&self, _calendar: &str, _edit: &model::list::ListEdit) -> Result<Option<model::Calendar>, BackendError> {
        Err(BackendError::Unsupported)
    }
}
