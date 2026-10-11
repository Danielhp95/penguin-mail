//! The unread count drawn on the tray icon: an orange pill in the top
//! right corner with the number in white. Cairo draws the pill and the
//! digits onto a layer of their own; the layer goes onto the icon here,
//! in Rust, so the compositing can be tested on plain bytes.

/// The badge's fill: the beak's orange from the app icon.
const ORANGE: (f64, f64, f64) = (0xe8 as f64 / 255.0, 0x66 as f64 / 255.0, 0x0c as f64 / 255.0);

/// What the badge says for `unread`: nothing at zero, the number up to
/// 99, and "99+" past it, which still fits a 16 px icon.
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

/// Where the badge sits on a `size` px icon, as (x, y, width, height),
/// for text `width` px wide: a circle for one digit, a pill for more,
/// against the top right corner and never wider than the icon.
fn pill(size: f64, text_width: f64) -> (f64, f64, f64, f64) {
    let height = (size * 0.5).round().max(9.0);
    let padding = (height * 0.22).max(1.5);
    let width = (text_width + 2.0 * padding).max(height).min(size).round();
    (size - width, 0.0, width, height)
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
        let height = pill(f64::from(size), 0.0).3;
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(height * 0.8);
        let mut options = cairo::FontOptions::new().ok();
        if let Some(options) = options.as_mut() {
            options.set_antialias(Antialias::Gray);
            options.set_hint_style(HintStyle::Slight);
            options.set_hint_metrics(HintMetrics::On);
            cr.set_font_options(options);
        }
        let Ok(extents) = cr.text_extents(text) else {
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
        if cr.show_text(text).is_err() {
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
    use super::{badged, layer, over, pill, text};

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
    fn one_digit_is_a_circle_and_more_is_a_pill_inside_the_icon() {
        let (x, y, width, height) = pill(16.0, 4.0);
        assert_eq!((y, width), (0.0, height));
        assert_eq!(x + width, 16.0);
        let (x, _, width, _) = pill(16.0, 14.0);
        assert_eq!((x, width), (0.0, 16.0), "a wide count stops at the icon's edge");
    }

    #[test]
    fn the_badge_layer_is_orange_in_its_corner_and_clear_elsewhere() {
        for size in [16u32, 22, 24, 32, 48] {
            let pixels = layer(size, "7");
            let side = size as usize;
            assert_eq!(pixels.len(), side * side * 4);
            let at = |x: usize, y: usize| &pixels[(y * side + x) * 4..(y * side + x) * 4 + 4];
            assert_eq!(at(0, side - 1)[0], 0, "{size} px: the bottom left is clear");
            // Just inside the pill's left edge, at mid height, is orange.
            let (x, _, _, height) = pill(f64::from(size), 0.0);
            let edge = at(x as usize + 1, (height / 2.0) as usize);
            assert_eq!(edge[0], 255, "{size} px: {edge:?}");
            assert!(edge[1] > 200 && edge[3] < 60, "{size} px: {edge:?}");
        }
    }

    #[test]
    fn the_number_is_white_inside_the_badge() {
        let pixels = layer(48, "8");
        let side = 48;
        let (x, y, width, height) = pill(48.0, 0.0);
        let mut white = 0;
        for row in y as usize..(y + height) as usize {
            for column in x as usize..(x + width) as usize {
                let p = &pixels[(row * side + column) * 4..(row * side + column) * 4 + 4];
                if p[0] == 255 && p[1] > 240 && p[2] > 240 && p[3] > 240 {
                    white += 1;
                }
            }
        }
        assert!(white > 20, "only {white} white pixels");
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
