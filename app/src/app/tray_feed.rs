//! The tray's unread counts, applied in the order they were made.

use std::future::Future;

use tokio::sync::watch;

/// Applies the newest value of `values`, starting with the one it holds
/// now, then again each time it changes, one at a time. An older value
/// never lands after a newer one, and the values that come while one is
/// applied give way to the last of them. Ends when the sender goes.
pub(super) async fn follow<T: Clone, F: Future<Output = ()>>(
    mut values: watch::Receiver<T>,
    mut apply: impl FnMut(T) -> F,
) {
    values.mark_changed();
    while values.changed().await.is_ok() {
        let value = values.borrow_and_update().clone();
        apply(value).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    /// A tray that takes a while to take each count, longer for some.
    fn slow_tray(
        seen: &Arc<Mutex<Vec<i64>>>,
    ) -> impl FnMut(i64) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + use<> {
        let seen = Arc::clone(seen);
        move |count| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(if count % 2 == 0 { 30 } else { 5 })).await;
                seen.lock().unwrap().push(count);
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_tray_ends_on_the_last_count_and_never_goes_back() {
        let (counts, values) = watch::channel(0);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let feed = tokio::spawn(follow(values, slow_tray(&seen)));
        for count in 1..=40 {
            counts.send_replace(count);
            if count % 7 == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        counts.send_replace(0);
        drop(counts);
        feed.await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.last(), Some(&0), "{seen:?}");
        let before_last = &seen[..seen.len() - 1];
        assert!(before_last.windows(2).all(|w| w[0] < w[1]), "out of order: {seen:?}");
    }

    #[tokio::test]
    async fn a_count_made_before_the_tray_came_up_is_shown() {
        let (counts, values) = watch::channel(3);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let feed = tokio::spawn(follow(values, slow_tray(&seen)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(counts);
        feed.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), [3]);
    }
}
