//! The text view the composer's body is written in, which has the screen
//! repaint all of it that shows after each edit.
//!
//! GTK 4.22 left the bottom row of a "y" or a "g" on screen when typing
//! moved the lines below it down, and left a row of a moved line out. GSK
//! bounds each run of text by its glyphs' exact ink, but the renderer
//! draws a line whose baseline falls on a fraction of a pixel at the
//! nearest whole pixel, up to half a pixel lower, so that row lies
//! outside the area it repaints. The body's `line-height: 1.5` puts most
//! baselines on a fraction, and so do wrapped lines without it. A plain
//! `queue_draw` changes nothing, because the renderer still repaints only
//! the nodes that differ. A faint fill under the text, whose alpha
//! changes with each edit, makes the node under the whole view differ,
//! and the renderer repaints all of it.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use std::cell::Cell;

    use gtk::subclass::prelude::*;
    use gtk::{gdk, glib, graphene, prelude::*};

    /// The two alphas the fill under the text takes in turn. At most a
    /// thousandth of black, it rounds to no change in any 8-bit colour.
    const FAINT: [f32; 2] = [1.0 / 2048.0, 1.0 / 1024.0];

    #[derive(Default)]
    pub struct BodyView {
        /// Which of [`FAINT`] the fill takes; it flips with each edit.
        pub(super) flip: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BodyView {
        const NAME: &'static str = "PenguinMailBodyView";
        type Type = super::BodyView;
        type ParentType = gtk::TextView;
    }

    impl ObjectImpl for BodyView {}
    impl WidgetImpl for BodyView {}

    impl TextViewImpl for BodyView {
        fn snapshot_layer(&self, layer: gtk::TextViewLayer, snapshot: gtk::Snapshot) {
            self.parent_snapshot_layer(layer, snapshot.clone());
            if layer != gtk::TextViewLayer::BelowText {
                return;
            }
            // The layers draw in buffer coordinates.
            let shown = self.obj().visible_rect();
            let alpha = FAINT[usize::from(self.flip.get())];
            snapshot.append_color(
                &gdk::RGBA::new(0.0, 0.0, 0.0, alpha),
                &graphene::Rect::new(
                    shown.x() as f32,
                    shown.y() as f32,
                    shown.width() as f32,
                    shown.height() as f32,
                ),
            );
        }
    }
}

glib::wrapper! {
    /// A text view that repaints all it shows after each edit.
    pub struct BodyView(ObjectSubclass<imp::BodyView>)
        @extends gtk::TextView, gtk::Widget,
        @implements gtk::Accessible, gtk::AccessibleText, gtk::Buildable,
            gtk::ConstraintTarget, gtk::Scrollable;
}

impl BodyView {
    /// A text view whose buffer, on each change, has the next frame
    /// repaint the whole view.
    pub fn new() -> BodyView {
        let view: BodyView = glib::Object::new();
        let weak = view.downgrade();
        view.buffer().connect_changed(move |_| {
            if let Some(view) = weak.upgrade() {
                let flip = &view.imp().flip;
                flip.set(!flip.get());
                view.queue_draw();
            }
        });
        view
    }
}
