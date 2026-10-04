use std::time::UNIX_EPOCH;

use chrono::DateTime;
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::app::{App, Hit};
use crate::picker::{ItemKind, PickerState, size_label, tilde};

use super::{
    ACCENT, DIM, ERR, btn, buttons, centered, ctrl, hit, key, scroll_window, selection_style, width,
};

pub fn draw(frame: &mut Frame, app: &App, st: &PickerState) {
    let area = frame.area();
    let modal = centered(
        area.width.saturating_sub(4).min(96),
        area.height.saturating_sub(2).min(30),
        area,
    );
    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::raw(" 📂 Open database ").fg(ACCENT).bold());
    let inner = block.inner(modal).inner(Margin::new(1, 0));
    frame.render_widget(block, modal);
    if inner.height < 7 || inner.width < 20 {
        return;
    }
    let row = |i: u16| Rect::new(inner.x, inner.y + i, inner.width, 1);

    // Current folder, trimmed from the left so the deepest part stays visible.
    let dir = tilde(&st.dir);
    let chars: Vec<char> = dir.chars().collect();
    let (shown, _) = scroll_window(&chars, chars.len().saturating_sub(1), inner.width as usize);
    frame.render_widget(Paragraph::new(Span::raw(shown).bold()), row(0));

    // Filter box.
    let filter_row = row(1);
    let prompt = Span::raw("/ ").fg(ACCENT).bold();
    let line = if st.filter.is_empty() {
        Line::from(vec![
            prompt,
            Span::raw("type to filter · ~ home · / root").fg(DIM),
        ])
    } else {
        Line::from(vec![prompt, Span::raw(st.filter.clone())])
    };
    frame.render_widget(Paragraph::new(line), filter_row);
    let cursor_x = (filter_row.x + 2 + width(&st.filter)).min(filter_row.right().saturating_sub(1));
    frame.set_cursor_position(Position::new(cursor_x, filter_row.y));
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize)).fg(DIM),
        row(2),
    );

    // Listing.
    let list = Rect::new(inner.x, inner.y + 3, inner.width, inner.height - 5);
    app.page_rows.set(list.height.max(1) as usize);
    let visible = st.visible();
    let mut offset = st.offset.get();
    if st.selected < offset {
        offset = st.selected;
    } else if st.selected >= offset + list.height as usize {
        offset = st.selected + 1 - list.height as usize;
    }
    st.offset.set(offset);

    for (r, (vi, &ii)) in visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(list.height as usize)
        .enumerate()
    {
        let item = &st.items[ii];
        let rect = Rect::new(list.x, list.y + r as u16, list.width, 1);
        let name = match item.kind {
            ItemKind::Parent => Line::from(Span::raw("↰ ..").fg(DIM)),
            ItemKind::Dir => Line::from(vec![
                Span::raw("▸ ").fg(DIM),
                Span::raw(format!("{}/", item.name)).fg(ACCENT),
            ]),
            ItemKind::File => {
                Line::from(vec![Span::raw("  "), Span::raw(item.name.clone()).bold()])
            }
        };
        frame.render_widget(Paragraph::new(name), rect);
        if item.kind == ItemKind::File {
            let date = item
                .modified
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| DateTime::from_timestamp(d.as_secs() as i64, 0))
                .map(|t| super::local_time(t.naive_utc(), "%Y-%m-%d"))
                .unwrap_or_default();
            let meta = format!("{:>9}  {date}", size_label(item.size));
            let w = width(&meta);
            if w + 20 < rect.width {
                frame.render_widget(
                    Paragraph::new(Span::raw(meta).fg(DIM)),
                    Rect::new(rect.right() - w, rect.y, w, 1),
                );
            }
        }
        if vi == st.selected {
            frame.buffer_mut().set_style(rect, selection_style(true));
        }
        hit(app, rect, Hit::PickerItem(vi));
    }

    let databases = visible
        .iter()
        .filter(|&&i| st.items[i].kind == ItemKind::File)
        .count();
    if databases == 0 {
        let msg = if st.filter.is_empty() {
            "No databases in this folder — open a folder, or ^n to create one."
        } else {
            "Nothing matches the filter."
        };
        let y = list.y + visible.len().saturating_sub(offset) as u16 + 1;
        if y < list.bottom() {
            frame.render_widget(
                Paragraph::new(Span::raw(msg).fg(DIM)),
                Rect::new(list.x + 2, y, list.width.saturating_sub(2), 1),
            );
        }
    }

    // Error or summary line, then buttons.
    let status = row(inner.height - 2);
    let line = match &st.error {
        Some(e) => Span::raw(format!("✗ {e}")).fg(ERR),
        None => Span::raw(match databases {
            0 => String::new(),
            1 => "1 database".into(),
            n => format!("{n} databases"),
        })
        .fg(DIM),
    };
    frame.render_widget(Paragraph::new(line), status);

    let back = if app.db_path.is_file() {
        "back"
    } else {
        "quit"
    };
    buttons(
        frame,
        app,
        inner.x,
        inner.bottom() - 1,
        inner.right(),
        vec![
            btn("⏎", "open", key(KeyCode::Enter)),
            btn("⌫", "up", key(KeyCode::Left)),
            btn("^n", "new database", ctrl('n')),
            btn(
                "^a",
                if st.show_all { "kdbx only" } else { "show all" },
                ctrl('a'),
            ),
            btn("esc", back, key(KeyCode::Esc)),
        ],
    );
}
