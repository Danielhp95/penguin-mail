//! Rules as Sieve over FakeJmap.

use std::sync::Arc;

use mailrs_domain::{Filter, FilterAction, FilterCriteria, MailSet};

use super::over;
use crate::fake::FakeJmap;
use crate::services::{AnyJmap, JmapSieve, MailBackend, RulesService, SieveRules};

async fn rules() -> (super::Harness, SieveRules<JmapSieve<AnyJmap>>) {
    let h = over(FakeJmap::new().with_sieve(&["fileinto", "vacation", "include", "imap4flags"])).await;
    let api = Arc::new(JmapSieve::new(Arc::new(AnyJmap::from(Arc::clone(&h.fake)))));
    let rules = SieveRules::new(api, h.sync.services().mail.clone(), "me@example.com".into(), "Stalwart".into());
    (h, rules)
}

#[tokio::test]
async fn a_rule_is_written_and_read_back_and_its_script_runs() {
    let (h, rules) = rules().await;
    rules.create_filter(&Filter::block("pest@example.com")).await.unwrap();
    assert_eq!(rules.filters().await.unwrap().len(), 1);
    let active = h.fake.with(|s| s.scripts.values().filter(|(_, _, a)| *a).count());
    assert_eq!(active, 1);
}

#[tokio::test]
async fn a_rule_files_into_the_mailbox_path_not_its_id() {
    let (h, rules) = rules().await;
    let trips = h.fake.add_mailbox("Trips", None);
    let rome = h.fake.add_mailbox("Rome", Some(&trips));
    h.sync.services().mail.mailboxes().await.unwrap();
    let filter = Filter {
        criteria: FilterCriteria { from: Some("travel@example.com".into()), ..FilterCriteria::default() },
        action: FilterAction { add: vec![MailSet::Mailbox(rome.clone())], ..FilterAction::default() },
        ..Filter::default()
    };
    rules.create_filter(&filter).await.unwrap();
    let text = h.fake.with(|s| s.scripts.values().next().unwrap().1.clone());
    // The rule's own comment keeps the id so the app reads the rule back;
    // the command the server runs names the path.
    let fileinto = text.lines().find(|l| l.trim_start().starts_with("fileinto")).unwrap();
    assert!(fileinto.contains("\"Trips/Rome\""), "{text}");
    assert!(!fileinto.contains(&rome), "{text}");
}

#[tokio::test]
async fn another_clients_active_script_stays_and_is_included() {
    let (h, rules) = rules().await;
    h.fake.with(|s| s.scripts.insert("theirs-id".into(), ("work".into(), "keep;".into(), true)));
    rules.create_filter(&Filter::block("pest@example.com")).await.unwrap();
    let kept = h.fake.with(|s| s.scripts.get("theirs-id").cloned()).unwrap();
    assert_eq!(kept.1, "keep;");
    let ours = h.fake.with(|s| s.scripts.values().find(|(n, _, _)| n == "penguin-mail").cloned()).unwrap();
    assert!(ours.1.contains("include"), "{}", ours.1);
}

#[tokio::test]
async fn without_include_another_clients_script_is_not_replaced_until_taken_over() {
    let h = over(FakeJmap::new().with_sieve(&["fileinto"])).await;
    let api = Arc::new(JmapSieve::new(Arc::new(AnyJmap::from(Arc::clone(&h.fake)))));
    let rules = SieveRules::new(api, h.sync.services().mail.clone(), "me@example.com".into(), "Stalwart".into());
    h.fake.with(|s| s.scripts.insert("theirs-id".into(), ("work".into(), "keep;".into(), true)));
    let err = rules.create_filter(&Filter::block("pest@example.com")).await.unwrap_err();
    assert!(matches!(err, crate::BackendError::WouldReplace { .. }), "{err:?}");
    assert!(h.fake.with(|s| s.scripts.get("theirs-id").is_some_and(|(_, _, a)| *a)));
}
