//! Push and the poll cadence over FakeJmap.

use std::time::Duration;

use super::{fastmail, over};
use crate::MailBackend;
use crate::fake::FakeJmap;

#[tokio::test]
async fn a_change_on_the_server_ends_the_watch_and_the_account_then_looks_slowly() {
    let h = fastmail().await;
    let mail = h.mail();
    let watching = tokio::spawn(async move { mail.watch().await });
    tokio::task::yield_now().await;
    h.fake.poke();
    tokio::time::timeout(Duration::from_secs(2), watching).await.expect("the watch ended").unwrap();
    assert_eq!(h.mail().poll_interval(), Some(Duration::from_secs(15 * 60)), "only the tray runs in a test");
    h.mail().set_window_open(true);
    assert_eq!(h.mail().poll_interval(), Some(Duration::from_secs(5 * 60)));
}

#[tokio::test]
async fn without_push_the_watch_waits_forever_and_the_account_looks_each_minute() {
    let h = over(FakeJmap::new().without_push()).await;
    let ended = tokio::time::timeout(Duration::from_millis(100), h.mail().watch()).await;
    assert!(ended.is_err(), "nothing can wake it");
    assert_eq!(h.mail().poll_interval(), Some(Duration::from_secs(60)));
}

#[tokio::test]
async fn before_the_first_watch_the_account_looks_each_minute() {
    let h = fastmail().await;
    assert_eq!(h.mail().poll_interval(), Some(Duration::from_secs(60)));
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_ends_at_once_is_not_reopened_before_the_backoff() {
    let h = over(FakeJmap::new().with_push_that_ends_at_once()).await;
    let mail = h.mail();
    mail.watch().await;
    assert_eq!(h.fake.push_opens(), 1);
    let again = tokio::spawn({
        let mail = h.mail();
        async move { mail.watch().await }
    });
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert_eq!(h.fake.push_opens(), 1, "still inside the first backoff");
    tokio::time::sleep(Duration::from_secs(2)).await;
    again.await.unwrap();
    assert_eq!(h.fake.push_opens(), 2);
    assert_eq!(h.mail().poll_interval(), Some(Duration::from_secs(60)), "a failing stream leaves the minute poll");
}

#[tokio::test(start_paused = true)]
async fn repeated_failures_wait_longer_before_each_reopening() {
    let h = over(FakeJmap::new().with_push_that_ends_at_once()).await;
    h.mail().watch().await;
    h.mail().watch().await;
    let third = tokio::spawn({
        let mail = h.mail();
        async move { mail.watch().await }
    });
    tokio::time::sleep(Duration::from_secs(119)).await;
    assert_eq!(h.fake.push_opens(), 2, "the second failure doubles the wait");
    tokio::time::sleep(Duration::from_secs(2)).await;
    third.await.unwrap();
    assert_eq!(h.fake.push_opens(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_stream_held_for_a_long_time_before_it_drops_starts_the_backoff_over() {
    let h = over(FakeJmap::new().with_push_that_ends_at_once()).await;
    h.mail().watch().await;
    h.mail().watch().await;
    h.fake.set_push_ends_after(Duration::from_secs(3 * 3600));
    h.mail().watch().await; // waits 120 s, then holds three hours and drops
    h.fake.set_push_ends_after(Duration::ZERO);
    let next = tokio::spawn({
        let mail = h.mail();
        async move { mail.watch().await }
    });
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert_eq!(h.fake.push_opens(), 3, "the pause is the base one, not 240 s");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(h.fake.push_opens(), 4);
    next.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_quiet_stream_that_holds_makes_the_account_look_slowly() {
    let h = fastmail().await;
    let mail = h.mail();
    let watching = tokio::spawn(async move { mail.watch().await });
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(h.mail().poll_interval(), Some(Duration::from_secs(60)), "not yet shown to hold");
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(h.mail().poll_interval(), Some(Duration::from_secs(15 * 60)));
    watching.abort();
}

#[tokio::test(start_paused = true)]
async fn the_account_looks_each_minute_while_a_failed_stream_pauses() {
    let h = over(FakeJmap::new().with_push_that_ends_at_once()).await;
    h.mail().watch().await;
    let mail = h.mail();
    let waiting = tokio::spawn(async move { mail.watch().await });
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(h.mail().poll_interval(), Some(Duration::from_secs(60)));
    waiting.abort();
}

#[tokio::test(start_paused = true)]
async fn a_change_starts_the_backoff_over() {
    let h = over(FakeJmap::new().with_push_that_ends_at_once()).await;
    h.mail().watch().await;
    h.mail().watch().await;
    h.fake.set_push_ends_after(Duration::from_secs(10_000));
    let mail = h.mail();
    let watching = tokio::spawn(async move { mail.watch().await });
    tokio::time::sleep(Duration::from_secs(121)).await;
    h.fake.poke();
    watching.await.unwrap();
    h.fake.set_push_ends_after(Duration::ZERO);
    h.mail().watch().await; // fails at once; the pause is the base one
    let next = tokio::spawn({
        let mail = h.mail();
        async move { mail.watch().await }
    });
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert_eq!(h.fake.push_opens(), 4);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(h.fake.push_opens(), 5);
    next.await.unwrap();
}
