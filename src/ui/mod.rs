mod browser;
mod editor;
mod overlays;
mod picker;
mod unlock;

use chrono::{NaiveDateTime, TimeZone};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Hit, Screen};

pub const ACCENT: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const ERR: Color = Color::Red;

/// Smallest terminal that fits every screen, form and dialog.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 15;

pub fn draw(frame: &mut Frame, app: &App) {
    app.hits.borrow_mut().clear();
    let area = frame.area();
    let too_small = area.width < MIN_WIDTH || area.height < MIN_HEIGHT;
    app.too_small.set(too_small);
    if too_small {
        let text = format!(
            "Terminal too small ({}x{}): keetui needs at least {MIN_WIDTH}x{MIN_HEIGHT}. \
             Ctrl-c quits if nothing is unsaved.",
            area.width, area.height
        );
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
        return;
    }
    match &app.screen {
        Screen::Picker(st) => picker::draw(frame, app, st),
        Screen::Unlock(st) => unlock::draw(frame, app, st),
        Screen::Create(st) => unlock::draw_create(frame, app, st),
        Screen::Browser => browser::draw(frame, app),
        Screen::EntryEdit(form) => {
            browser::draw(frame, app);
            modal_begin(app);
            editor::draw_entry(frame, app, form);
        }
        Screen::GroupEdit(form) => {
            browser::draw(frame, app);
            modal_begin(app);
            editor::draw_group(frame, app, form);
        }
    }
    if let Some(overlay) = &app.overlay {
        modal_begin(app);
        overlays::draw(frame, app, overlay);
    }
}

/// A modal takes over the mouse: only regions it registers are clickable.
fn modal_begin(app: &App) {
    app.hits.borrow_mut().clear();
}

/// Width of the label column in forms: the entry and group editors and
/// the unlock and new-database screens.
pub(crate) const FORM_LABEL_W: u16 = 11;

/// A form field's label: an accent bar and bold when focused, dim otherwise.
pub(crate) fn field_label(frame: &mut Frame, row: Rect, name: &str, focused: bool) {
    let line = if focused {
        Line::from(vec![
            Span::raw("▌").fg(ACCENT),
            Span::raw(name.to_string()).fg(ACCENT).bold(),
        ])
    } else {
        Line::from(vec![Span::raw(" "), Span::raw(name.to_string()).fg(DIM)])
    };
    frame.render_widget(
        Paragraph::new(line),
        Rect {
            height: 1,
            width: FORM_LABEL_W.min(row.width),
            ..row
        },
    );
}

/// A click that replays a key press.
pub(crate) fn key(code: KeyCode) -> Hit {
    Hit::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A click that replays a character key.
pub(crate) fn ch(c: char) -> Hit {
    key(KeyCode::Char(c))
}

/// A click that replays Ctrl plus a character.
pub(crate) fn ctrl(c: char) -> Hit {
    Hit::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

pub(crate) fn hit(app: &App, rect: Rect, h: Hit) {
    app.hits.borrow_mut().push((rect, h));
}

pub(crate) fn hovered(app: &App, rect: Rect) -> bool {
    app.mouse.is_some_and(|p| rect.contains(p))
}

/// Style of the selected row in a list.
pub(crate) fn selection_style(focused: bool) -> Style {
    if focused {
        Style::new()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new()
            .fg(Color::Reset)
            .add_modifier(Modifier::REVERSED)
    }
}

/// A rect of at most `w` x `h`, centered in `area`.
pub(crate) fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Bullets for a secret being typed; one per character.
pub(crate) fn mask(len: usize) -> String {
    "•".repeat(len)
}

/// Placeholder for a stored secret; fixed width so it doesn't leak length.
pub(crate) const HIDDEN: &str = "••••••••";

/// Terminal cells taken by `s`: a CJK character takes two.
pub(crate) fn width(s: &str) -> u16 {
    u16::try_from(s.width()).unwrap_or(u16::MAX)
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Format a UTC timestamp (keepass stores them in UTC) in local time.
pub(crate) fn local_time(utc: NaiveDateTime, format: &str) -> String {
    in_zone(utc, &chrono::Local, format)
}

fn in_zone<Tz: TimeZone>(utc: NaiveDateTime, zone: &Tz, format: &str) -> String
where
    Tz::Offset: std::fmt::Display,
{
    zone.from_utc_datetime(&utc).format(format).to_string()
}

/// Truncate to `max` terminal cells, marking the cut with an ellipsis.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > max - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// `s` without its first `cells` terminal cells (a wide character
/// straddling the cut goes too), and how many cells were dropped.
pub(crate) fn skip_cells(s: &str, cells: usize) -> (&str, usize) {
    let mut skipped = 0;
    for (i, c) in s.char_indices() {
        if skipped >= cells {
            return (&s[i..], skipped);
        }
        skipped += char_width(c);
    }
    ("", skipped)
}

/// The slice of a single-line value that fits in `width` terminal cells
/// while keeping the cursor visible, plus the cursor's column within it.
pub(crate) fn scroll_window(chars: &[char], cursor: usize, width: usize) -> (String, usize) {
    let width = width.max(1);
    // Start just late enough for the text before the cursor, and the
    // cursor's own cell, to fit.
    let mut start = 0;
    let mut before: usize = chars[..cursor].iter().map(|&c| char_width(c)).sum();
    while before + 1 > width && start < cursor {
        before -= char_width(chars[start]);
        start += 1;
    }
    let mut text = String::new();
    let mut used = 0;
    for &c in &chars[start..] {
        let w = char_width(c);
        if used + w > width {
            break;
        }
        text.push(c);
        used += w;
    }
    (text, before)
}

/// A clickable button, `[key label]`. Returns its width.
pub(crate) fn button(
    frame: &mut Frame,
    app: &App,
    x: u16,
    y: u16,
    key: &str,
    label: &str,
    h: Hit,
) -> u16 {
    let text = if key.is_empty() {
        format!("[{label}]")
    } else {
        format!("[{key} {label}]")
    };
    let rect = Rect::new(x, y, width(&text), 1).intersection(frame.area());
    let line = if hovered(app, rect) {
        Line::from(Span::raw(text).fg(Color::Black).bg(ACCENT).bold())
    } else {
        let mut spans = vec![Span::raw("[").fg(DIM)];
        if !key.is_empty() {
            spans.push(Span::raw(format!("{key} ")).fg(DIM));
        }
        spans.push(Span::raw(label.to_string()).fg(ACCENT));
        spans.push(Span::raw("]").fg(DIM));
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), rect);
    hit(app, rect, h);
    rect.width
}

pub(crate) struct Btn<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub hit: Hit,
}

pub(crate) fn btn<'a>(key: &'a str, label: &'a str, hit: Hit) -> Btn<'a> {
    Btn { key, label, hit }
}

fn btn_width(b: &Btn) -> u16 {
    let key = if b.key.is_empty() {
        0
    } else {
        width(b.key) + 1
    };
    key + width(b.label) + 2
}

/// Total width of a row of buttons separated by single spaces.
pub(crate) fn buttons_width(btns: &[Btn]) -> u16 {
    btns.iter().map(btn_width).sum::<u16>() + btns.len().saturating_sub(1) as u16
}

/// Render buttons left to right from `x`; stops before overflowing `max_x`.
pub(crate) fn buttons(frame: &mut Frame, app: &App, x: u16, y: u16, max_x: u16, btns: Vec<Btn>) {
    let mut x = x;
    for b in btns {
        if x + btn_width(&b) > max_x {
            break;
        }
        x += button(frame, app, x, y, b.key, b.label, b.hit) + 1;
    }
}

/// Render buttons right-aligned in `area` (one row); returns the x where
/// they start so callers can fit other content before it.
pub(crate) fn buttons_right(frame: &mut Frame, app: &App, area: Rect, btns: Vec<Btn>) -> u16 {
    let w = buttons_width(&btns);
    if btns.is_empty() || w > area.width {
        return area.right();
    }
    let x = area.right() - w;
    buttons(frame, app, x, area.y, area.right(), btns);
    x
}

/// A bottom key bar of `key label` items; each item is clickable.
pub(crate) fn key_bar(frame: &mut Frame, app: &App, area: Rect, items: Vec<Btn>) {
    let mut x = area.x + 1;
    for item in items {
        let w = width(item.key) + 1 + width(item.label);
        if x + w > area.right() {
            break;
        }
        let rect = Rect::new(x, area.y, w, 1);
        let line = if hovered(app, rect) {
            Line::from(
                Span::raw(format!("{} {}", item.key, item.label))
                    .fg(Color::Black)
                    .bg(ACCENT),
            )
        } else {
            Line::from(vec![
                Span::raw(item.key.to_string()).fg(ACCENT).bold(),
                Span::raw(format!(" {}", item.label)),
            ])
        };
        frame.render_widget(Paragraph::new(line), rect);
        hit(app, rect, item.hit);
        x += w + 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_window_keeps_cursor_visible() {
        let chars: Vec<char> = "abcdefghij".chars().collect();
        assert_eq!(scroll_window(&chars, 3, 5), ("abcde".to_string(), 3));
        assert_eq!(scroll_window(&chars, 10, 5), ("ghij".to_string(), 4));
        assert_eq!(scroll_window(&chars, 7, 5), ("defgh".to_string(), 4));
    }

    #[test]
    fn times_are_shown_in_the_local_zone() {
        let at = |h| {
            chrono::NaiveDate::from_ymd_opt(2026, 10, 4)
                .unwrap()
                .and_hms_opt(h, 44, 0)
                .unwrap()
        };
        let utc_plus_8 = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(
            in_zone(at(14), &utc_plus_8, "%Y-%m-%d %H:%M"),
            "2026-10-04 22:44"
        );
        // Late evening UTC is already the next day here.
        assert_eq!(in_zone(at(20), &utc_plus_8, "%Y-%m-%d"), "2026-10-05");
    }

    /// Terminal cells, as ratatui measures them.
    fn cells(s: &str) -> usize {
        Span::raw(s).width()
    }

    #[test]
    fn measures_terminal_cells_not_chars() {
        let mut wrong = Vec::new();
        // CJK characters take two cells each.
        if width("クレ") != 4 {
            wrong.push(format!("width(\"クレ\") = {}", width("クレ")));
        }
        let cut = truncate("in クレジットカード", 12);
        if cells(&cut) > 12 || !cut.ends_with('…') {
            wrong.push(format!(
                "truncate to 12 gave {cut:?}, {} cells",
                cells(&cut)
            ));
        }
        let chars: Vec<char> = "パスワード".chars().collect();
        let (shown, col) = scroll_window(&chars, chars.len(), 6);
        if cells(&shown) >= 6 || col != cells(&shown) {
            wrong.push(format!("scroll_window(6) gave {shown:?}, cursor at {col}"));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");

        assert_eq!(skip_cells("クレジット", 4), ("ジット", 4));
        // A wide character straddling the cut is dropped whole.
        assert_eq!(skip_cells("クレジット", 3), ("ジット", 4));
    }

    #[test]
    fn truncate_marks_cut() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
    }
}
