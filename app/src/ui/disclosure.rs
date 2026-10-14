//! Disclosure icons whose shape stays independent of the desktop icon theme.

use gtk::prelude::*;

pub fn row(row: adw::ExpanderRow) -> adw::ExpanderRow {
    fn replace(widget: &gtk::Widget) {
        if widget.has_css_class("expander-row-arrow")
            && let Some(image) = widget.downcast_ref::<gtk::Image>()
        {
            image.set_icon_name(Some("penguin-mail-chevron-up-symbolic"));
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            replace(&widget);
            child = widget.next_sibling();
        }
    }
    replace(row.upcast_ref());
    row
}

#[cfg(test)]
pub(crate) mod checks {
    use super::*;

    pub fn run() {
        let row = row(adw::ExpanderRow::builder().title("Details").build());
        fn find(widget: &gtk::Widget) -> Option<gtk::Image> {
            if widget.has_css_class("expander-row-arrow") {
                return widget.clone().downcast().ok();
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                if let Some(image) = find(&widget) { return Some(image); }
                child = widget.next_sibling();
            }
            None
        }
        let arrow = find(row.upcast_ref()).expect("native disclosure image");
        assert_eq!(arrow.icon_name().as_deref(), Some("penguin-mail-chevron-up-symbolic"));
    }
}
