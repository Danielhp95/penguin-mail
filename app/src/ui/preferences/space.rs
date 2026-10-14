//! Server-space rows stay visible while reports arrive in the shared cache.

use adw::prelude::*;
use gtk::glib;
use mailrs_domain::translate::{fill, gettext};
use mailrs_sync::Storage;

pub(super) struct Row {
    row: glib::WeakRef<adw::PreferencesRow>,
    subtitle: glib::WeakRef<gtk::Label>,
    bar: glib::WeakRef<gtk::LevelBar>,
    end: glib::WeakRef<gtk::Label>,
    gauge: glib::WeakRef<gtk::Box>,
    title: String,
    unavailable: String,
}

impl Row {
    pub fn new(title: &str, space: Option<Storage>, unavailable: &str, narrow: &adw::Breakpoint) -> (adw::PreferencesRow, Self) {
        let label = gtk::Label::builder().label(title).xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["title"]).build();
        let subtitle = gtk::Label::builder().label(unavailable)
            .xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["subtitle"]).build();
        let words = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(3)
            .hexpand(true).valign(gtk::Align::Center).build();
        words.append(&label);
        words.append(&subtitle);
        let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(0.0)
            .width_request(150).valign(gtk::Align::Center).css_classes(["server-space"]).build();
        // Server usage runs in the opposite direction to GTK's default level colors.
        for offset in [gtk::LEVEL_BAR_OFFSET_LOW, gtk::LEVEL_BAR_OFFSET_HIGH, gtk::LEVEL_BAR_OFFSET_FULL] {
            bar.remove_offset_value(Some(offset));
        }
        let end = gtk::Label::builder().width_chars(4).xalign(1.0)
            .css_classes(["server-space-percent", "numeric"]).build();
        let gauge = gtk::Box::builder().spacing(12).valign(gtk::Align::Center).visible(false).build();
        gauge.append(&bar);
        gauge.append(&end);
        let content = gtk::Box::builder().spacing(16).css_classes(["server-space-row"]).build();
        content.append(&words);
        content.append(&gauge);
        narrow.add_setter(&content, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
        narrow.add_setter(&content, "spacing", Some(&10.to_value()));
        narrow.add_setter(&bar, "hexpand", Some(&true.to_value()));
        narrow.add_setter(&end, "visible", Some(&false.to_value()));
        let row = adw::PreferencesRow::builder().title(title).activatable(false).focusable(false).child(&content).build();
        let updater = Self {
            row: row.downgrade(), subtitle: subtitle.downgrade(), bar: bar.downgrade(),
            end: end.downgrade(), gauge: gauge.downgrade(), title: title.to_string(), unavailable: unavailable.to_string(),
        };
        updater.update(space);
        (row, updater)
    }

    pub fn update(&self, space: Option<Storage>) {
        let (Some(row), Some(subtitle), Some(bar), Some(end), Some(gauge)) =
            (self.row.upgrade(), self.subtitle.upgrade(), self.bar.upgrade(), self.end.upgrade(), self.gauge.upgrade())
        else { return; };
        let state = space.map_or_else(|| self.unavailable.clone(), super::state_line);
        subtitle.set_label(&state);
        let name = format!("{}: {state}", self.title);
        crate::ui::name(&row, &name);
        row.set_tooltip_text(Some(&state));
        bar.update_property(&[gtk::accessible::Property::Label(&name)]);
        gauge.set_visible(space.is_some());
        for (class, active) in [("full", space.is_some_and(|s| s.full())), ("nearly-full", space.is_some_and(|s| !s.full() && s.past_warning()))] {
            for widget in [subtitle.upcast_ref::<gtk::Widget>(), bar.upcast_ref()] {
                if active { widget.add_css_class(class); } else { widget.remove_css_class(class); }
            }
        }
        if let Some(space) = space {
            bar.set_value((space.used as f64 / space.limit as f64).min(1.0));
            end.set_label(&if space.full() { gettext("Full") } else {
                fill(&gettext("{percent}%"), &[("percent", &space.percent().to_string())])
            });
            if space.full() {
                end.add_css_class("server-space-chip");
                end.remove_css_class("server-space-percent");
                end.set_width_chars(-1);
            } else {
                end.remove_css_class("server-space-chip");
                end.add_css_class("server-space-percent");
                end.set_width_chars(4);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod checks {
    use super::*;

    pub fn run() {
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 450sp").unwrap());
        let (row, update) = Row::new("Space on the Server", None, "No report yet", &narrow);
        let gauge = update.gauge.upgrade().unwrap();
        assert!(!gauge.get_visible());
        update.update(Storage::new(250, 1000, None));
        assert!(gauge.get_visible(), "late reports must reveal the existing row's gauge");
        assert_eq!(update.bar.upgrade().unwrap().value(), 0.25);
        update.update(Storage::new(900, 1000, Some(800)));
        assert_eq!(update.bar.upgrade().unwrap().value(), 0.9);
        assert!(update.bar.upgrade().unwrap().has_css_class("nearly-full"));
        update.update(None);
        assert!(!gauge.get_visible(), "an unavailable report must not show a fictitious zero usage");
        assert!(row.child().is_some());
    }
}
