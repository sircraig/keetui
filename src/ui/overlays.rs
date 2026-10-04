use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, ConfirmState, GenState, Hit, Overlay, PendingAction, Screen};

use super::{ACCENT, DIM, ERR, WARN, btn, button, buttons, ch, hit, key, modal};

pub fn draw(frame: &mut Frame, app: &App, overlay: &Overlay) {
    match overlay {
        Overlay::Help => draw_help(frame, app),
        Overlay::Generator(st) => draw_generator(frame, app, st),
        Overlay::Confirm(cs) => draw_confirm(frame, app, cs),
    }
}

fn draw_confirm(frame: &mut Frame, app: &App, cs: &ConfirmState) {
    let destructive = matches!(
        cs.pending,
        PendingAction::DeleteEntry(_)
            | PendingAction::DeleteGroup(_)
            | PendingAction::DiscardForm
            | PendingAction::OverwriteExternal { .. }
    );
    let color = if destructive { ERR } else { WARN };
    let inner = modal(frame, frame.area(), (62, 8), " Confirm ", color);
    if inner.height < 2 {
        return;
    }
    frame.render_widget(
        Paragraph::new(cs.prompt.clone()).wrap(Wrap { trim: true }),
        Rect {
            height: inner.height - 1,
            ..inner
        },
    );
    let btns = match cs.pending {
        PendingAction::QuitDirty => vec![
            btn("s", "save & quit", ch('s')),
            btn("d", "discard & quit", ch('d')),
            btn("esc", "cancel", key(KeyCode::Esc)),
        ],
        PendingAction::DeleteEntry(_) | PendingAction::DeleteGroup(_) => {
            vec![btn("y", "delete", ch('y')), btn("n", "cancel", ch('n'))]
        }
        PendingAction::DiscardForm => {
            vec![
                btn("y", "discard", ch('y')),
                btn("n", "keep editing", ch('n')),
            ]
        }
        PendingAction::ConvertFormat { .. } => {
            vec![
                btn("y", "save as KDBX 4.1", ch('y')),
                btn("n", "cancel", ch('n')),
            ]
        }
        PendingAction::OverwriteExternal { .. } => {
            vec![btn("y", "overwrite", ch('y')), btn("n", "cancel", ch('n'))]
        }
    };
    buttons(frame, app, inner.x, inner.bottom() - 1, inner.right(), btns);
}

fn draw_generator(frame: &mut Frame, app: &App, st: &GenState) {
    let inner = modal(
        frame,
        frame.area(),
        (60, 10),
        " Password generator ",
        ACCENT,
    );
    if inner.height < 5 {
        return;
    }
    let row = |i: u16| Rect::new(inner.x, inner.y + i, inner.width, 1);

    // Length: [-] 20 [+]
    let r = row(0);
    frame.render_widget(Paragraph::new(Span::raw("Length").fg(DIM)), r);
    let mut x = r.x + 9;
    x += button(frame, app, x, r.y, "", "−", ch('-')) + 1;
    let len = format!("{:>3}", st.opts.length);
    frame.render_widget(
        Paragraph::new(Span::raw(len.clone()).bold()),
        Rect::new(x, r.y, 3, 1),
    );
    x += 4;
    button(frame, app, x, r.y, "", "+", ch('+'));

    // Character classes, toggled with 1-4 or by clicking.
    let r = row(1);
    frame.render_widget(Paragraph::new(Span::raw("Use").fg(DIM)), r);
    let classes = [
        ('1', "a-z", st.opts.lower),
        ('2', "A-Z", st.opts.upper),
        ('3', "0-9", st.opts.digits),
        ('4', "#$%", st.opts.symbols),
    ];
    let mut x = r.x + 9;
    for (k, name, on) in classes {
        let text = format!("{} {name}", if on { "■" } else { "□" });
        let w = text.chars().count() as u16;
        if x + w > r.right() {
            break;
        }
        let rect = Rect::new(x, r.y, w, 1);
        let style = if super::hovered(app, rect) {
            Style::new().fg(ratatui::style::Color::Black).bg(ACCENT)
        } else if on {
            Style::new().fg(ACCENT)
        } else {
            Style::new().fg(DIM)
        };
        frame.render_widget(Paragraph::new(Span::styled(text, style)), rect);
        hit(app, rect, ch(k));
        x += w + 2;
    }

    // Preview
    let r = row(3);
    frame.render_widget(
        Paragraph::new(Span::raw(st.preview.to_string()).bold().fg(WARN)),
        r,
    );

    let editing = matches!(app.screen, Screen::EntryEdit(_));
    buttons(
        frame,
        app,
        inner.x,
        inner.bottom() - 1,
        inner.right(),
        vec![
            btn(
                "⏎",
                if editing { "use" } else { "copy" },
                key(KeyCode::Enter),
            ),
            btn("r", "regenerate", ch('r')),
            btn("esc", "close", key(KeyCode::Esc)),
        ],
    );
}

fn draw_help(frame: &mut Frame, app: &App) {
    // Any click closes help.
    hit(app, frame.area(), Hit::Dismiss);
    let inner = modal(frame, frame.area(), (76, 33), " Help ", ACCENT);

    let sections: &[(&str, &[(&str, &str)])] = &[
        (
            "Navigate",
            &[
                ("j/k ↑/↓", "move"),
                ("PgUp/PgDn g/G", "page · top / bottom"),
                ("h/l ←/→", "collapse/expand groups · switch pane"),
                ("Tab", "switch between groups and entries"),
                ("/", "search all entries · Esc ends the search"),
            ],
        ),
        (
            "Selected entry",
            &[
                ("⏎ or c", "copy password"),
                ("y / t / u", "copy username / TOTP code / URL"),
                ("o", "open URL in the browser"),
                ("r", "show/hide password"),
                ("e / d", "edit / delete"),
            ],
        ),
        (
            "Database",
            &[
                ("a / A", "new entry / new group"),
                ("e (groups)", "rename group"),
                ("Ctrl-g", "password generator"),
                ("Ctrl-s", "save · q quit"),
                ("Ctrl-l", "lock"),
            ],
        ),
        (
            "Mouse",
            &[
                ("click", "select · click ▸ to expand · buttons act"),
                ("double-click", "entry: copy password · group: expand"),
                ("wheel", "scroll lists · Shift+drag selects text"),
            ],
        ),
    ];
    let mut lines: Vec<Line> = Vec::new();
    for (title, rows) in sections {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::raw(*title).bold()));
        for (k, v) in *rows {
            lines.push(Line::from(vec![
                Span::raw(format!("  {k:<14}")).fg(ACCENT),
                Span::raw(*v),
            ]));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(
        Span::raw(
            "Copied secrets clear from the clipboard after 15s or when keetui exits. \
             Press any key to close.",
        )
        .fg(DIM),
    ));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
