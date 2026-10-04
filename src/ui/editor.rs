use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::app::{App, ENTRY_FIELD_LABELS, EntryForm, F_NOTES, F_OTP, F_PASS, GroupForm, Hit};

use super::{
    ACCENT, DIM, btn, buttons, buttons_right, centered, hit, mask, scroll_window, skip_cells, width,
};

const LABEL_W: u16 = 11;

fn ctrl(c: char) -> Hit {
    Hit::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn key(code: KeyCode) -> Hit {
    Hit::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn modal_block(title: &'static str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::raw(title).fg(ACCENT).bold())
}

pub fn draw_entry(frame: &mut Frame, app: &App, form: &EntryForm) {
    let modal = centered(80, 22, above_status(frame.area()));
    frame.render_widget(Clear, modal);
    let title = if form.target.is_some() {
        " Edit entry "
    } else {
        " New entry "
    };
    let block = modal_block(title);
    let inner = block.inner(modal).inner(Margin::new(1, 1));
    frame.render_widget(block, modal);
    if inner.height < 4 {
        return;
    }

    let footer_y = inner.bottom() - 1;
    let value_x = inner.x + LABEL_W;
    let value_w = inner.right().saturating_sub(value_x);
    let mut cursor: Option<Position> = None;

    // Single-line fields.
    for (i, name) in ENTRY_FIELD_LABELS.iter().enumerate().take(F_NOTES) {
        let y = inner.y + i as u16;
        if y >= footer_y {
            break;
        }
        let row = Rect::new(inner.x, y, inner.width, 1);
        let focused = form.focus == i;
        hit(app, row, Hit::EditorField(i));
        draw_label(frame, row, name, focused);

        let mut w = value_w;
        if i == F_PASS {
            let bx = buttons_right(
                frame,
                app,
                Rect::new(value_x, y, value_w, 1),
                vec![
                    btn("^r", if form.reveal { "hide" } else { "show" }, ctrl('r')),
                    btn("^g", "generate", ctrl('g')),
                ],
            );
            w = bx.saturating_sub(value_x + 1);
        }
        let field = &form.fields[i];
        // The TOTP secret is as sensitive as the password: anyone who sees it
        // can generate codes indefinitely.
        let secret = i == F_PASS || i == F_OTP;
        let chars: Vec<char> = if secret && !form.reveal {
            mask(field.text.chars().count()).chars().collect()
        } else {
            field.text.chars().collect()
        };
        let (shown, col) = scroll_window(&chars, field.cursor, w as usize);
        frame.render_widget(Paragraph::new(shown), Rect::new(value_x, y, w, 1));
        if focused {
            cursor = Some(Position::new(value_x + col as u16, y));
        }
    }

    // Notes: multi-line, scrolled to keep the cursor visible.
    let notes_y = inner.y + F_NOTES as u16;
    let notes_h = footer_y.saturating_sub(notes_y + 1);
    if notes_h > 0 {
        let notes_rect = Rect::new(inner.x, notes_y, inner.width, notes_h);
        let focused = form.focus == F_NOTES;
        hit(app, notes_rect, Hit::EditorField(F_NOTES));
        draw_label(frame, notes_rect, "Notes", focused);

        let field = &form.fields[F_NOTES];
        let before: String = field.text.chars().take(field.cursor).collect();
        let cur_line = before.matches('\n').count();
        // Columns are terminal cells: CJK characters take two.
        let cur_prefix = before.rsplit('\n').next().unwrap_or("");
        let cur_col = usize::from(width(cur_prefix));
        let top = (cur_line + 1).saturating_sub(notes_h as usize);
        // At least one column, as scroll_window does: with none (a very
        // narrow terminal) the cursor arithmetic below would underflow.
        let hscroll = (cur_col + 1).saturating_sub(usize::from(value_w.max(1)));
        let lines: Vec<Line> = field
            .text
            .split('\n')
            .skip(top)
            .take(notes_h as usize)
            .map(|l| Line::raw(skip_cells(l, hscroll).0.to_string()))
            .collect();
        let empty = field.text.is_empty();
        let body = if empty && !focused {
            Paragraph::new(Span::raw("—").fg(DIM))
        } else {
            Paragraph::new(lines)
        };
        frame.render_widget(body, Rect::new(value_x, notes_y, value_w, notes_h));
        if focused {
            let skipped = skip_cells(cur_prefix, hscroll).1;
            cursor = Some(Position::new(
                value_x + cur_col.saturating_sub(skipped) as u16,
                notes_y + (cur_line - top) as u16,
            ));
        }
    }

    // Footer: buttons plus a hint for the focused field.
    buttons(
        frame,
        app,
        inner.x,
        footer_y,
        inner.right(),
        vec![
            btn("^s", "save", ctrl('s')),
            btn("esc", "cancel", key(KeyCode::Esc)),
        ],
    );
    let hint = match form.focus {
        F_OTP => "otpauth:// URL or base32 secret · ^r show",
        F_NOTES => "⏎ new line · tab next field",
        _ => "tab/↑↓ next field · ^u clear",
    };
    let hint_w = hint.chars().count() as u16;
    if hint_w + 28 < inner.width {
        frame.render_widget(
            Paragraph::new(Span::raw(hint).fg(DIM)),
            Rect::new(inner.right() - hint_w, footer_y, hint_w, 1),
        );
    }

    if let Some(p) = cursor {
        let x = p.x.min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(x, p.y));
    }
}

/// The screen minus the status line and key bar at the bottom: saving a
/// form reports errors in the status line, so an editor must not cover it.
fn above_status(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(2),
        ..area
    }
}

fn draw_label(frame: &mut Frame, row: Rect, name: &str, focused: bool) {
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
            width: LABEL_W.min(row.width),
            ..row
        },
    );
}

pub fn draw_group(frame: &mut Frame, app: &App, form: &GroupForm) {
    let modal = centered(56, 8, above_status(frame.area()));
    frame.render_widget(Clear, modal);
    let title = if form.target.is_some() {
        " Rename group "
    } else {
        " New group "
    };
    let block = modal_block(title);
    let inner = block.inner(modal).inner(Margin::new(1, 1));
    frame.render_widget(block, modal);
    if inner.height < 3 {
        return;
    }

    let parent = app
        .vault
        .as_ref()
        .map(|v| v.group_path(form.parent))
        .map(|p| {
            if p.is_empty() {
                "(root)".to_string()
            } else {
                p
            }
        })
        .unwrap_or_default();
    if form.target.is_none() {
        frame.render_widget(
            Paragraph::new(Span::raw(format!("inside {parent}")).fg(DIM).italic()),
            Rect::new(
                inner.x + LABEL_W,
                inner.y + 1,
                inner.width.saturating_sub(LABEL_W),
                1,
            ),
        );
    }

    let row = Rect::new(inner.x, inner.y, inner.width, 1);
    draw_label(frame, row, "Name", true);
    let w = inner.width.saturating_sub(LABEL_W);
    let chars: Vec<char> = form.name.text.chars().collect();
    let (shown, col) = scroll_window(&chars, form.name.cursor, w as usize);
    frame.render_widget(
        Paragraph::new(shown),
        Rect::new(inner.x + LABEL_W, inner.y, w, 1),
    );
    frame.set_cursor_position(Position::new(inner.x + LABEL_W + col as u16, inner.y));

    buttons(
        frame,
        app,
        inner.x,
        inner.bottom() - 1,
        inner.right(),
        vec![
            btn("⏎", "save", key(KeyCode::Enter)),
            btn("esc", "cancel", key(KeyCode::Esc)),
        ],
    );
}
