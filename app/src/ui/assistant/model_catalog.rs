//! A bounded model-list cache for the current provider connection.

use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::{LocalBoxFuture, Shared};
use mailrs_ai::{ModelList, ProviderConfig};

const MODEL_CACHE_AGE: Duration = Duration::from_secs(5 * 60);
type Request = Shared<LocalBoxFuture<'static, Result<ModelList, String>>>;

#[derive(Default)]
pub(super) struct ModelCache {
    current: Option<(ProviderConfig, Instant, Request)>,
}

impl ModelCache {
    pub fn request(
        &mut self,
        config: ProviderConfig,
        now: Instant,
        load: impl FnOnce() -> LocalBoxFuture<'static, Result<ModelList, String>>,
    ) -> Request {
        if let Some((cached, since, request)) = &self.current
            && *cached == config
            && now.saturating_duration_since(*since) < MODEL_CACHE_AGE
            && !matches!(request.peek(), Some(Err(_)))
        {
            return request.clone();
        }
        let request = load().shared();
        self.current = Some((config, now, request.clone()));
        request
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn config(key: &str) -> ProviderConfig {
        ProviderConfig::Anthropic {
            api_key: key.into(),
            model: "list".into(),
        }
    }

    #[test]
    fn repeated_opens_share_a_request_and_credentials_and_age_invalidate_it() {
        futures::executor::block_on(async {
            let mut cache = ModelCache::default();
            let now = Instant::now();
            let loads = Rc::new(Cell::new(0));
            let load = || {
                let loads = Rc::clone(&loads);
                async move {
                    loads.set(loads.get() + 1);
                    Ok(mailrs_ai::ModelList::new(vec![mailrs_ai::Model::new(
                        "test-model",
                    )]))
                }
                .boxed_local()
            };
            let first = cache.request(config("one"), now, load);
            let second = cache.request(config("one"), now, load);
            assert_eq!(first.await.unwrap(), second.await.unwrap());
            assert_eq!(
                loads.get(),
                1,
                "reopening during a request shares the fetch"
            );
            cache.request(config("one"), now, load).await.unwrap();
            assert_eq!(loads.get(), 1, "reopening a completed list uses the cache");
            cache.request(config("two"), now, load).await.unwrap();
            assert_eq!(
                loads.get(),
                2,
                "changed credentials fetch their own catalog"
            );
            cache
                .request(config("two"), now + MODEL_CACHE_AGE, load)
                .await
                .unwrap();
            assert_eq!(loads.get(), 3, "expired lists refresh");
        });
    }

    #[test]
    fn a_failed_listing_can_be_retried_on_the_next_open() {
        futures::executor::block_on(async {
            let mut cache = ModelCache::default();
            let now = Instant::now();
            assert!(
                cache
                    .request(config("one"), now, || async { Err("offline".into()) }
                        .boxed_local())
                    .await
                    .is_err()
            );
            let result = cache
                .request(config("one"), now, || {
                    async { Ok(mailrs_ai::ModelList::default()) }.boxed_local()
                })
                .await;
            assert!(result.is_ok());
        });
    }
}
