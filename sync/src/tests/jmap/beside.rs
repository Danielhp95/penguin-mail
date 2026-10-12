//! Local rules beside a JMAP server's Sieve script, as an account switched
//! from IMAP keeps them until they move into the script.

use std::sync::Arc;

use mailrs_domain::{Filter, Vacation};

use super::{Harness, over};
use crate::fake::FakeJmap;
use crate::services::{AnyJmap, AutoReplyService, JmapSieve, LocalRules, RulesBeside, RulesService, SieveRules};

async fn beside(fake: FakeJmap) -> (Harness, RulesBeside) {
    let h = over(fake).await;
    let api = Arc::new(JmapSieve::new(Arc::new(AnyJmap::from(Arc::clone(&h.fake)))));
    let server = SieveRules::new(api, h.mail(), "me@example.com".into(), "Stalwart".into());
    let local = LocalRules::new(h.db.clone(), h.account_id);
    (h, RulesBeside::new(server, local))
}

fn includes() -> FakeJmap {
    FakeJmap::new().with_sieve(&["fileinto", "vacation", "include", "imap4flags"])
}

/// A server that cannot include, with a script of the person's own running.
fn theirs_runs() -> FakeJmap {
    let fake = FakeJmap::new().with_sieve(&["fileinto"]);
    fake.with(|s| s.scripts.insert("theirs-id".into(), ("work".into(), "keep;".into(), true)));
    fake
}

#[tokio::test]
async fn local_rules_move_into_the_script_and_leave_the_table() {
    let (_h, rules) = beside(includes()).await;
    rules.local().create_filter(&Filter::block("pest@example.com")).await.unwrap();
    rules.move_in().await.unwrap();
    assert_eq!(rules.server().filters().await.unwrap().len(), 1, "the script holds the rule");
    assert!(rules.local().filters().await.unwrap().is_empty(), "nothing runs here twice");
}

#[tokio::test]
async fn moving_again_after_a_crash_adds_no_second_copy() {
    let (_h, rules) = beside(includes()).await;
    rules.local().create_filter(&Filter::block("pest@example.com")).await.unwrap();
    rules.move_in().await.unwrap();
    // The table write after the script's was lost.
    rules.local().create_filter(&Filter::block("pest@example.com")).await.unwrap();
    rules.move_in().await.unwrap();
    assert_eq!(rules.server().filters().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_automatic_reply_in_the_script_stays_when_rules_move_in() {
    let (_h, rules) = beside(includes()).await;
    let away = Vacation { enabled: true, subject: "Away".into(), body: "Back on Monday".into(), ..Vacation::default() };
    rules.server().set_vacation(&away).await.unwrap();
    rules.local().create_filter(&Filter::block("pest@example.com")).await.unwrap();
    rules.move_in().await.unwrap();
    assert!(rules.server().vacation().await.unwrap().enabled);
}

#[tokio::test]
async fn a_script_of_the_persons_own_keeps_the_rules_here_beside_it() {
    let (h, rules) = beside(theirs_runs()).await;
    rules.local().create_filter(&Filter::block("pest@example.com")).await.unwrap();
    let refused = rules.move_in().await.unwrap_err();
    assert!(matches!(refused, crate::BackendError::WouldReplace { .. }), "{refused:?}");
    assert_eq!(rules.local().filters().await.unwrap().len(), 1, "the rule still runs here");
    assert!(h.fake.with(|s| s.scripts.get("theirs-id").is_some_and(|(_, _, active)| *active)));
    assert_eq!(rules.filters().await.unwrap().len(), 1, "the Rules dialog lists it");
    assert_eq!(rules.kept_here().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_rule_kept_here_is_changed_here_and_a_new_one_goes_to_the_server() {
    let (_h, rules) = beside(includes()).await;
    let here = rules.local().create_filter(&Filter::block("pest@example.com")).await.unwrap();
    let there = rules.create_filter(&Filter::block("spam@example.com")).await.unwrap();
    assert_eq!(rules.server().filters().await.unwrap().len(), 1, "a new rule goes to the server");
    assert_eq!(rules.filters().await.unwrap().len(), 2, "both are listed");
    let edited = rules.replace_filter(here.id.as_deref().unwrap(), &Filter::block("pests@example.com")).await.unwrap();
    assert_eq!(rules.local().filters().await.unwrap(), vec![edited.clone()]);
    rules.delete_filter(edited.id.as_deref().unwrap()).await.unwrap();
    assert!(rules.local().filters().await.unwrap().is_empty());
    rules.delete_filter(there.id.as_deref().unwrap()).await.unwrap();
    assert!(rules.server().filters().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_rules_list_says_which_rules_run_here() {
    let (h, rules) = beside(theirs_runs()).await;
    let here = rules.local().create_filter(&Filter::block("pest@example.com")).await.unwrap();
    let services = crate::AccountServices::fake_jmap(Arc::clone(&h.fake)).with_rules(crate::AnyRules::Beside(rules));
    let (sender, _events) = async_channel::unbounded();
    let sync = Arc::new(crate::AccountSync::new(h.account_id, services, h.db.clone(), sender));
    let accounts = crate::tests::Connected(std::collections::HashMap::from([(h.account_id, sync)]));
    let settings = crate::AccountSettings::new(Arc::new(accounts), h.db.clone());
    let crate::Permitted::Done(list) = settings.rule_list(h.account_id).await.unwrap() else {
        panic!("no permission is asked over JMAP");
    };
    assert_eq!(list.rules.len(), 1);
    assert_eq!(list.here, vec![here.id.unwrap()]);
}
