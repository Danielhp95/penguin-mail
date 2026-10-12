//! Switch to JMAP: says what the switch changes and what it keeps, asks
//! for what the JMAP sign-in needs, and runs the switch. The account stays
//! where it is, with its name and colour. The words come from
//! `crate::offered`; the work happens in `Core::switch_to_jmap`.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use mailrs_discover::JmapFound;
use mailrs_domain::Account;
use mailrs_domain::translate::{fill, gettext};
use mailrs_sync::switch::{SwitchTo, Switched};

use crate::add_account::{self, Address, JmapProposal};
use crate::core::Core;
use crate::offered::{self, SwitchRow};

/// The page's side margin, and the narrower one under 400 px.
const SIDE: i32 = 20;
const NARROW_SIDE: i32 = 14;
/// The gap between the page's groups.
const SPACING: i32 = 18;

struct Dialog {
    core: Rc<Core>,
    account: Account,
    proposal: JmapProposal,
    token: bool,
    dialog: adw::Dialog,
    toasts: adw::ToastOverlay,
    secret: adw::PasswordEntryRow,
    switch: gtk::Button,
    progress: gtk::Box,
    header: adw::HeaderBar,
    scrolled: gtk::ScrolledWindow,
    page: gtk::Box,
    /// The failure toast on show, which a new try takes away.
    failure: std::cell::RefCell<Option<adw::Toast>>,
    /// The height the failure toast takes under the page while it shows,
    /// so it covers nothing, as in the mockup.
    toast_room: std::cell::Cell<i32>,
    done: Box<dyn Fn(Switched)>,
}

/// Shows Switch to JMAP for `account` over the session `found`.
/// `calendars` says the account's calendars sign in with the password a
/// token replaces for mail. `done` runs once the switch went through, after
/// the dialog closes.
pub fn present(
    parent: &impl IsA<gtk::Widget>,
    core: Rc<Core>,
    account: Account,
    found: JmapFound,
    calendars: bool,
    done: impl Fn(Switched) + 'static,
) {
    let Some(address) = Address::parse(&account.email) else {
        return;
    };
    let preview = core.demo.then(|| std::env::var("MAILRS_DEMO_SWITCH").ok()).flatten();
    let found = match preview.as_deref() {
        // A server that takes a password, which no demo account has.
        Some("password") => JmapFound { provider: None, session_url: "https://mail.home.example/jmap/session".into(), ..found },
        _ => found,
    };
    let proposal = add_account::jmap_proposal(found, None, &address);
    let token = add_account::token_only(&proposal);
    let rows = offered::switch_rows(&account, token && calendars);

    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(SPACING)
        .margin_top(14)
        .margin_bottom(22)
        .margin_start(SIDE)
        .margin_end(SIDE)
        .build();
    page.append(&info_group(&gettext("What changes"), &rows.changes));
    page.append(&info_group(&gettext("What stays"), &rows.stays));

    let title = add_account::jmap_secret_title(token);
    let secret = adw::PasswordEntryRow::builder().title(&title).build();
    // Added, not set: a builder's classes replace the row's own `entry`.
    secret.add_css_class("switch-entry");
    crate::ui::name(&secret, &title);
    let sent_to = gtk::Label::builder()
        .label(fill(
            &gettext("Sent to {host} and nowhere else."),
            &[("host", &add_account::host_of(&proposal.session_url))],
        ))
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label", "switch-sent-to"])
        .build();
    let entry = adw::PreferencesGroup::new();
    entry.add(&secret);
    let secret_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
    secret_area.append(&sent_to);
    secret_area.append(&entry);
    page.append(&secret_area);
    if let Some((heading, body, link)) = add_account::jmap_hint(&proposal, token) {
        page.append(&help_card(&heading, &body, link.as_ref()));
    }
    let progress = gtk::Box::builder().spacing(10).halign(gtk::Align::Center).visible(false).css_classes(["switch-progress"]).build();
    progress.append(&adw::Spinner::builder().width_request(24).height_request(24).build());
    progress.append(&gtk::Label::new(Some(&gettext("Matching your mail…"))));
    page.append(&progress);

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&page)
        .build();
    let switch = gtk::Button::builder().label(gettext("Switch")).css_classes(["suggested-action"]).sensitive(false).build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_end(&switch);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));
    let toasts = adw::ToastOverlay::new();
    toasts.add_css_class("switch-toasts");
    toasts.set_child(Some(&toolbar));
    let dialog = adw::Dialog::builder()
        .title(gettext("Switch to JMAP"))
        .content_width(480)
        .child(&toasts)
        .css_classes(["switch-jmap"])
        .build();
    if let Ok(condition) = adw::BreakpointCondition::parse("max-width: 400px") {
        let narrow = adw::Breakpoint::new(condition);
        narrow.add_setters(&[(&page, "margin-start", NARROW_SIDE), (&page, "margin-end", NARROW_SIDE)]);
        dialog.add_breakpoint(narrow);
    }

    let this = Rc::new(Dialog {
        core,
        account,
        proposal,
        token,
        dialog: dialog.clone(),
        toasts,
        secret: secret.clone(),
        switch: switch.clone(),
        progress,
        header,
        scrolled,
        page,
        failure: std::cell::RefCell::default(),
        toast_room: std::cell::Cell::new(0),
        done: Box::new(done),
    });
    let ready = switch.clone();
    secret.connect_changed(move |row| ready.set_sensitive(!row.text().is_empty()));
    let weak = Rc::downgrade(&this);
    switch.connect_clicked(move |_| {
        if let Some(this) = weak.upgrade() {
            this.start();
        }
    });
    let weak = Rc::downgrade(&this);
    secret.connect_entry_activated(move |_| {
        if let Some(this) = weak.upgrade()
            && this.switch.is_sensitive()
        {
            this.start();
        }
    });
    // The dialog owns the state behind its controls until it closes.
    let keep = std::cell::RefCell::new(Some(Rc::clone(&this)));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });
    dialog.present(Some(parent));
    // Once the page has its width, so the wrapped rows count.
    let weak = Rc::downgrade(&this);
    let mut tries = 0;
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        tries += 1;
        match weak.upgrade() {
            Some(this) if !this.fit() && tries < 40 => glib::ControlFlow::Continue,
            _ => glib::ControlFlow::Break,
        }
    });
    // Nothing has focus until the person picks the field, as the mockup
    // draws it; Tab reaches the field first.
    if preview.as_deref().is_none_or(|stage| stage == "offer") {
        dialog.set_focus(None::<&gtk::Widget>);
    }
    if let Some(stage) = preview {
        if stage != "offer" {
            this.secret.set_text(&"x".repeat(if this.token { 24 } else { 12 }));
            this.secret.grab_focus();
        }
        // After the dialog has mapped and settled its own focus.
        glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || this.preview(&stage));
    }
}

/// A titled group of icon rows that only inform: nothing in them opens.
fn info_group(title: &str, rows: &[SwitchRow]) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    for row in rows {
        let shown = crate::ui::plain_row().build();
        shown.add_css_class("switch-info");
        shown.set_title(&row.title);
        shown.set_subtitle(&row.subtitle);
        shown.add_prefix(&gtk::Image::builder().icon_name(row.icon).css_classes(["dim-label"]).build());
        group.add(&shown);
    }
    group
}

/// The provider's advice on making a token, in a tinted card, with the
/// page that explains it.
fn help_card(heading: &str, body: &str, link: Option<&add_account::Link>) -> gtk::Box {
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["switch-help"]).build();
    let label = |text: &str, classes: &[&str]| {
        gtk::Label::builder().label(text).wrap(true).xalign(0.0).css_classes(classes.to_vec()).build()
    };
    card.append(&label(heading, &["switch-help-title"]));
    card.append(&label(body, &["switch-help-body", "dim-label"]));
    if let Some(link) = link {
        let content = adw::ButtonContent::builder().icon_name("adw-external-link-symbolic").label(&link.label).build();
        let button = gtk::LinkButton::builder().uri(&link.url).halign(gtk::Align::Start).css_classes(["switch-help-link"]).build();
        button.set_child(Some(&content));
        crate::ui::name(&button, &link.label);
        card.append(&button);
    }
    card
}

impl Dialog {
    /// Dims the entry and Switch while the switch runs, with the spinner
    /// under them, and lights them again after it.
    fn running(&self, on: bool) {
        self.secret.set_sensitive(!on);
        self.switch.set_sensitive(!on && !self.secret.text().is_empty());
        self.progress.set_visible(on);
        // Closing now would leave a failure with nowhere to show.
        self.dialog.set_can_close(!on);
        self.fit();
    }

    /// Sizes the dialog to its page at the width it has. A dialog keeps
    /// the height it opened with, and asks its scrolled page for a height
    /// at its natural width: at 360 px the rows wrap to more lines than
    /// that, and the spinner's line comes later, so both would scroll out
    /// of sight.
    /// Answers whether the page had a width to measure at.
    fn fit(&self) -> bool {
        let width = self.scrolled.width();
        if width <= 0 || !self.scrolled.is_mapped() {
            return false;
        }
        let page = self.page.measure(gtk::Orientation::Vertical, width).1;
        self.dialog.set_content_height(self.header.height() + page + self.toast_room.get());
        true
    }

    /// Puts the cursor at the end of the secret, with nothing selected,
    /// so a fix to a pasted token does not start by replacing it.
    fn focus_secret(&self) {
        self.secret.grab_focus();
        // The entry selects its text as it takes focus, once this returns.
        let secret = self.secret.clone();
        glib::idle_add_local_once(move || {
            let end = secret.text().chars().count() as i32;
            secret.select_region(end, end);
        });
    }

    fn start(self: &Rc<Self>) {
        if self.secret.text().is_empty() {
            return;
        }
        // Taken out before the dismiss, whose signal could reach back here.
        let shown = self.failure.borrow_mut().take();
        if let Some(toast) = shown {
            toast.dismiss();
        }
        self.running(true);
        self.secret.upcast_ref::<gtk::Widget>().announce(&gettext("Matching your mail…"), gtk::AccessibleAnnouncementPriority::Medium);
        let to = SwitchTo {
            session_url: self.proposal.session_url.clone(),
            token: self.token,
            user: self.account.email.clone(),
            secret: self.secret.text().to_string(),
        };
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            match this.core.switch_to_jmap(this.account.clone(), to).await {
                Ok(switched) => {
                    this.running(false);
                    this.dialog.close();
                    (this.done)(switched);
                }
                Err(err) => {
                    this.running(false);
                    let failed = offered::switch_failure(&err, &this.proposal.provider_name, this.token);
                    this.fail(&failed.line, failed.again);
                }
            }
        });
    }

    /// The failure toast. When the server's mail kept changing, it offers
    /// another try with what the person typed.
    fn fail(self: &Rc<Self>, line: &str, again: bool) {
        // A toast's own title keeps to one line and cuts the reason off;
        // this one wraps, as the mockup's three-line toast does. The toast
        // sizes itself to the label's natural width, so that width has to
        // fit the dialog: its margins and the close button take about 100
        // px, Try Again about 150 more, and a character about 7.5.
        let around = if again { 250 } else { 100 };
        let chars = ((self.toasts.width() - around) as f32 / 7.5).clamp(16.0, 60.0) as i32;
        let words = gtk::Label::builder()
            .label(line)
            .wrap(true)
            .max_width_chars(chars)
            .xalign(0.0)
            .css_classes(["switch-toast"])
            .build();
        let toast = adw::Toast::builder().custom_title(&words).timeout(0).build();
        if again {
            toast.set_button_label(Some(&gettext("Try Again")));
            let weak = Rc::downgrade(self);
            toast.connect_button_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.start();
                }
            });
        }
        // The toast is its words, 10 px above and below them, at least 46
        // px, and 8 px from the page and 18 from the foot.
        let words_height = words.measure(gtk::Orientation::Vertical, chars * 15 / 2).1;
        self.toast_room.set((words_height + 20).max(46) + 26);
        let weak = Rc::downgrade(self);
        toast.connect_dismissed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toast_room.set(0);
                this.fit();
            }
        });
        self.toasts.add_toast(toast.clone());
        self.failure.replace(Some(toast));
        self.fit();
        self.secret.upcast_ref::<gtk::Widget>().announce(line, gtk::AccessibleAnnouncementPriority::High);
        self.focus_secret();
    }

    /// The demo's screenshot stages, from `MAILRS_DEMO_SWITCH`: `token`
    /// and `password` with a secret typed, `progress`, `failed` (the
    /// demo refuses every switch), `changed`, `done` or `done-rules`,
    /// which close the dialog as a switch would, and `rules-off`, which
    /// opens the Rules with a rule the switch turned off.
    fn preview(self: &Rc<Self>, stage: &str) {
        if stage != "offer" {
            // The entry selects its text when the window first takes focus.
            let secret = self.secret.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
                let end = secret.text().chars().count() as i32;
                secret.select_region(end, end);
            });
        }
        match stage {
            "progress" => self.running(true),
            "failed" => self.start(),
            "changed" => {
                let changed = anyhow::Error::new(mailrs_sync::switch::SwitchError::Sync(
                    mailrs_sync::SyncError::Backend(mailrs_sync::BackendError::Changed),
                ));
                let failed = offered::switch_failure(&changed, &self.proposal.provider_name, self.token);
                self.fail(&failed.line, failed.again);
            }
            "done" | "done-rules" => {
                self.dialog.close();
                let rules_off = if stage == "done-rules" { 2 } else { 0 };
                (self.done)(Switched { account: self.account.clone(), kept: 0, dropped: 0, rules_off });
            }
            // A rule the switch turned off, as the Rules dialog lists it.
            "rules-off" => {
                let (this, id) = (Rc::clone(self), self.account.id);
                glib::spawn_future_local(async move {
                    let off = mailrs_domain::Filter {
                        id: Some("local-demo-allotment".into()),
                        criteria: mailrs_domain::FilterCriteria { subject: Some("allotment".into()), ..Default::default() },
                        action: mailrs_domain::FilterAction {
                            add: vec![mailrs_domain::MailSet::Mailbox("Projects/Allotment".into())],
                            ..Default::default()
                        },
                        off: true,
                        ..Default::default()
                    };
                    if this.core.db.write(move |c| mailrs_store::local_rules::add(c, id, &off)).await.is_ok() {
                        let _ = this.dialog.activate_action("win.account-rules", Some(&id.to_variant()));
                        this.dialog.close();
                    }
                });
            }
            _ => {}
        }
    }
}
