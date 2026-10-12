//! The listing run, headless. Each test drives `ListingRun` the way the
//! window does and checks what the list shows and what was asked for.

use std::collections::HashMap;

use mailrs_domain::{Folder, Target};
use mailrs_sync::PAGE;

use super::fake::{Call, Fake, row};
use super::newly_followed;
use crate::ui::window::Reveal;
use crate::ui::window::on_screen::OnScreen;
use crate::ui::{Mailbox, Standard};

fn inbox() -> Mailbox {
    Mailbox::Unified(Standard::Inbox)
}

fn label(id: &str) -> Mailbox {
    Mailbox::Label {
        account_id: 1,
        label_id: id.into(),
        name: id.into(),
    }
}

fn trash() -> Mailbox {
    Mailbox::Folder {
        account_id: Some(1),
        folder: Folder::Trash,
    }
}

fn show(mailbox: Mailbox) -> impl FnOnce(&mut OnScreen) -> Option<super::Redraw> {
    move |screen| Some(screen.show(mailbox, false))
}

/// A server search the reader typed over is stopped, and what it had
/// found by then never reaches the list.
#[tokio::test]
async fn a_search_stopped_mid_answer_never_lands() {
    let (fake, run) = Fake::new();
    run.change(show(inbox()));
    fake.settle().await;
    fake.during(Call::ServerSearch, |run| run.typed("invoices"));
    run.typed("invoice");
    fake.settle().await;
    let stops = fake.with(|s| s.stops.clone());
    assert_eq!(stops.len(), 2, "each search asked the servers");
    assert!(stops[0].stopped(), "the key stopped the first search");
    assert!(!stops[1].stopped());
    let pages = fake.with(|s| s.pages.clone());
    assert!(
        !pages.iter().flatten().any(|id| id == "invoice-server"),
        "the stopped answer stayed off the list: {pages:?}"
    );
    assert_eq!(fake.ids(), ["invoices-server"]);
}

/// A notification opens a thread while another mailbox is on screen: the
/// inbox comes on screen and the thread is selected once its rows land.
#[tokio::test]
async fn a_reveal_selects_after_the_first_page_lands() {
    let (fake, run) = Fake::new();
    fake.with(|s| s.store = vec![row("t1", 1)]);
    run.change(show(trash()));
    fake.settle().await;
    run.reveal((1, "t1".into(), Reveal::Read));
    assert_eq!(fake.calls(Call::Select), 0, "the inbox's rows are not there yet");
    fake.settle().await;
    assert_eq!(run.mailbox(), inbox());
    let calls = fake.with(|s| s.calls.clone());
    let last_rows = calls.iter().rposition(|c| *c == Call::SetRows).unwrap();
    let select = calls.iter().position(|c| *c == Call::Select).unwrap();
    assert!(last_rows < select, "selected after the rows landed");
    assert_eq!(fake.with(|s| s.selected.clone()), [(1, "t1".into(), Reveal::Read)]);
}

#[tokio::test]
async fn a_reveal_in_the_inbox_on_screen_selects_at_once() {
    let (fake, run) = Fake::new();
    run.change(show(inbox()));
    fake.settle().await;
    run.reveal((1, "t1".into(), Reveal::Reply));
    assert_eq!(fake.calls(Call::Select), 1);
    assert_eq!(fake.calls(Call::List), 1, "nothing listed again");
}

/// A change event in a folder the servers list: the rows that left it go,
/// and the rest stay.
#[tokio::test]
async fn a_folder_prunes_the_threads_that_left_it() {
    let (fake, run) = Fake::new();
    fake.with(|s| {
        s.store = vec![row("t1", 1), row("t2", 2)];
        s.gone = vec![Target::thread(1, "t2")];
    });
    run.change(show(trash()));
    fake.settle().await;
    assert_eq!(fake.ids(), ["t1", "t2"]);
    run.changed(vec![(1, "t1".into()), (1, "t2".into())]);
    fake.settle().await;
    assert_eq!(fake.calls(Call::GoneFrom), 1);
    assert_eq!(fake.ids(), ["t1"]);
    assert_eq!(fake.calls(Call::List), 1, "the folder was not listed again");
}

/// An answer for a mailbox the reader left while it was out goes nowhere.
#[tokio::test]
async fn a_late_answer_for_a_mailbox_no_longer_shown_is_dropped() {
    let (fake, run) = Fake::new();
    fake.during(Call::List, |run| run.change(show(label("B"))));
    run.change(show(label("A")));
    fake.settle().await;
    assert_eq!(run.mailbox(), label("B"));
    assert_eq!(fake.with(|s| s.pages.clone()), [["B-1"]]);
}

/// The answer to a prune that comes back after the folder left the screen
/// takes no rows from the mailbox shown now.
#[tokio::test]
async fn a_late_prune_leaves_the_next_mailbox_alone() {
    let (fake, run) = Fake::new();
    fake.with(|s| {
        s.store = vec![row("t1", 1)];
        s.gone = vec![Target::thread(1, "t1")];
    });
    run.change(show(trash()));
    fake.settle().await;
    fake.during(Call::GoneFrom, |run| run.change(show(inbox())));
    run.prune(vec![Target::thread(1, "t1")]);
    fake.settle().await;
    assert_eq!(fake.calls(Call::DropRows), 0);
    assert_eq!(fake.ids(), ["t1"]);
}

/// The list was first listed while an account's first sync had stored
/// only a few threads. Older mail that arrives afterwards still reaches
/// it, a page at a time as the reader scrolls.
#[tokio::test]
async fn mail_that_arrives_after_a_short_first_page_loads_as_the_reader_scrolls() {
    let (fake, run) = Fake::new();
    let newest: Vec<_> = (0..20).map(|n| row(&format!("n{n}"), n)).collect();
    fake.with(|s| s.store = newest.clone());
    run.change(show(inbox()));
    fake.settle().await;
    assert_eq!(fake.ids().len(), 20);
    run.scrolled_to_end();
    fake.settle().await;
    assert_eq!(fake.calls(Call::Append), 0, "the store held no more yet");

    let older: Vec<_> = (0..600).map(|n| row(&format!("o{n}"), 100 + n)).collect();
    fake.with(|s| s.store.extend(older.iter().cloned()));
    run.changed(older.iter().map(|r| (1, r.id.clone())).collect());
    fake.settle().await;
    assert_eq!(fake.ids().len(), PAGE, "a page's worth went in");

    run.scrolled_to_end();
    fake.settle().await;
    assert_eq!(fake.with(|s| s.held.clone()), [PAGE]);
    assert_eq!(fake.ids().len(), 620, "the rest came with the next page");
}

#[tokio::test]
async fn a_burst_of_change_events_reads_the_store_once() {
    let (fake, run) = Fake::new();
    fake.with(|s| s.store = vec![row("t1", 1), row("t2", 2)]);
    run.change(show(inbox()));
    fake.settle().await;
    run.changed(vec![(1, "t1".into())]);
    run.changed(vec![(1, "t2".into())]);
    fake.settle().await;
    assert_eq!(fake.calls(Call::Changed), 1);
    assert_eq!(fake.calls(Call::Refreshing), 1);
}

/// A folder put on screen shows the spinner while the servers answer; the
/// same folder listed again after a change keeps its rows up.
#[tokio::test]
async fn a_reload_keeps_the_rows_where_a_new_mailbox_spins() {
    let (fake, run) = Fake::new();
    run.change(show(trash()));
    assert_eq!(fake.calls(Call::Loading), 1);
    fake.settle().await;
    run.reload_remote();
    fake.settle().await;
    assert_eq!(fake.calls(Call::Loading), 1);
    assert_eq!(fake.calls(Call::List), 2);
}

/// A label on a folder account is followed the first time it is listed,
/// and not again on a reload.
#[tokio::test]
async fn a_label_is_followed_once() {
    let (fake, run) = Fake::new();
    run.change(show(label("Work")));
    fake.settle().await;
    run.reload();
    fake.settle().await;
    assert_eq!(fake.with(|s| s.followed.clone()), [(1, "Work".to_string())]);
}

#[test]
fn a_mailbox_followed_once_is_not_followed_again() {
    let mut followed = HashMap::new();
    assert!(newly_followed(&mut followed, 1, "Work".to_string()));
    assert!(!newly_followed(&mut followed, 1, "Work".to_string()));
    assert!(
        newly_followed(&mut followed, 1, "Travel".to_string()),
        "another mailbox of the same account is new"
    );
    assert!(
        newly_followed(&mut followed, 2, "Work".to_string()),
        "the same name in another account is new"
    );
}
