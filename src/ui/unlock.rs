//! The unlock and new-database screens.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use crate::app::{
    App, C_CONFIRM, C_KEYFILE, C_PASS, CREATE_FIELD_LABELS, CreateState, Hit, StatusKind,
    TextField, UnlockState,
};

use super::{
    ACCENT, DIM, ERR, OK, WARN, btn, buttons, centered, hit, mask, scroll_window, truncate,
};

const LABEL_W: u16 = 11;

fn key(code: KeyCode, mods: KeyModifiers) -> Hit {
    Hit::Key(KeyEvent::new(code, mods))
}

/// The bordered box shared by both screens; returns its padded inner area.
fn panel(frame: &mut Frame, h: u16, title: &'static str) -> Option<Rect> {
    let modal = centered(66, h, frame.area());
    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::raw(title).fg(ACCENT).bold());
    let inner = block.inner(modal).inner(Margin::new(2, 1));
    frame.render_widget(block, modal);
    // Only draw the form when all of it fits (border + padding = 4 rows).
    (inner.height + 4 >= h).then_some(inner)
}

fn message(working: bool, working_msg: &'static str, error: Option<&String>) -> Paragraph<'static> {
    if working {
        Paragraph::new(Span::raw(working_msg).fg(ACCENT))
    } else if let Some(err) = error {
        Paragraph::new(Span::raw(format!("✗ {err}")).fg(ERR).bold()).wrap(Wrap { trim: true })
    } else {
        Paragraph::new("")
    }
}

fn hint(frame: &mut Frame, inner: Rect, text: &str) {
    let w = text.chars().count() as u16;
    frame.render_widget(
        Paragraph::new(Span::raw(text.to_string()).fg(DIM)),
        Rect::new(inner.right().saturating_sub(w), inner.bottom() - 1, w, 1),
    );
}

pub fn draw(frame: &mut Frame, app: &App, st: &UnlockState) {
    let Some(inner) = panel(frame, 14, " 🔒 keetui ") else {
        return;
    };
    let row = |i: u16| Rect::new(inner.x, inner.y + i, inner.width, 1);

    let name = app
        .db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = app
        .db_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    frame.render_widget(Paragraph::new(Span::raw(name).bold()), row(0));
    frame.render_widget(
        Paragraph::new(Span::raw(truncate(&dir, inner.width as usize)).fg(DIM)),
        row(1),
    );

    let pw_chars: Vec<char> = mask(st.password.text.chars().count()).chars().collect();
    let pw_cursor = input_row(
        frame,
        app,
        row(3),
        0,
        "Password",
        &pw_chars,
        &st.password,
        !st.focus_keyfile,
        None,
    );
    let kf_chars: Vec<char> = st.keyfile.text.chars().collect();
    let kf_cursor = input_row(
        frame,
        app,
        row(4),
        1,
        "Key file",
        &kf_chars,
        &st.keyfile,
        st.focus_keyfile,
        Some("optional"),
    );

    // While locked, a failed save-and-quit reports through the status.
    let status_error = match &app.status {
        Some((msg, StatusKind::Error, _)) => Some(msg),
        _ => None,
    };
    let error = st.error.as_ref().or(status_error);
    let note = match &st.locked {
        Some(reason) if !st.working && error.is_none() => Paragraph::new(
            Span::raw(format!("{reason}. Enter the master password to continue.")).fg(WARN),
        )
        .wrap(Wrap { trim: true }),
        _ => message(st.working, "Unlocking…", error),
    };
    frame.render_widget(
        note,
        Rect {
            height: 2,
            ..row(6)
        },
    );

    let mut btns = vec![btn("⏎", "unlock", key(KeyCode::Enter, KeyModifiers::NONE))];
    // Switching databases would drop the unsaved work held by the lock.
    if !app.locked_with_unsaved_work() {
        btns.push(btn(
            "^o",
            "open other",
            key(KeyCode::Char('o'), KeyModifiers::CONTROL),
        ));
        btns.push(btn(
            "^n",
            "new",
            key(KeyCode::Char('n'), KeyModifiers::CONTROL),
        ));
    }
    btns.push(btn("esc", "quit", key(KeyCode::Esc, KeyModifiers::NONE)));
    buttons(frame, app, inner.x, inner.bottom() - 1, inner.right(), btns);

    if !st.working {
        frame.set_cursor_position(if st.focus_keyfile {
            kf_cursor
        } else {
            pw_cursor
        });
    }
}

pub fn draw_create(frame: &mut Frame, app: &App, st: &CreateState) {
    let Some(inner) = panel(frame, 15, " ✚ New database ") else {
        return;
    };
    let row = |i: u16| Rect::new(inner.x, inner.y + i, inner.width, 1);

    frame.render_widget(
        Paragraph::new(Span::raw("Create an empty KeePass (KDBX4) database.").fg(DIM)),
        row(0),
    );

    let mut cursor = None;
    for (i, label) in CREATE_FIELD_LABELS.iter().enumerate() {
        let field = &st.fields[i];
        let secret = (i == C_PASS || i == C_CONFIRM) && !st.reveal;
        let chars: Vec<char> = if secret {
            mask(field.text.chars().count()).chars().collect()
        } else {
            field.text.chars().collect()
        };
        let placeholder = (i == C_KEYFILE).then_some("optional");
        let r = row(2 + i as u16);
        let p = input_row(
            frame,
            app,
            r,
            i,
            label,
            &chars,
            field,
            st.focus == i,
            placeholder,
        );
        if st.focus == i {
            cursor = Some(p);
        }
        // Live feedback on whether the confirmation matches.
        if i == C_CONFIRM && !field.text.is_empty() {
            let ok = st.fields[C_PASS].text == field.text;
            let (mark, color) = if ok {
                ("✓ match", OK)
            } else {
                ("✗ differs", ERR)
            };
            let w = mark.chars().count() as u16;
            if r.width > LABEL_W + w + 8 {
                frame.render_widget(
                    Paragraph::new(Span::raw(mark).fg(color)),
                    Rect::new(r.right() - w, r.y, w, 1),
                );
            }
        }
    }

    let msg = if !st.working && st.error.is_none() {
        Paragraph::new(Span::raw("There is no way to recover a forgotten master password.").fg(DIM))
    } else {
        message(st.working, "Creating…", st.error.as_ref())
    };
    frame.render_widget(
        msg,
        Rect {
            height: 2,
            ..row(7)
        },
    );

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
            btn("⏎", "create", key(KeyCode::Enter, KeyModifiers::NONE)),
            btn(
                "^r",
                if st.reveal { "hide" } else { "show" },
                key(KeyCode::Char('r'), KeyModifiers::CONTROL),
            ),
            btn("esc", back, key(KeyCode::Esc, KeyModifiers::NONE)),
        ],
    );
    hint(frame, inner, "tab next field");

    if let Some(p) = cursor.filter(|_| !st.working) {
        frame.set_cursor_position(p);
    }
}

/// Draw a labelled input row; returns where the cursor goes when focused.
#[allow(clippy::too_many_arguments)]
fn input_row(
    frame: &mut Frame,
    app: &App,
    row: Rect,
    index: usize,
    label: &str,
    chars: &[char],
    field: &TextField,
    focused: bool,
    placeholder: Option<&str>,
) -> Position {
    hit(app, row, Hit::LoginField(index));
    let label = if focused {
        Line::from(vec![
            Span::raw("▌").fg(ACCENT),
            Span::raw(label.to_string()).fg(ACCENT).bold(),
        ])
    } else {
        Line::from(vec![Span::raw(" "), Span::raw(label.to_string()).fg(DIM)])
    };
    frame.render_widget(Paragraph::new(label), row);
    let x = row.x + LABEL_W;
    let w = row.right().saturating_sub(x);
    let (shown, col) = scroll_window(chars, field.cursor, w as usize);
    let value = match placeholder {
        Some(p) if chars.is_empty() && !focused => Span::raw(p.to_string()).fg(DIM).italic(),
        _ => Span::raw(shown),
    };
    frame.render_widget(Paragraph::new(value), Rect::new(x, row.y, w, 1));
    Position::new(x + col as u16, row.y)
}
