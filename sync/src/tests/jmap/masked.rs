//! Masked Email over FakeJmap, through the account settings.

use mailrs_jmap::MASKED_EMAIL;

use super::over;
use crate::fake::FakeJmap;
use crate::masked::MaskedState;

#[tokio::test]
async fn a_masked_address_is_made_listed_turned_off_and_deleted() {
    let h = over(FakeJmap::new().with_capability(MASKED_EMAIL)).await;
    assert!(h.sync.services().offers().masked_email);
    let jmap = match &h.sync.services().mail {
        crate::AnyMail::Jmap(adapter) => adapter.clone(),
        _ => unreachable!(),
    };
    let made = jmap.make_masked("https://shop.example", "Shop").await.unwrap();
    assert_eq!(made.state, MaskedState::Enabled);
    jmap.set_masked(&made.id, MaskedState::Disabled).await.unwrap();
    let listed = jmap.masked_addresses().await.unwrap();
    assert_eq!(listed[0].state, MaskedState::Disabled);
    jmap.set_masked(&made.id, MaskedState::Deleted).await.unwrap();
    assert!(jmap.masked_addresses().await.unwrap().is_empty(), "a deleted address is not listed");
}

#[tokio::test]
async fn an_account_without_masked_email_offers_none() {
    let h = over(FakeJmap::new()).await;
    assert!(!h.sync.services().offers().masked_email);
}

#[tokio::test]
async fn only_the_server_sets_an_address_pending() {
    let h = over(FakeJmap::new().with_capability(MASKED_EMAIL)).await;
    let crate::AnyMail::Jmap(jmap) = h.mail() else { unreachable!() };
    let made = jmap.make_masked("https://shop.example", "").await.unwrap();
    assert!(matches!(jmap.set_masked(&made.id, MaskedState::Pending).await, Err(crate::BackendError::Unsupported)));
}

fn settings(h: &super::Harness) -> crate::AccountSettings<crate::tests::Connected> {
    let connected = std::collections::HashMap::from([(h.account_id, std::sync::Arc::clone(&h.sync))]);
    crate::AccountSettings::new(std::sync::Arc::new(crate::tests::Connected(connected)), h.db.clone())
}

#[tokio::test]
async fn the_account_settings_make_and_list_a_masked_address() {
    let h = over(FakeJmap::new().with_capability(MASKED_EMAIL)).await;
    let settings = settings(&h);
    let made = settings.create_masked_address(h.account_id, " shop.example ", "Shop").await.unwrap();
    assert_eq!(made.site, "shop.example", "the site goes without the spaces typed around it");
    assert_eq!(settings.masked_addresses(h.account_id).await.unwrap(), [made]);
}

#[tokio::test]
async fn the_account_settings_refuse_masked_calls_without_masked_email() {
    let h = over(FakeJmap::new()).await;
    let refused = settings(&h).masked_addresses(h.account_id).await;
    assert!(matches!(refused, Err(crate::SyncError::Backend(crate::BackendError::Unsupported))));
}
