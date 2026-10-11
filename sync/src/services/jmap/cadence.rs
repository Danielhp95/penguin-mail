//! How often a JMAP account looks at its mail. One EventSource stream
//! wakes the engine when the server reports a change; the account then
//! looks every five minutes while the window is open and every fifteen
//! while only the tray runs, in case the stream missed something, as the
//! IMAP adapter keeps a slow poll beside IDLE. A server without push, or
//! whose stream keeps failing, is looked at every minute.

use std::time::Duration;

use mailrs_jmap::Pushed;

use super::{Jmap, JmapApi};

/// How often an account without push looks.
const NO_PUSH_POLL: Duration = Duration::from_secs(60);

/// How long the first failed watch in a row waits before it reopens the
/// stream, so a server that closes it at once costs a reconnect a minute
/// rather than a busy loop.
const WATCH_RETRY: Duration = Duration::from_secs(60);

/// The longest a repeated failure waits: the slow poll's own longest
/// pace.
const WATCH_RETRY_CAP: Duration = Duration::from_secs(15 * 60);

/// The wait before reopening the stream after `failures` failures in a
/// row before this one.
fn watch_retry(failures: u32) -> Duration {
    WATCH_RETRY.saturating_mul(1u32 << failures.min(8)).min(WATCH_RETRY_CAP)
}

fn slow_poll(tray_only: bool) -> Duration {
    match tray_only {
        true => Duration::from_secs(15 * 60),
        false => Duration::from_secs(5 * 60),
    }
}

impl<J: JmapApi> Jmap<J> {
    pub(super) fn poll_every(&self) -> Duration {
        let known = self.known();
        match known.push {
            Some(true) => slow_poll(!known.window_open),
            _ => NO_PUSH_POLL,
        }
    }

    /// Waits until the server says the mail changed. Without push it never
    /// ends. A failed stream ends the watch at once, so the engine looks
    /// at the account, and the next watch waits out a pause, longer after
    /// each failure in a row, before it reopens the stream. The engine
    /// runs one watch at a time, which the client's push lock also
    /// enforces.
    pub(super) async fn watch_mail(&self) {
        let pause = self.known().push_pause.take();
        if let Some(pause) = pause {
            tokio::time::sleep(pause).await;
        }
        match self.api().push().await {
            Ok(Pushed::Changed) => {
                let mut known = self.known();
                known.push = Some(true);
                known.push_failures = 0;
            }
            Ok(Pushed::NoPush) => {
                self.known().push = Some(false);
                std::future::pending::<()>().await;
            }
            Err(_) => {
                let failures = {
                    let mut known = self.known();
                    known.push = Some(false);
                    known.push_pause = Some(watch_retry(known.push_failures));
                    known.push_failures = known.push_failures.saturating_add(1);
                    known.push_failures
                };
                tracing::warn!(failures, "the push stream failed; looking at the account instead");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::watch_retry;

    #[test]
    fn a_watch_that_keeps_failing_waits_longer_each_time_up_to_the_slow_pace() {
        assert_eq!(watch_retry(0), Duration::from_secs(60));
        assert_eq!(watch_retry(1), Duration::from_secs(120));
        assert_eq!(watch_retry(10), Duration::from_secs(15 * 60));
    }
}
