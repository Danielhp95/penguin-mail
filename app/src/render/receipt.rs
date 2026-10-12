//! Read receipts in the page: the line that says a message asked for one,
//! the card that offers to send it, and a receipt that arrived drawn as
//! one line in place of its body.
//!
//! The card is HTML rather than a widget, so it sits where the mockup puts
//! it, between the sender and the text, and scrolls with the message. Its
//! buttons are `mailrs:` links the view intercepts. Mail cannot forge one:
//! the cleaner keeps only http, https, mailto, cid and data links, and a
//! body's shadow root (`contain:content`) cannot draw over the card.

use std::fmt::Write;

use mailrs_domain::ReadReport;
use mailrs_domain::translate::{fill, gettext};

use super::escape;

/// What an article says about a read receipt its sender asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipt<'a> {
    /// The line under the header, and no card: the question was answered,
    /// or the person never wants to be asked.
    Noted,
    /// The line and the card, which asks about a receipt to `to`. While
    /// `working`, the answer is on its way and neither button presses.
    Asked { to: &'a str, working: bool },
}

/// The line above a message that asked for a receipt. It stays after the
/// answer, so the message still says it asked.
pub(super) fn note(html: &mut String) {
    let _ = write!(
        html,
        "<p class=\"receipt-note\"><span class=\"bell\" aria-hidden=\"true\"></span>{}</p>",
        escape(&gettext("Read receipt requested"))
    );
}

/// The card. Neither answer is suggested: sending tells the sender
/// something, and the app leans neither way.
pub(super) fn card(html: &mut String, to: &str, working: bool) {
    let detail = fill(
        &gettext("Sending a receipt tells {address} that you opened this message. Nothing is sent unless you choose."),
        &[("address", to)],
    );
    let _ = write!(
        html,
        "<div class=\"receipt-card\" role=\"group\" aria-label=\"{group}\">\
         <span class=\"receipt-icon\" aria-hidden=\"true\"><span class=\"bell\"></span></span>\
         <div class=\"receipt-words\"><p class=\"receipt-title\">{title}</p>\
         <p class=\"receipt-detail\">{detail}</p></div><div class=\"receipt-buttons\">",
        group = escape(&gettext("Read receipt")),
        title = escape(&gettext("The sender asked to know when you read this")),
        detail = escape(&detail),
    );
    for (answer, label) in [("send", gettext("Send Receipt")), ("decline", gettext("Don't Send"))] {
        match working {
            // No link to follow while the answer goes out, so a second
            // press cannot send a second receipt.
            true => {
                let _ = write!(
                    html,
                    "<a class=\"pill\" role=\"button\" aria-disabled=\"true\">{}</a>",
                    escape(&label)
                );
            }
            false => {
                let _ = write!(
                    html,
                    "<a class=\"pill\" role=\"button\" href=\"mailrs:receipt/{answer}\">{}</a>",
                    escape(&label)
                );
            }
        }
    }
    html.push_str("</div></div>");
}

/// A receipt that arrived, as one line: who read the message, and when.
/// `when` is already in words.
pub(super) fn report_line(html: &mut String, report: &ReadReport, when: &str) {
    // The address goes in bold, so the sentence is escaped first and the
    // address, escaped on its own, goes into its place.
    let who = format!("<strong>{}</strong>", escape(&report.by));
    let (class, said) = match report.displayed {
        true => (
            "receipt-line",
            escape(&fill(&gettext("Read by {who} on {when}"), &[("when", when)])),
        ),
        false => ("receipt-line gone", escape(&gettext("Deleted by {who} without being read"))),
    };
    let _ = write!(
        html,
        "<div class=\"body {class}\"><span class=\"mark\" aria-hidden=\"true\"></span><span>{}</span></div>",
        said.replace("{who}", &who)
    );
}

/// The styles for all three, in the page's theme. Sizes and colours come
/// from the approved mockup (`phase6_mock.py`): the card's 18 px padding,
/// 36 px icon disc and 34 px pills, and the receipt line's 52 px strip in
/// the palette's `ok` green. The accent is the desktop's.
pub(super) fn css(dark: bool) -> String {
    // The mockup tints the icon disc with 14 % of the accent in light and
    // 24 % in dark, and the green strip, its stroke and its disc with 7 %,
    // 25 % and 22 % (28 % in dark) of `ok`.
    let (tint, ok, disc) = match dark {
        true => (24, "#57e389", 28),
        false => (14, "#16683f", 22),
    };
    format!(
        ".receipt-note{{display:flex;align-items:center;gap:5px;margin:-22px 0 18px;color:var(--accent-text);font-size:12.5px;font-weight:600;line-height:19px}}\
.collapsed .receipt-note{{display:none}}\
.bell{{width:14px;height:14px;flex:none;background:currentColor;-webkit-mask:url(\"{BELL}\") center/contain no-repeat}}\
.receipt-card{{display:grid;grid-template-columns:36px minmax(0,1fr);column-gap:14px;margin:20px 0 0;padding:18px;\
border:1px solid color-mix(in srgb,var(--fg) 12%,transparent);border-radius:14px;background:var(--bg)}}\
.receipt-icon{{width:36px;height:36px;border-radius:50%;display:flex;align-items:center;justify-content:center;\
background:color-mix(in srgb,var(--accent) {tint}%,var(--bg));color:var(--accent-text)}}\
.receipt-icon .bell{{width:16px;height:16px}}\
.receipt-title{{margin:0;font-size:15px;font-weight:800;line-height:22px}}\
.receipt-detail{{margin:5px 0 0;color:var(--dim);font-size:13px;font-weight:500;line-height:18px;overflow-wrap:anywhere}}\
.receipt-buttons{{grid-column:2;display:flex;flex-wrap:wrap;gap:8px;margin-top:24px}}\
.pill{{display:inline-flex;align-items:center;justify-content:center;height:34px;padding:0 16px;border-radius:10px;\
background:color-mix(in srgb,var(--fg) 12%,var(--bg));color:var(--fg);font-size:13.5px;font-weight:700;\
text-decoration:none;cursor:pointer;user-select:none;transition:background-color 120ms ease,transform 100ms ease}}\
.pill[href]:hover{{background:color-mix(in srgb,var(--fg) 18%,var(--bg))}}\
.pill[href]:active{{transform:scale(0.97)}}\
.pill[aria-disabled]{{opacity:.5;cursor:default}}\
.receipt-line{{display:flex;align-items:center;gap:12px;min-height:52px;box-sizing:border-box;padding:0 16px;margin-left:0;\
border-radius:12px;font-size:14px;line-height:20px;overflow-wrap:anywhere;\
background:color-mix(in srgb,{ok} 7%,var(--bg));border:1px solid color-mix(in srgb,{ok} 25%,var(--bg))}}\
.receipt-line .mark{{width:24px;height:24px;flex:none;border-radius:50%;background:color-mix(in srgb,{ok} {disc}%,var(--bg));position:relative}}\
.receipt-line .mark::after{{content:\"\";position:absolute;inset:4px;background:{ok};-webkit-mask:url(\"{CHECK}\") center/contain no-repeat}}\
.receipt-line strong{{font-weight:700}}\
.receipt-line.gone{{background:var(--surface);border-color:var(--line)}}\
.receipt-line.gone .mark{{background:var(--card)}}\
.receipt-line.gone .mark::after{{background:var(--dim);-webkit-mask-image:url(\"{GONE}\")}}\
@media (max-width:{narrow}px){{.receipt-title{{line-height:22px}}.receipt-detail{{margin-top:6px}}\
.receipt-buttons{{grid-column:1 / -1;margin-top:16px}}.receipt-buttons .pill{{flex:1 1 0;min-width:max-content}}}}",
        narrow = super::PAGE_NARROW,
    )
}

/// The mockup's bell, as a mask.
const BELL: &str = "data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'>\
<path fill='none' stroke='black' stroke-width='1.5' stroke-linecap='round' stroke-linejoin='round' \
d='M4 11 V7.5 A4 4 0 0 1 12 7.5 V11 L13 12.5 H3 Z M6.5 14 H9.5'/></svg>";

/// The mockup's check, drawn at 1.8 px.
const CHECK: &str = "data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'>\
<path fill='none' stroke='black' stroke-width='1.8' stroke-linecap='round' stroke-linejoin='round' d='M3.5 8.5 L6.5 11.5 L12.5 5'/></svg>";

/// A dash, for a message deleted unread.
const GONE: &str = "data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'>\
<path fill='none' stroke='black' stroke-width='1.8' stroke-linecap='round' d='M4 8 H12'/></svg>";
