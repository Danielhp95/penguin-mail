//! The unread count drawn on the tray icon: the number in bold white on
//! an orange pill across the middle of the envelope, as large as the
//! picture allows, with the flap and the frame showing around it so the
//! penguin stays recognisable. Cairo draws the pill and the
//! digits onto a layer of their own; the layer goes onto the icon here,
//! in Rust, so the compositing can be tested on plain bytes.

/// The badge's fill: the beak's orange from the app icon.
const ORANGE: (f64, f64, f64) = (0xe8 as f64 / 255.0, 0x66 as f64 / 255.0, 0x0c as f64 / 255.0);

/// What the badge says for `unread`: nothing at zero, the number up to
/// 99, and "99+" past it. A 16 px icon says "9+" instead; see [`fitted`].
pub fn text(unread: i64) -> Option<String> {
    match unread {
        ..=0 => None,
        1..=99 => Some(unread.to_string()),
        _ => Some("99+".into()),
    }
}

/// `layer` laid over `base`, both `ksni::Icon` data: ARGB32 in network
/// byte order with straight alpha, the same size. Source over, as a
/// painter puts one picture on another.
pub fn over(base: &mut [u8], layer: &[u8]) {
    for (under, top) in base.as_chunks_mut::<4>().0.iter_mut().zip(layer.as_chunks::<4>().0) {
        let top_alpha = u32::from(top[0]);
        if top_alpha == 0 {
            continue;
        }
        if top_alpha == 255 {
            *under = *top;
            continue;
        }
        let under_alpha = u32::from(under[0]);
        // Everything in 255ths: what shows of the base is its alpha times
        // what the top leaves uncovered.
        let shown = under_alpha * (255 - top_alpha) / 255;
        let alpha = top_alpha + shown;
        under[0] = alpha as u8;
        for channel in 1..4 {
            let mixed = u32::from(top[channel]) * top_alpha + u32::from(under[channel]) * shown;
            under[channel] = ((mixed + alpha / 2) / alpha) as u8;
        }
    }
}

/// `length` rounded, then made one longer if needed so that it leaves an
/// even margin on a `size` px icon, which keeps a centred shape on whole
/// pixels.
fn even_with(size: f64, length: f64) -> f64 {
    let length = length.round();
    if (size - length) % 2.0 == 0.0 { length } else { length + 1.0 }
}

/// Where the badge sits on a `size` px icon, as (x, y, width, height),
/// for text `width` px wide: a circle for one digit, a pill for more,
/// centred across the icon and a little below its middle, over the face,
/// and never wider than the icon.
pub(super) fn pill(size: f64, text_width: f64) -> (f64, f64, f64, f64) {
    let height = even_with(size, (size * 0.6).max(12.0));
    let padding = (height * 0.2).max(1.0);
    let width = even_with(size, (text_width + 2.0 * padding).max(height)).min(size);
    let y = (size * 0.56 - height / 2.0).round().min(size - height);
    ((size - width) / 2.0, y, width, height)
}

/// The text the badge shows on a `size` px icon and how far to scale its
/// font down to fit, given the text's width at full size. "99+" cannot be
/// read at 16 px, so the smallest icon says "9+" instead.
fn fitted(size: f64, text: &str, width_at: impl Fn(&str) -> f64) -> (String, f64) {
    let text = match text {
        "99+" if size < 22.0 => "9+",
        text => text,
    };
    let room = size - 2.0 * (size * 0.06).max(1.0);
    (text.into(), (room / width_at(text)).min(1.0))
}

/// The badge alone on a transparent `size` px square, as ARGB32 bytes in
/// network order with straight alpha, ready for [`over`].
pub fn layer(size: u32, text: &str) -> Vec<u8> {
    use gtk::cairo::{self, Antialias, Context, Format, HintMetrics, HintStyle, ImageSurface};
    let side = size as i32;
    let Ok(mut surface) = ImageSurface::create(Format::ARgb32, side, side) else {
        return Vec::new();
    };
    {
        let Ok(cr) = Context::new(&surface) else {
            return Vec::new();
        };
        let full = pill(f64::from(size), 0.0).3 * 0.92;
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(full);
        let mut options = cairo::FontOptions::new().ok();
        if let Some(options) = options.as_mut() {
            options.set_antialias(Antialias::Gray);
            options.set_hint_style(HintStyle::Slight);
            options.set_hint_metrics(HintMetrics::On);
            cr.set_font_options(options);
        }
        let (text, scale) = fitted(f64::from(size), text, |text| {
            cr.text_extents(text).map_or(f64::MAX, |e| e.width())
        });
        cr.set_font_size(full * scale);
        let Ok(extents) = cr.text_extents(&text) else {
            return Vec::new();
        };
        // The digits' own height, so a "+" does not pull the number off
        // centre.
        let digits = cr.text_extents("0").unwrap_or(extents);
        let (x, y, width, height) = pill(f64::from(size), extents.width());
        let radius = height / 2.0;
        cr.new_sub_path();
        cr.arc(x + width - radius, y + radius, radius, -90f64.to_radians(), 90f64.to_radians());
        cr.arc(x + radius, y + radius, radius, 90f64.to_radians(), 270f64.to_radians());
        cr.close_path();
        cr.set_source_rgb(ORANGE.0, ORANGE.1, ORANGE.2);
        if cr.fill().is_err() {
            return Vec::new();
        }
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let left = x + (width - extents.width()) / 2.0 - extents.x_bearing();
        let base = y + (height - digits.height()) / 2.0 - digits.y_bearing();
        cr.move_to(left.round(), base.round());
        if cr.show_text(&text).is_err() {
            return Vec::new();
        }
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let Ok(data) = surface.data() else {
        return Vec::new();
    };
    from_cairo(size as usize, stride, &data)
}

/// Cairo's ARGB32, one native-endian word a pixel with premultiplied
/// alpha, as straight ARGB bytes in network order.
fn from_cairo(size: usize, stride: usize, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        let row = &data[y * stride..y * stride + size * 4];
        for word in row.as_chunks::<4>().0 {
            let pixel = u32::from_ne_bytes(*word);
            let alpha = pixel >> 24;
            let straight = |channel: u32| match alpha {
                0 => 0,
                a => ((channel * 255 + a / 2) / a).min(255) as u8,
            };
            out.extend_from_slice(&[
                alpha as u8,
                straight((pixel >> 16) & 0xff),
                straight((pixel >> 8) & 0xff),
                straight(pixel & 0xff),
            ]);
        }
    }
    out
}

/// `icons` with the badge for `unread` on each, or as they are when there
/// is nothing unread.
pub fn badged(icons: &[ksni::Icon], unread: i64) -> Vec<ksni::Icon> {
    let Some(text) = text(unread) else {
        return icons.to_vec();
    };
    icons
        .iter()
        .map(|icon| {
            let mut data = icon.data.clone();
            let badge = layer(icon.width as u32, &text);
            if icon.width == icon.height && badge.len() == data.len() {
                over(&mut data, &badge);
            }
            ksni::Icon {
                width: icon.width,
                height: icon.height,
                data,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{badged, fitted, layer, over, pill, text};

    #[test]
    fn nothing_unread_has_no_badge() {
        assert_eq!(text(0), None);
        assert_eq!(text(-3), None);
    }

    #[test]
    fn the_badge_says_the_number_up_to_ninety_nine() {
        assert_eq!(text(1).as_deref(), Some("1"));
        assert_eq!(text(42).as_deref(), Some("42"));
        assert_eq!(text(99).as_deref(), Some("99"));
    }

    #[test]
    fn past_ninety_nine_the_badge_says_ninety_nine_plus() {
        assert_eq!(text(100).as_deref(), Some("99+"));
        assert_eq!(text(12_000).as_deref(), Some("99+"));
    }

    #[test]
    fn an_opaque_top_pixel_replaces_the_one_under_it() {
        let mut base = [255, 10, 20, 30];
        over(&mut base, &[255, 232, 102, 12]);
        assert_eq!(base, [255, 232, 102, 12]);
    }

    #[test]
    fn a_clear_top_pixel_leaves_the_base_alone() {
        let mut base = [200, 10, 20, 30];
        over(&mut base, &[0, 255, 255, 255]);
        assert_eq!(base, [200, 10, 20, 30]);
    }

    #[test]
    fn a_half_clear_top_pixel_mixes_with_an_opaque_base() {
        let mut base = [255, 0, 0, 0];
        over(&mut base, &[128, 255, 255, 255]);
        assert_eq!(base[0], 255);
        assert!((127..=129).contains(&base[1]), "{base:?}");
    }

    #[test]
    fn an_edge_pixel_over_nothing_keeps_its_own_colour() {
        // Antialiasing at the badge's rim lands on the icon's transparent
        // corner; the rim must stay orange, not darken towards black.
        let mut base = [0, 0, 0, 0];
        over(&mut base, &[100, 232, 102, 12]);
        assert_eq!(base, [100, 232, 102, 12]);
    }

    #[test]
    fn the_badge_is_centred_across_the_icon_and_as_wide_as_its_number_needs() {
        let (x, y, width, height) = pill(48.0, 10.0);
        assert_eq!(width, height, "one digit is a circle");
        assert_eq!(x * 2.0 + width, 48.0, "centred across");
        assert!(y > 0.0 && y + height < 48.0, "inside the icon: {y} + {height}");
        assert!(height >= 48.0 * 0.55, "{height} px is too small to read");
        let (x, _, width, _) = pill(16.0, 30.0);
        assert_eq!((x, width), (0.0, 16.0), "a wide count stops at the icon's edge");
    }

    #[test]
    fn only_the_smallest_icon_says_nine_plus() {
        // Each character 8 px wide at the size the badge wants, so "99+"
        // shrinks at 22 px as well; it still says the same at every size
        // from there up, whatever the font measures.
        let width = |text: &str| 8.0 * text.len() as f64;
        assert_eq!(fitted(16.0, "99+", width).0, "9+");
        assert_eq!(fitted(16.0, "42", width).0, "42");
        assert_eq!(fitted(22.0, "99+", width).0, "99+");
        assert_eq!(fitted(64.0, "99+", width).0, "99+");
    }

    #[test]
    fn a_count_that_fits_keeps_the_full_size() {
        let (_, full) = fitted(48.0, "7", |text: &str| 6.0 * text.len() as f64);
        let (_, shrunk) = fitted(16.0, "42", |text: &str| 8.0 * text.len() as f64);
        assert_eq!(full, 1.0);
        assert!(shrunk < 1.0);
    }

    /// The first and past-the-last rows holding white pixels.
    fn white_rows(pixels: &[u8], side: usize) -> (usize, usize) {
        let (mut top, mut bottom) = (side, 0);
        for (i, p) in pixels.as_chunks::<4>().0.iter().enumerate() {
            if p[0] == 255 && p[1] > 240 && p[2] > 240 && p[3] > 240 {
                top = top.min(i / side);
                bottom = bottom.max(i / side + 1);
            }
        }
        (top, bottom)
    }

    #[test]
    fn the_badge_layer_is_orange_in_the_middle_and_clear_in_the_corners() {
        for size in [16u32, 22, 24, 32, 48, 64] {
            let pixels = layer(size, "7");
            let side = size as usize;
            assert_eq!(pixels.len(), side * side * 4);
            let at = |x: usize, y: usize| &pixels[(y * side + x) * 4..(y * side + x) * 4 + 4];
            for (x, y) in [(0, 0), (side - 1, 0), (0, side - 1), (side - 1, side - 1)] {
                assert_eq!(at(x, y)[0], 0, "{size} px: ({x}, {y}) is clear");
            }
            // Just inside the circle's left edge, at its middle, is orange.
            let (x, y, _, height) = pill(f64::from(size), 0.0);
            let edge = at(x as usize + 1, (y + height / 2.0) as usize);
            assert_eq!(edge[0], 255, "{size} px: {edge:?}");
            assert!(edge[1] > 200 && edge[3] < 60, "{size} px: {edge:?}");
        }
    }

    #[test]
    fn the_number_is_white_and_large() {
        for size in [16u32, 22, 32, 48, 64] {
            let side = size as usize;
            let (top, bottom) = white_rows(&layer(size, "8"), side);
            let tall = bottom.saturating_sub(top);
            assert!(tall * 100 >= side * 38, "{size} px: the digit is {tall} px tall");
        }
    }

    #[test]
    fn nothing_unread_leaves_the_icons_as_they_are() {
        let icons = vec![ksni::Icon {
            width: 16,
            height: 16,
            data: vec![255; 16 * 16 * 4],
        }];
        assert_eq!(badged(&icons, 0)[0].data, icons[0].data);
        assert_ne!(badged(&icons, 3)[0].data, icons[0].data);
    }
}
