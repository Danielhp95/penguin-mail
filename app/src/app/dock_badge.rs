//! The Dock badge's platform boundary; other desktops use the tray.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::Badge;

#[cfg(not(target_os = "macos"))]
#[derive(Default)]
pub(super) struct Badge {}

#[cfg(not(target_os = "macos"))]
impl Badge {
    pub(super) fn watch(&self, _app: &std::rc::Rc<super::App>) {}

    pub(super) fn recount(&self) {}
}
