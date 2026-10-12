//! The Dock keeps the All Inboxes unread count even without an open window.
//! `crate::unread` counts; this only draws the number.

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSString;

#[derive(Default)]
pub(in crate::app) struct Badge {}

impl Badge {
    /// Shows `unread` on the app icon, and nothing at zero.
    pub(in crate::app) fn show(&self, unread: i64) {
        let label = (unread > 0).then(|| NSString::from_str(&unread.to_string()));
        let mtm = MainThreadMarker::new().expect("the Dock updates on the GTK thread");
        NSApplication::sharedApplication(mtm)
            .dockTile()
            .setBadgeLabel(label.as_deref());
    }
}
