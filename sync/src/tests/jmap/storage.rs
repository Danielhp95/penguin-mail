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

#[tokio::test]
async fn email_quota_without_a_separate_primary_reaches_the_storage_report() {
    let h = over(FakeJmap::new().with_capability(QUOTA)).await;
    h.fake.with(|s| {
        s.session.primary_accounts.remove(QUOTA);
        let mut quota = mail_quota(400, 1000, None);
        quota.types = vec!["Email".into()];
        s.quotas = vec![quota];
    });
    assert_eq!(h.mail().storage().await.unwrap(), Storage::new(400, 1000, None));
}

#[tokio::test]
async fn an_imap_quota_fallback_reports_space_when_jmap_lacks_quotas() {
    let h = over(FakeJmap::new()).await;
    let crate::services::AnyMail::Jmap(mail) = h.mail() else { panic!("not JMAP") };
    let mail = mail.with_storage_fallback(std::sync::Arc::new(|_| Box::pin(async {
        Ok(Storage::new(771 * 1024, 48828416 * 1024, None))
    })));
    assert_eq!(mail.storage().await.unwrap(), Storage::new(771 * 1024, 48828416 * 1024, None));
}

#[tokio::test]
async fn jmap_quotas_take_precedence_over_the_imap_fallback() {
    let h = over(FakeJmap::new().with_capability(QUOTA)).await;
    h.fake.with(|s| s.quotas = vec![mail_quota(900, 1000, None)]);
    let crate::services::AnyMail::Jmap(mail) = h.mail() else { panic!("not JMAP") };
    let mail = mail.with_storage_fallback(std::sync::Arc::new(|_| Box::pin(async {
        Err(crate::BackendError::Unsupported)
    })));
    assert_eq!(mail.storage().await.unwrap(), Storage::new(900, 1000, None));
}

#[tokio::test]
async fn an_alias_account_reads_its_quota_as_the_current_session_owner() {
    let h = over(FakeJmap::new()).await;
    h.fake.with(|s| s.session.username = "owner@example.com".into());
    let crate::services::AnyMail::Jmap(mail) = h.mail() else { panic!("not JMAP") };
    let mail = mail.with_storage_fallback(std::sync::Arc::new(|username| Box::pin(async move {
        if username != "owner@example.com" { return Err(crate::BackendError::NeedsReauth); }
        Ok(Storage::new(100, 1000, None))
    })));
    assert_eq!(mail.storage().await.unwrap(), Storage::new(100, 1000, None));
}
