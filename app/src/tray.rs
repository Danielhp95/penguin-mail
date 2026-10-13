//! The StatusNotifierItem shown by Ubuntu's AppIndicator extension.

mod badge;

use crate::APP_ID;
use mailrs_domain::AccountId;
use mailrs_domain::translate::{fill, fill_plural, gettext};

pub enum TrayCommand {
    Toggle,
    Open,
    /// Show the window on this account's inbox.
    OpenInbox(AccountId),
    Compose,
    Check,
    CheckForUpdates,
    InstallUpdate,
    WhatsNew,
    Quit,
}

/// What the tray says it is holding. A screen reader reads the title, so
/// the count is a sentence rather than a number beside a word.
fn summary(unread: i64) -> String {
    match unread {
        ..=0 => gettext("No unread mail"),
        n => fill_plural(
            "{count} unread message",
            "{count} unread messages",
            n as usize,
            &[("count", &n.to_string())],
        ),
    }
}

/// One account's line in the tray menu. The count sat two spaces after
/// the address, which reads out as a bare number.
fn account_line(email: &str, unread: i64) -> String {
    match unread {
        ..=0 => email.to_string(),
        n => fill_plural(
            "{account}, {count} unread message",
            "{account}, {count} unread messages",
            n as usize,
            &[("account", email), ("count", &n.to_string())],
        ),
    }
}

/// One account's unread INBOX count, with what the tray needs to name it
/// and to open it.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountUnread {
    pub id: AccountId,
    pub email: String,
    pub unread: i64,
}

/// A line above the menu's separator.
#[derive(Debug, PartialEq)]
enum AccountLine {
    /// An account with unread mail; choosing it opens that inbox.
    Unread { id: AccountId, label: String },
    /// Nothing is unread anywhere.
    NoUnread(String),
}

/// The lines above the separator: the accounts with unread mail in the
/// order given, or one line saying there is none. No accounts, no lines.
fn account_lines(accounts: &[AccountUnread]) -> Vec<AccountLine> {
    if accounts.is_empty() {
        return Vec::new();
    }
    let lines: Vec<AccountLine> = accounts
        .iter()
        .filter(|a| a.unread > 0)
        .map(|a| AccountLine::Unread {
            id: a.id,
            label: account_line(&a.email, a.unread),
        })
        .collect();
    if lines.is_empty() {
        vec![AccountLine::NoUnread(summary(0))]
    } else {
        lines
    }
}

/// The app icon at the sizes trays draw, for a host that cannot find the
/// icon by name. A snap's or Flatpak's icons are not on the host, and
/// Cinnamon's tray shows a blank picture when neither the name nor a
/// pixmap gives it one. The colour icon reads on light and dark panels.
const PIXMAPS: [(u32, &[u8]); 6] = [
    (16, include_bytes!("../data/tray/16.png")),
    (22, include_bytes!("../data/tray/22.png")),
    (24, include_bytes!("../data/tray/24.png")),
    (32, include_bytes!("../data/tray/32.png")),
    (48, include_bytes!("../data/tray/48.png")),
    (64, include_bytes!("../data/tray/64.png")),
];

/// `rgba`, rows of `stride` bytes holding `width` pixels of straight
/// RGBA, as the ARGB32 in network byte order that a tray pixmap takes.
/// Empty when `rgba` holds fewer rows than `height`.
fn argb_from_rgba(width: usize, height: usize, stride: usize, rgba: &[u8]) -> Vec<u8> {
    let row = width * 4;
    if height > 0 && (stride < row || rgba.len() < stride * (height - 1) + row) {
        return Vec::new();
    }
    let mut argb = Vec::with_capacity(row * height);
    for y in 0..height {
        for [r, g, b, a] in rgba[y * stride..y * stride + row].as_chunks::<4>().0 {
            argb.extend_from_slice(&[*a, *r, *g, *b]);
        }
    }
    argb
}

/// The name the tray sends for `unread`, and whether a badged pixmap
/// went with it; see `MailTray::icon_name`.
fn icon_name(unread: i64, badged: bool) -> &'static str {
    match (unread, badged) {
        (..=0, _) => "mail-read-symbolic",
        (_, true) => "",
        (_, false) => "mail-unread-symbolic",
    }
}

/// The tray pictures decoded, for `MailTray::pixmaps`. Runs on the GTK
/// thread, where GDK decodes PNG itself.
pub fn pixmaps() -> Vec<ksni::Icon> {
    use gtk::gdk;
    use gtk::prelude::*;
    PIXMAPS
        .iter()
        .filter_map(|(_, png)| {
            let texture = crate::ui::texture::here(&gtk::glib::Bytes::from_static(png))?;
            let mut downloader = gdk::TextureDownloader::new(&texture);
            downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
            let (bytes, stride) = downloader.download_bytes();
            let (width, height) = (texture.width(), texture.height());
            let data = argb_from_rgba(width as usize, height as usize, stride, &bytes);
            (!data.is_empty()).then_some(ksni::Icon {
                width,
                height,
                data,
            })
        })
        .collect()
}

pub struct MailTray {
    pub unread: i64,
    /// Each account's id, address and unread INBOX count.
    pub accounts: Vec<AccountUnread>,
    pub commands: async_channel::Sender<TrayCommand>,
    /// This copy can update itself, so the menu offers a check.
    pub can_update: bool,
    /// A newer release waiting to be installed.
    pub update: Option<String>,
    /// The app icon for a host that cannot find `icon_name`; see `pixmaps`.
    pub pixmaps: Vec<ksni::Icon>,
    /// `pixmaps` with the unread count on them, drawn when the count
    /// changes rather than at each time the host asks.
    pub badged: Vec<ksni::Icon>,
}

impl MailTray {
    fn send(&self, command: TrayCommand) {
        let _ = self.commands.try_send(command);
    }

    /// Takes a new unread count and draws the badge again when what it
    /// says changes.
    pub fn set_unread(&mut self, unread: i64) {
        let redraw = badge::text(unread) != badge::text(self.unread) || self.badged.is_empty();
        self.unread = unread;
        if redraw {
            self.badged = badge::badged(&self.pixmaps, unread);
        }
    }

    fn summary(&self) -> String {
        summary(self.unread)
    }
}

impl ksni::Tray for MailTray {
    fn id(&self) -> String {
        APP_ID.into()
    }

    fn title(&self) -> String {
        fill(
            &gettext("Penguin Mail: {summary}"),
            &[("summary", &self.summary())],
        )
    }

    /// A host draws the icon it finds by this name and passes over the
    /// pixmap, so with mail unread the name goes empty and every host
    /// draws the pixmap that carries the count. With none, the name stays
    /// for a host that draws only themed icons, and so it does when the
    /// pixmaps could not be made, which would leave nothing to draw.
    fn icon_name(&self) -> String {
        icon_name(self.unread, !self.badged.is_empty()).into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        match self.badged.is_empty() {
            true => self.pixmaps.clone(),
            false => self.badged.clone(),
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: fill(
                &gettext("Penguin Mail: {summary}"),
                &[("summary", &self.summary())],
            ),
            description: self
                .accounts
                .iter()
                .filter(|a| a.unread > 0)
                .map(|a| format!("{}: {}", a.email, a.unread))
                .collect::<Vec<_>>()
                .join("\n"),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::Toggle);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};
        let mut items: Vec<MenuItem<Self>> = account_lines(&self.accounts)
            .into_iter()
            .map(|line| match line {
                AccountLine::Unread { id, label } => StandardItem {
                    label,
                    activate: Box::new(move |tray: &mut Self| {
                        tray.send(TrayCommand::OpenInbox(id));
                    }),
                    ..Default::default()
                }
                .into(),
                AccountLine::NoUnread(label) => StandardItem {
                    label,
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            })
            .collect();
        if !items.is_empty() {
            items.push(MenuItem::Separator);
        }
        let item = |label: &str, command: fn() -> TrayCommand| -> MenuItem<Self> {
            StandardItem {
                label: label.into(),
                activate: Box::new(move |tray: &mut Self| tray.send(command())),
                ..Default::default()
            }
            .into()
        };
        items.push(item(&gettext("Open Penguin Mail"), || TrayCommand::Open));
        items.push(item(&gettext("New Message"), || TrayCommand::Compose));
        items.push(item(&gettext("Check for Mail"), || TrayCommand::Check));
        if let Some(version) = &self.update {
            items.push(MenuItem::Separator);
            let install = fill(
                &gettext("Install Update {version}"),
                &[("version", version)],
            );
            let notes = fill(&gettext("What's New in {version}"), &[("version", version)]);
            items.push(item(&install, || TrayCommand::InstallUpdate));
            items.push(item(&notes, || TrayCommand::WhatsNew));
        } else if self.can_update {
            items.push(item(&gettext("Check for Updates"), || {
                TrayCommand::CheckForUpdates
            }));
        }
        items.push(MenuItem::Separator);
        items.push(item(&gettext("Quit"), || TrayCommand::Quit));
        items
    }
}

/// Checks that need GTK, run from the one GTK test in
/// `ui::composer::richbuffer`.
#[cfg(test)]
pub mod checks {
    pub fn run() {
        the_tray_pictures_decode_to_argb();
        the_badge_sits_on_the_real_icons();
        the_penguin_fills_every_picture_with_and_without_the_badge();
        if let Some(dir) = std::env::var_os("PENGUIN_MAIL_TRAY_SHOTS") {
            save_badges(std::path::Path::new(&dir));
        }
    }

    fn the_badge_sits_on_the_real_icons() {
        let plain = super::pixmaps();
        for icon in super::badge::badged(&plain, 7) {
            let w = icon.width as usize;
            // Just inside the badge's left edge, at its middle, is orange
            // over the envelope.
            let (x, y, _, height) = super::badge::pill(w as f64, 0.0);
            let at = (((y + height / 2.0) as usize) * w + x as usize + 1) * 4;
            let pixel = &icon.data[at..at + 4];
            assert!(pixel[0] == 255 && pixel[1] > 200 && pixel[3] < 60, "{w} px: {pixel:?}");
        }
    }

    /// The box around the pixels that are mostly opaque, as (width, height).
    fn opaque_extent(icon: &ksni::Icon) -> (usize, usize) {
        let w = icon.width as usize;
        let (mut left, mut top, mut right, mut bottom) = (w, w, 0, 0);
        for (i, pixel) in icon.data.as_chunks::<4>().0.iter().enumerate() {
            if pixel[0] > 127 {
                let (x, y) = (i % w, i / w);
                (left, top, right, bottom) = (left.min(x), top.min(y), right.max(x + 1), bottom.max(y + 1));
            }
        }
        (right.saturating_sub(left), bottom.saturating_sub(top))
    }

    /// A host draws the picture at the panel's size, so transparent
    /// margins in it make the penguin small beside the other icons.
    fn the_penguin_fills_every_picture_with_and_without_the_badge() {
        let plain = super::pixmaps();
        for icons in [plain.clone(), super::badge::badged(&plain, 3)] {
            for icon in &icons {
                let side = icon.width as usize;
                let (width, height) = opaque_extent(icon);
                assert!(width * 100 >= side * 90, "{side} px: {width} px wide");
                assert!(height * 100 >= side * 70, "{side} px: {height} px tall");
            }
        }
    }

    /// Writes each badged size over a light and a dark panel colour, six
    /// times larger with no smoothing, as PNGs for a person to look at:
    /// `PENGUIN_MAIL_TRAY_SHOTS=dir cargo test` writes them.
    fn save_badges(dir: &std::path::Path) {
        use gtk::gdk;
        use gtk::prelude::*;
        const SCALE: usize = 6;
        const PANELS: [[u8; 3]; 2] = [[0xf6, 0xf5, 0xf4], [0x24, 0x24, 0x24]];
        let plain = super::pixmaps();
        for unread in [1, 7, 42, 120] {
            for icon in super::badge::badged(&plain, unread) {
                let side = icon.width as usize;
                let (width, height) = (side * SCALE * 2 + SCALE * 2, side * SCALE);
                let mut rgba = vec![0u8; width * height * 4];
                for (panel, colour) in PANELS.iter().enumerate() {
                    let left = panel * (side * SCALE + SCALE * 2);
                    for y in 0..height {
                        for x in 0..side * SCALE {
                            let at = (y / SCALE * side + x / SCALE) * 4;
                            let [a, r, g, b] = [icon.data[at], icon.data[at + 1], icon.data[at + 2], icon.data[at + 3]];
                            let mix = |c: u8, under: u8| {
                                ((u32::from(c) * u32::from(a) + u32::from(under) * (255 - u32::from(a))) / 255) as u8
                            };
                            let out = (y * width + left + x) * 4;
                            rgba[out..out + 4].copy_from_slice(&[
                                mix(r, colour[0]),
                                mix(g, colour[1]),
                                mix(b, colour[2]),
                                255,
                            ]);
                        }
                    }
                }
                let texture = gdk::MemoryTexture::new(
                    width as i32,
                    height as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &gtk::glib::Bytes::from_owned(rgba),
                    width * 4,
                );
                let path = dir.join(format!("tray-badge-{unread}-{side}.png"));
                texture.save_to_png(&path).expect("the picture saves");
            }
        }
    }

    fn the_tray_pictures_decode_to_argb() {
        let icons = super::pixmaps();
        let sizes: Vec<i32> = icons.iter().map(|icon| icon.width).collect();
        assert_eq!(sizes, [16, 22, 24, 32, 48, 64]);
        for icon in &icons {
            let (w, h) = (icon.width as usize, icon.height as usize);
            assert_eq!(icon.data.len(), w * h * 4);
            // The middle of the icon is the penguin's face: opaque paper,
            // alpha first.
            let middle = ((h / 2) * w + w / 2) * 4;
            assert_eq!(icon.data[middle], 255, "{w} px icon is opaque in the middle");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountId, AccountLine, AccountUnread, account_line, account_lines, summary};

    #[test]
    fn a_pixmap_turns_rgba_rows_into_argb_and_drops_the_padding() {
        // Two pixels a row, then two bytes of padding to a stride of 10.
        let rgba = [
            1, 2, 3, 4, 5, 6, 7, 8, 0, 0, //
            9, 10, 11, 12, 13, 14, 15, 16, 0, 0,
        ];
        assert_eq!(
            super::argb_from_rgba(2, 2, 10, &rgba),
            [4, 1, 2, 3, 8, 5, 6, 7, 12, 9, 10, 11, 16, 13, 14, 15]
        );
    }

    #[test]
    fn a_pixmap_short_of_rows_is_none() {
        assert_eq!(super::argb_from_rgba(2, 2, 8, &[0; 12]), Vec::<u8>::new());
    }

    #[test]
    fn each_tray_picture_is_the_size_it_is_sent_as() {
        for (size, png) in super::PIXMAPS {
            // A PNG's width and height are the two big-endian words after
            // the signature and the IHDR chunk's length and name.
            let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
            assert_eq!((width, height), (size, size));
        }
    }

    #[test]
    fn with_mail_unread_the_name_steps_aside_for_the_badged_pixmap() {
        assert_eq!(super::icon_name(0, true), "mail-read-symbolic");
        assert_eq!(super::icon_name(5, true), "");
    }

    /// With no picture to send, an empty name would leave every host with
    /// nothing to draw.
    #[test]
    fn with_no_badged_pixmap_the_name_stays() {
        assert_eq!(super::icon_name(5, false), "mail-unread-symbolic");
        assert_eq!(super::icon_name(0, false), "mail-read-symbolic");
    }

    #[test]
    fn a_new_count_draws_the_badge_and_nothing_unread_takes_it_off() {
        let (commands, _received) = async_channel::unbounded();
        let plain = ksni::Icon {
            width: 16,
            height: 16,
            data: vec![255; 16 * 16 * 4],
        };
        let mut tray = super::MailTray {
            unread: 0,
            accounts: Vec::new(),
            commands,
            can_update: false,
            update: None,
            pixmaps: vec![plain.clone()],
            badged: Vec::new(),
        };
        tray.set_unread(7);
        let seven = tray.badged[0].data.clone();
        assert_ne!(seven, plain.data);
        tray.set_unread(42);
        assert_ne!(tray.badged[0].data, seven, "a new number is drawn");
        tray.set_unread(0);
        assert_eq!(tray.badged[0].data, plain.data);
    }

    #[test]
    fn the_tray_counts_unread_mail_in_a_sentence() {
        assert_eq!(summary(0), "No unread mail");
        assert_eq!(summary(1), "1 unread message");
        assert_eq!(summary(7), "7 unread messages");
    }

    #[test]
    fn an_account_line_says_what_its_number_counts() {
        assert_eq!(account_line("ann@example.com", 0), "ann@example.com");
        assert_eq!(
            account_line("ann@example.com", 1),
            "ann@example.com, 1 unread message"
        );
        assert_eq!(
            account_line("ann@example.com", 3),
            "ann@example.com, 3 unread messages"
        );
    }

    fn unread(id: AccountId, email: &str, unread: i64) -> AccountUnread {
        AccountUnread {
            id,
            email: email.into(),
            unread,
        }
    }

    #[test]
    fn the_menu_lists_only_accounts_with_unread_mail_in_their_order() {
        let lines = account_lines(&[
            unread(1, "a@example.com", 0),
            unread(2, "b@example.com", 4),
            unread(3, "c@example.com", 0),
            unread(4, "d@example.com", 1),
        ]);
        assert_eq!(
            lines,
            [
                AccountLine::Unread {
                    id: 2,
                    label: "b@example.com, 4 unread messages".into()
                },
                AccountLine::Unread {
                    id: 4,
                    label: "d@example.com, 1 unread message".into()
                },
            ]
        );
    }

    #[test]
    fn the_menu_says_so_when_nothing_is_unread() {
        let lines = account_lines(&[unread(1, "a@example.com", 0), unread(2, "b@example.com", 0)]);
        assert_eq!(lines, [AccountLine::NoUnread("No unread mail".into())]);
    }

    #[test]
    fn the_menu_lists_nothing_without_accounts() {
        assert_eq!(account_lines(&[]), []);
    }

    #[test]
    fn choosing_an_account_line_asks_to_open_that_inbox() {
        use ksni::Tray;
        use ksni::menu::MenuItem;
        let (commands, received) = async_channel::unbounded();
        let mut tray = super::MailTray {
            unread: 4,
            accounts: vec![unread(1, "a@example.com", 0), unread(2, "b@example.com", 4)],
            commands,
            can_update: false,
            update: None,
            pixmaps: Vec::new(),
            badged: Vec::new(),
        };
        let menu = tray.menu();
        let MenuItem::Standard(line) = &menu[0] else {
            panic!("the first item is an account line");
        };
        assert_eq!(line.label, "b@example.com, 4 unread messages");
        (line.activate)(&mut tray);
        assert!(matches!(
            received.try_recv(),
            Ok(super::TrayCommand::OpenInbox(2))
        ));
        assert!(matches!(menu[1], MenuItem::Separator));
    }
}
