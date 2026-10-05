//! The recent databases: where keetui starts without a database path.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, Hit, StatusKind};
use crate::picker::tilde;
use crate::recent::{RecentItem, RecentState, file_name};

use super::{
    ACCENT, DIM, ERR, OK, WARN, btn, buttons, ch, ctrl, file_date, hit, key, modal_with_margin,
    scroll_offset, selection_style, truncate, truncate_start, width,
};

pub fn draw(frame: &mut Frame, app: &App, st: &RecentState) {
    let area = frame.area();
    // Room for every database (or for saying there are none yet), a blank
    // line, and the title, rule, status and buttons around them.
    let rows = if st.items.is_empty() {
        3
    } else {
        st.items.len() as u16
    };
    let size = (
        area.width.saturating_sub(4).min(96),
        area.height.saturating_sub(2).min(rows + 7),
    );
    let inner = modal_with_margin(frame, area, size, " 🔑 keetui ", ACCENT, Margin::new(1, 0));
    if inner.height < 6 || inner.width < 20 {
        return;
    }
    let row = |i: u16| Rect::new(inner.x, inner.y + i, inner.width, 1);

    frame.render_widget(Paragraph::new(Span::raw("Recent databases").bold()), row(0));
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize)).fg(DIM),
        row(1),
    );

    let list = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 4);
    app.page_rows.set(list.height.max(1) as usize);
    if st.items.is_empty() {
        let first = if st.file.is_some() {
            "No recent databases yet: the ones you open or create are listed here."
        } else {
            "Recent databases can't be remembered: HOME is not set."
        };
        let text = vec![
            Line::from(Span::raw(first).fg(DIM)),
            Line::raw(""),
            Line::from(Span::raw("⏎ browse for a database · ^n create a new one").fg(DIM)),
        ];
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }),
            Rect::new(list.x + 2, list.y, list.width - 2, list.height),
        );
    }

    let offset = scroll_offset(
        st.offset.get(),
        Some(st.selected),
        list.height.into(),
        st.items.len(),
    );
    st.offset.set(offset);
    // The names line up in a column as wide as the longest, within reason.
    let name_w = st
        .items
        .iter()
        .map(|it| width(&file_name(&it.path)))
        .max()
        .unwrap_or(0)
        .min(list.width / 2);
    for (r, (i, item)) in st
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(list.height as usize)
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + r as u16, list.width, 1);
        draw_item(frame, rect, item, name_w);
        if i == st.selected {
            frame.buffer_mut().set_style(rect, selection_style(true));
        }
        hit(app, rect, Hit::RecentItem(i));
    }

    // What the last key did, else where the selected database is.
    let status = row(inner.height - 2);
    let line = match (&st.message, st.items.get(st.selected)) {
        (Some((msg, StatusKind::Info)), _) => Span::raw(msg.clone()).fg(OK),
        (Some((msg, StatusKind::Error)), _) => Span::raw(format!("✗ {msg}")).fg(ERR),
        (None, Some(item)) => {
            let mut text = tilde(&item.path);
            if let Some(date) = file_date(item.modified).filter(|_| item.found) {
                text.push_str(&format!(" · modified {date}"));
            }
            Span::raw(truncate_start(&text, status.width as usize)).fg(DIM)
        }
        (None, None) => Span::raw(""),
    };
    frame.render_widget(Paragraph::new(line), status);

    let btns = if st.items.is_empty() {
        vec![
            btn("⏎", "browse", key(KeyCode::Enter)),
            btn("^n", "new database", ctrl('n')),
            btn("esc", "quit", key(KeyCode::Esc)),
        ]
    } else {
        vec![
            btn("⏎", "open", key(KeyCode::Enter)),
            btn("^o", "browse", ctrl('o')),
            btn("^n", "new database", ctrl('n')),
            btn("d", "forget", ch('d')),
            btn("esc", "quit", key(KeyCode::Esc)),
        ]
    };
    buttons(frame, app, inner.x, inner.bottom() - 1, inner.right(), btns);
}

/// One database: its name, its folder, and whether it is missing.
fn draw_item(frame: &mut Frame, rect: Rect, item: &RecentItem, name_w: u16) {
    let note = if item.found { "" } else { "not found" };
    let note_w = if note.is_empty() { 0 } else { width(note) + 2 };
    let name = truncate(&file_name(&item.path), name_w as usize);
    let pad = " ".repeat((name_w - width(&name)) as usize + 2);
    // The folder gets what is left, losing its start if it must; with
    // hardly any room it says nothing useful, and the status line shows it.
    let folder_w = rect.width.saturating_sub(2 + name_w + 2 + note_w);
    let folder = match item.path.parent() {
        Some(dir) if folder_w >= 8 => truncate_start(&tilde(dir), folder_w as usize),
        _ => String::new(),
    };
    let name_style = if item.found {
        Style::new().bold()
    } else {
        Style::new().fg(DIM)
    };
    let line = Line::from(vec![
        Span::raw("  "),
        Span::styled(name, name_style),
        Span::raw(pad),
        Span::raw(folder).fg(DIM),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
    if !note.is_empty() && note_w < rect.width {
        let w = width(note);
        frame.render_widget(
            Paragraph::new(Span::raw(note).fg(WARN)),
            Rect::new(rect.right() - w, rect.y, w, 1),
        );
    }
}
