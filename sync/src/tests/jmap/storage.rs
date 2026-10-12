//! How full a JMAP account is, from its quotas.

use mailrs_jmap::{QUOTA, Quota};

use super::over;
use crate::fake::FakeJmap;
use crate::{MailBackend, Storage};

fn mail_quota(used: u64, limit: u64, warn: Option<u64>) -> Quota {
    Quota {
        id: "Q1".into(),
        resource_type: "octets".into(),
        scope: "account".into(),
        used,
        hard_limit: limit,
        warn_limit: warn,
        types: vec!["Mail".into()],
    }
}

#[tokio::test]
async fn storage_comes_from_the_account_byte_quota() {
    let h = over(FakeJmap::new().with_capability(QUOTA)).await;
    h.fake.with(|s| s.quotas = vec![mail_quota(900, 1000, Some(950))]);
    assert_eq!(h.mail().storage().await.unwrap(), Storage::new(900, 1000, Some(950)));
}

#[tokio::test]
async fn a_server_without_quotas_says_nothing() {
    let h = over(FakeJmap::new()).await;
    assert_eq!(h.mail().storage().await.unwrap(), None);
    assert!(h.fake.with(|s| !s.calls.iter().any(|c| c == "Quota/get")), "no call for a capability the session lacks");
}

#[tokio::test]
async fn a_server_that_sets_no_quota_says_nothing() {
    let h = over(FakeJmap::new().with_capability(QUOTA)).await;
    assert_eq!(h.mail().storage().await.unwrap(), None, "Cyrus and Stalwart answer an empty list for a user with no quota");
}
