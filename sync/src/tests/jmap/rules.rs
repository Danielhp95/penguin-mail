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

/// Stalwart keeps JMAP's own reply as a script named `vacation`. The
/// rules script must not include it, or that reply keeps answering
/// after the person turns the reply off in Penguin Mail.
async fn rules_beside_the_servers_reply() -> (super::Harness, SieveRules<JmapSieve<AnyJmap>>) {
    let (h, rules) = rules().await;
    (h, rules.beside_server_reply(Some(crate::services::SERVER_REPLY_SCRIPT)))
}

#[tokio::test]
async fn the_servers_reply_script_is_never_included() {
    let (h, rules) = rules_beside_the_servers_reply().await;
    h.fake.with(|s| s.scripts.insert("reply-id".into(), ("vacation".into(), "keep;".into(), true)));
    let err = rules.create_filter(&Filter::block("pest@example.com")).await.unwrap_err();
    assert!(matches!(&err, crate::BackendError::WouldReplace { script } if script == "vacation"), "{err:?}");
    rules.take_over().await;
    rules.create_filter(&Filter::block("pest@example.com")).await.unwrap();
    let ours = h.fake.with(|s| s.scripts.values().find(|(n, _, _)| n == "penguin-mail").cloned()).unwrap();
    assert!(!ours.1.contains("include"), "{}", ours.1);
    assert!(ours.2, "the rules script runs");
}

#[tokio::test]
async fn an_include_of_the_servers_reply_script_is_dropped() {
    let (h, rules) = rules_beside_the_servers_reply().await;
    let written = "require [\"include\"];\ninclude :personal \"vacation\";\n";
    h.fake.with(|s| s.scripts.insert("ours".into(), ("penguin-mail".into(), written.into(), true)));
    rules.create_filter(&Filter::block("pest@example.com")).await.unwrap();
    let ours = h.fake.with(|s| s.scripts.get("ours").cloned()).unwrap();
    assert!(!ours.1.contains("include"), "{}", ours.1);
}

#[tokio::test]
async fn the_reply_stays_on_after_a_rule_is_added() {
    use crate::services::AutoReplyService;
    let (_h, rules) = rules_beside_the_servers_reply().await;
    let away = mailrs_domain::Vacation { enabled: true, subject: "Away".into(), body: "Back on Monday.".into(), ..Default::default() };
    rules.set_vacation(&away).await.unwrap();
    rules.create_filter(&Filter::block("pest@example.com")).await.unwrap();
    let back = rules.vacation().await.unwrap();
    assert!(back.enabled);
    assert_eq!(back.body, "Back on Monday.");
}

/// An account whose sync holds Sieve rules over the fake, as
/// `connect_jmap` attaches them, so folder changes reach the rules.
async fn synced_rules() -> (super::Harness, Arc<crate::AccountSync>, SieveRules<JmapSieve<AnyJmap>>) {
    let h = over(FakeJmap::new().with_sieve(&["fileinto", "vacation", "include", "imap4flags"])).await;
    let services = crate::AccountServices::fake_jmap(Arc::clone(&h.fake));
    let api = Arc::new(JmapSieve::new(Arc::new(AnyJmap::from(Arc::clone(&h.fake)))));
    let rules = SieveRules::new(api, services.mail.clone(), "me@example.com".into(), "Stalwart".into());
    let services = services.with_rules(crate::AnyRules::JmapSieve(rules.clone()));
    let (sender, _events) = async_channel::unbounded();
    let sync = Arc::new(crate::AccountSync::new(h.account_id, services, h.db.clone(), sender));
    (h, sync, rules)
}

fn files_into(id: &str) -> Filter {
    Filter {
        criteria: FilterCriteria { from: Some("travel@example.com".into()), ..FilterCriteria::default() },
        action: FilterAction { add: vec![MailSet::Mailbox(id.to_string())], ..FilterAction::default() },
        ..Filter::default()
    }
}

fn script_text(h: &super::Harness) -> String {
    h.fake.with(|s| s.scripts.values().find(|(n, _, _)| n == "penguin-mail").map(|(_, t, _)| t.clone()).unwrap_or_default())
}

#[tokio::test]
async fn a_folder_renamed_elsewhere_is_followed_by_the_script() {
    let (h, sync, rules) = synced_rules().await;
    let trips = h.fake.add_mailbox("Trips", None);
    let rome = h.fake.add_mailbox("Rome", Some(&trips));
    sync.refresh_labels().await.unwrap();
    rules.create_filter(&files_into(&rome)).await.unwrap();
    h.fake.with(|s| s.mailboxes.get_mut(&rome).unwrap().name = "Roma".into());
    sync.refresh_labels().await.unwrap();
    let text = script_text(&h);
    assert!(text.contains("fileinto \"Trips/Roma\""), "{text}");
    assert!(!text.contains("\"Trips/Rome\""), "{text}");
}

#[tokio::test]
async fn a_folder_renamed_here_is_followed_by_the_script() {
    let (h, sync, rules) = synced_rules().await;
    let rome = h.fake.add_mailbox("Rome", None);
    sync.refresh_labels().await.unwrap();
    rules.create_filter(&files_into(&rome)).await.unwrap();
    sync.rename_label(&rome, "Roma").await.unwrap();
    let text = script_text(&h);
    assert!(text.contains("fileinto \"Roma\""), "{text}");
}

#[tokio::test]
async fn a_deleted_folder_turns_its_rule_off_and_other_saves_still_work() {
    let (h, sync, rules) = synced_rules().await;
    let rome = h.fake.add_mailbox("Rome", None);
    sync.refresh_labels().await.unwrap();
    rules.create_filter(&files_into(&rome)).await.unwrap();
    sync.delete_label(&rome).await.unwrap();
    let listed = rules.filters().await.unwrap();
    assert!(listed[0].off, "{listed:?}");
    assert!(!script_text(&h).contains("fileinto"), "{}", script_text(&h));
    rules.create_filter(&Filter::block("pest@example.com")).await.unwrap();
    let listed = rules.filters().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].off && !listed[1].off, "{listed:?}");
}

#[tokio::test]
async fn a_folder_deleted_elsewhere_turns_its_rule_off_at_the_next_look() {
    let (h, sync, rules) = synced_rules().await;
    let rome = h.fake.add_mailbox("Rome", None);
    sync.refresh_labels().await.unwrap();
    rules.create_filter(&files_into(&rome)).await.unwrap();
    h.fake.with(|s| s.mailboxes.remove(&rome));
    sync.refresh_labels().await.unwrap();
    let listed = rules.filters().await.unwrap();
    assert!(listed[0].off, "{listed:?}");
    rules.create_filter(&Filter::block("pest@example.com")).await.unwrap();
}

#[tokio::test]
async fn a_rule_saved_before_the_first_listing_waits_for_it() {
    let (h, rules) = rules().await;
    let rome = h.fake.add_mailbox("Rome", None);
    rules.create_filter(&files_into(&rome)).await.unwrap();
    assert!(script_text(&h).contains("fileinto \"Rome\""), "{}", script_text(&h));
}

#[tokio::test]
async fn saving_a_rule_into_a_missing_folder_still_says_so() {
    let (_h, rules) = rules().await;
    let err = rules.create_filter(&files_into("mb-gone")).await.unwrap_err();
    assert!(matches!(err, crate::BackendError::Refused(_)), "{err:?}");
}
