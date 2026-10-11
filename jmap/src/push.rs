//! Push over EventSource (RFC 8620 section 7.3): one long GET whose body
//! is a stream of events. A `state` event names the types whose state
//! moved in each account; a ping keeps a quiet connection alive. The
//! reader caps one event at a megabyte and gives up on a stream that says
//! nothing for the idle limit the caller sets, twice the ping interval.
//!
//! Cyrus answers no other request of the account while a stream is open.
//! So a new stream is followed by one `Core/echo`, which every server
//! answers; when no answer comes within the probe limit the stream is
//! dropped and push stays off for the client's life. A call that stalls
//! later while a stream is open does the same (see `JmapClient::sent`).
//!
//! The open stream holds no request permit. A permit lasts one call, and
//! a stream lives for hours: holding one of the four would starve the
//! account's normal calls.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use crate::blob::expand;
use crate::http::{failure, network};
use crate::request::Request;
use crate::session::{CORE, MAIL};
use crate::{JmapClient, JmapError};

/// How often the server is asked to ping, in seconds.
pub const PING_SECONDS: u32 = 60;

/// The most one event, or the bytes waiting for its end, may take.
pub const MOST_EVENT_BYTES: usize = 1 << 20;

const TYPES: &str = "Email,Mailbox,EmailDelivery";

/// How long the echo sent beside a new stream may take before the server
/// counts as one that holds calls while a stream is open.
const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The longest event name kept. Real names are `state` and `ping`.
const MOST_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StateChange {
    #[serde(default)]
    pub changed: BTreeMap<String, BTreeMap<String, String>>,
}

impl StateChange {
    pub fn touches(&self, account: &str, types: &[&str]) -> bool {
        self.changed.get(account).is_some_and(|moved| moved.keys().any(|t| types.contains(&t.as_str())))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// Something in the mail account changed.
    Changed,
    /// The session names no event source; the account polls.
    NoPush,
}

/// Lines in, state changes out.
#[derive(Debug, Default)]
pub struct Parser {
    event: String,
    data: String,
    /// The event in progress passed the cap; its rest is skipped.
    too_big: bool,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    /// Takes one line without its line ending, and answers a state change
    /// when the line ends one.
    pub fn line(&mut self, line: &str) -> Option<StateChange> {
        if line.is_empty() {
            let (event, data) = (std::mem::take(&mut self.event), std::mem::take(&mut self.data));
            if std::mem::take(&mut self.too_big) || event == "ping" || data.is_empty() {
                return None;
            }
            let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
            let says_state = event == "state" || parsed.get("@type").and_then(|t| t.as_str()) == Some("StateChange");
            return says_state.then(|| serde_json::from_value(parsed).ok()).flatten();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" if value.len() <= MOST_NAME_BYTES => self.event = value.to_string(),
            "data" if !self.too_big => {
                if self.data.len() + value.len() + 1 > MOST_EVENT_BYTES {
                    self.data = String::new();
                    self.too_big = true;
                } else {
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                }
            }
            _ => {}
        }
        None
    }
}

/// An open event stream and the bytes read from it.
pub(crate) struct EventStream {
    response: reqwest::Response,
    pending: Vec<u8>,
    parser: Parser,
}

impl EventStream {
    /// The next state change. Any bytes reset the idle clock, a ping too.
    async fn next(&mut self, idle: Duration) -> Result<StateChange, JmapError> {
        loop {
            while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                let text = String::from_utf8_lossy(&line);
                if let Some(change) = self.parser.line(text.trim_end_matches(['\r', '\n'])) {
                    return Ok(change);
                }
            }
            // A burst near the cap must not stay allocated for hours.
            if self.pending.is_empty() && self.pending.capacity() > 64 << 10 {
                self.pending.shrink_to(64 << 10);
            }
            // Only a line still waiting for its end counts against the cap.
            if self.pending.len() > MOST_EVENT_BYTES {
                return Err(JmapError::AnswerTooLarge { limit: MOST_EVENT_BYTES });
            }
            let chunk = tokio::time::timeout(idle, self.response.chunk())
                .await
                .map_err(|_| JmapError::Network("the event stream went quiet".into()))?
                .map_err(network)?
                .ok_or_else(|| JmapError::Network("the event stream ended".into()))?;
            self.pending.extend_from_slice(&chunk);
        }
    }
}

impl JmapClient {
    async fn open_events(&self, template: &str, idle: Duration) -> Result<EventStream, JmapError> {
        let ping = PING_SECONDS.to_string();
        let url = match template.contains("{types}") {
            true => {
                let url = expand(template, &[("types", TYPES), ("closeafter", "no"), ("ping", &ping)]);
                self.endpoint(&url)?
            }
            false => {
                let mut url = self.endpoint(template)?;
                url.query_pairs_mut().append_pair("types", TYPES).append_pair("closeafter", "no").append_pair("ping", &ping);
                url
            }
        };
        let response = tokio::time::timeout(idle, async {
            self.authorized(reqwest::Method::GET, url)
                .await?
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .send()
                .await
                .map_err(network)
        })
        .await
        .map_err(|_| JmapError::Network("the event stream did not open".into()))??;
        if !response.status().is_success() {
            return Err(failure(response).await);
        }
        Ok(EventStream { response, pending: Vec::new(), parser: Parser::new() })
    }

    /// Waits until the server says the mail account changed. The stream
    /// stays open between calls; one that fails is dropped and opened
    /// again by the next call.
    ///
    /// One task drives this: a second concurrent caller waits on the
    /// stream's lock. After `Network` (the stream ended or went quiet) the
    /// caller must back off before calling again, or a server that closes
    /// at once makes a tight reconnect loop.
    pub async fn push(&self, idle: Duration) -> Result<Pushed, JmapError> {
        if self.push_off() {
            return Ok(Pushed::NoPush);
        }
        let session = self.session().await?;
        let Some(template) = session.event_source_url.clone() else {
            return Ok(Pushed::NoPush);
        };
        let account = session.account_for(MAIL).ok_or_else(|| JmapError::UnknownCapability(MAIL.into()))?.to_string();
        let mut held = self.events().lock().await;
        let close = |held: &mut Option<EventStream>| {
            *held = None;
            self.set_streaming(false);
        };
        loop {
            if self.push_off() {
                close(&mut held);
                return Ok(Pushed::NoPush);
            }
            if held.is_none() {
                *held = Some(self.open_events(&template, idle).await?);
                self.set_streaming(true);
                match self.answers_beside_a_stream().await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::info!("the server holds calls while a push stream is open; polling instead");
                        self.stop_pushing();
                        close(&mut held);
                        return Ok(Pushed::NoPush);
                    }
                    Err(err) => {
                        close(&mut held);
                        return Err(err);
                    }
                }
            }
            let Some(stream) = held.as_mut() else { continue };
            let next = tokio::select! {
                next = stream.next(idle) => next,
                () = self.stream_unwanted() => continue,
            };
            match next {
                Ok(change) if change.touches(&account, &["Email", "Mailbox", "EmailDelivery"]) => return Ok(Pushed::Changed),
                Ok(_) => {}
                Err(err) => {
                    close(&mut held);
                    return Err(err);
                }
            }
        }
    }

    /// Whether the server answers a `Core/echo` within the probe limit
    /// while the new stream is open.
    async fn answers_beside_a_stream(&self) -> Result<bool, JmapError> {
        let mut request = Request::new();
        let call = request.call(CORE, "Core/echo", serde_json::json!({}));
        match tokio::time::timeout(PROBE_LIMIT, self.send(&request)).await {
            Err(_) => Ok(false),
            Ok(answer) => {
                answer?.take::<serde_json::Value>(&call, "Core/echo")?;
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(lines: &[&str]) -> Vec<StateChange> {
        let mut parser = Parser::new();
        lines.iter().filter_map(|l| parser.line(l)).collect()
    }

    #[test]
    fn a_state_event_is_read_at_its_blank_line() {
        let got = fed(&[
            "event: state",
            r#"data: {"@type":"StateChange","changed":{"A1":{"Email":"e2"}}}"#,
            "",
        ]);
        assert_eq!(got.len(), 1);
        assert!(got[0].touches("A1", &["Email", "Mailbox"]));
        assert!(!got[0].touches("A2", &["Email"]));
    }

    #[test]
    fn data_over_several_lines_joins_and_pings_and_comments_are_ignored() {
        let got = fed(&[
            ": a comment",
            "event: ping",
            "data: {\"interval\":60}",
            "",
            "event: state",
            "data: {\"@type\":\"StateChange\",",
            "data: \"changed\":{\"A1\":{\"Mailbox\":\"m2\"}}}",
            "",
        ]);
        assert_eq!(got.len(), 1);
        assert!(got[0].touches("A1", &["Mailbox"]));
        assert!(!got[0].touches("A1", &["Email"]));
    }

    #[test]
    fn an_event_without_a_name_counts_as_state_when_it_says_state_change() {
        let got = fed(&[r#"data: {"@type":"StateChange","changed":{"A1":{"EmailDelivery":"d1"}}}"#, ""]);
        assert!(got[0].touches("A1", &["EmailDelivery"]));
    }

    #[test]
    fn data_that_is_not_json_is_skipped() {
        assert!(fed(&["event: state", "data: {oops", ""]).is_empty());
    }

    #[test]
    fn an_event_past_the_cap_is_dropped_and_the_next_one_still_reads() {
        let mut parser = Parser::new();
        let filler = "x".repeat(64 << 10);
        for _ in 0..(MOST_EVENT_BYTES / filler.len() + 2) {
            assert!(parser.line(&format!("data: {filler}")).is_none());
        }
        assert!(parser.data.len() <= MOST_EVENT_BYTES, "the data kept growing");
        assert!(parser.line("").is_none());
        parser.line("event: state");
        parser.line(r#"data: {"@type":"StateChange","changed":{"A1":{"Email":"e3"}}}"#);
        assert!(parser.line("").is_some_and(|c| c.touches("A1", &["Email"])));
    }
}
