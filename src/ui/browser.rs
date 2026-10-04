use std::time::{SystemTime, UNIX_EPOCH};

use chrono::NaiveDateTime;
use keepass::db::{Times, fields};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Hit, Pane, Screen, StatusKind};
use crate::event::Action;

use super::{
    ACCENT, Btn, DIM, ERR, HIDDEN, OK, WARN, btn, buttons, buttons_right, buttons_width, hit,
    hovered, key, key_bar, scroll_window, selection_style, truncate, width,
};

const LABEL_W: u16 = 10;
const STANDARD_FIELDS: [&str; 6] = [
    fields::TITLE,
    fields::USERNAME,
    fields::PASSWORD,
    fields::URL,
    fields::NOTES,
    fields::OTP,
];

pub fn draw(frame: &mut Frame, app: &App) {
    let [main, status, keys] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [groups_area, entries_area, detail_area] = Layout::horizontal([
        Constraint::Percentage(22),
        Constraint::Percentage(33),
        Constraint::Percentage(45),
    ])
    .areas(main);

    draw_groups(frame, app, groups_area);
    draw_entries(frame, app, entries_area);
    draw_detail(frame, app, detail_area);
    draw_status(frame, app, status);
    draw_keys(frame, app, keys);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let (border, title) = if focused {
        (Style::new().fg(ACCENT), Span::raw(title).fg(ACCENT).bold())
    } else {
        (Style::new().fg(DIM), Span::raw(title))
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title)
}

// -- groups -------------------------------------------------------------------

fn draw_groups(frame: &mut Frame, app: &App, area: Rect) {
    let Some(v) = &app.vault else { return };
    let focused = app.pane == Pane::Groups;
    hit(app, area, Hit::Pane(Pane::Groups));
    let block = pane_block(" Groups ".into(), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let bin = v.db.recycle_bin().map(|g| g.id());
    let items: Vec<ListItem> = app
        .group_rows
        .iter()
        .map(|(id, depth)| {
            let Some(g) = v.db.group(*id) else {
                return ListItem::new("?");
            };
            let marker = if g.group_ids().next().is_none() {
                "  "
            } else if app.expanded.contains(id) {
                "▾ "
            } else {
                "▸ "
            };
            let name = if g.name.is_empty() { "(root)" } else { &g.name };
            let mut spans = vec![Span::raw("  ".repeat(*depth)), Span::raw(marker).fg(DIM)];
            if Some(*id) == bin {
                spans.push(Span::raw(format!("🗑 {name}")).fg(DIM));
            } else {
                spans.push(Span::raw(name.to_string()));
            }
            let count = g.entries().count();
            if count > 0 {
                spans.push(Span::raw(format!(" {count}")).fg(DIM));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let selected = app
        .group_rows
        .iter()
        .position(|(g, _)| Some(*g) == app.sel_group);
    let mut state = ListState::default()
        .with_offset(app.group_offset.get())
        .with_selected(selected);
    let list = List::new(items).highlight_style(selection_style(focused));
    frame.render_stateful_widget(list, inner, &mut state);
    app.group_offset.set(state.offset());

    for row in 0..inner.height {
        let Some((id, depth)) = app.group_rows.get(state.offset() + row as usize) else {
            break;
        };
        let y = inner.y + row;
        hit(app, Rect::new(inner.x, y, inner.width, 1), Hit::Group(*id));
        let marker_x = inner.x + 2 * *depth as u16;
        if marker_x + 2 <= inner.right() {
            hit(app, Rect::new(marker_x, y, 2, 1), Hit::GroupToggle(*id));
        }
    }
}

// -- entries ------------------------------------------------------------------

fn draw_entries(frame: &mut Frame, app: &App, area: Rect) {
    let Some(v) = &app.vault else { return };
    let focused = app.pane == Pane::Entries;
    hit(app, area, Hit::Pane(Pane::Entries));

    let searching = app.search.as_deref().is_some_and(|q| !q.trim().is_empty());
    let n = app.entry_rows.len();
    let title = if searching {
        format!(" Search · {n} match{} ", if n == 1 { "" } else { "es" })
    } else if app.search.is_some() {
        " Search ".to_string()
    } else {
        let name = app
            .sel_group
            .and_then(|g| v.db.group(g))
            .map(|g| {
                if g.name.is_empty() {
                    "(root)".to_string()
                } else {
                    g.name.clone()
                }
            })
            .unwrap_or_default();
        format!(" {name} · {n} ")
    };
    let block = pane_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [search_row, rule, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    draw_search_box(frame, app, search_row);
    frame.render_widget(
        Paragraph::new("─".repeat(rule.width as usize)).fg(DIM),
        rule,
    );
    app.page_rows.set(list_area.height.max(1) as usize);

    if app.entry_rows.is_empty() {
        let msg = if searching {
            "No matching entries."
        } else if app.search.is_some() {
            "Type to search every group."
        } else {
            "No entries in this group."
        };
        let area = list_area.inner(Margin::new(1, 0));
        frame.render_widget(Paragraph::new(Span::raw(msg).fg(DIM)), area);
        if app.search.is_none() && area.height > 2 {
            buttons(
                frame,
                app,
                area.x,
                area.y + 2,
                area.right(),
                vec![btn("a", "new entry", Hit::Act(Action::NewEntry))],
            );
        }
        return;
    }

    let items: Vec<ListItem> = app
        .entry_rows
        .iter()
        .map(|id| {
            let Some(e) = v.db.entry(*id) else {
                return ListItem::new("?");
            };
            let mut spans = match e.get_title() {
                Some(t) if !t.is_empty() => vec![Span::raw(t.to_string())],
                _ => vec![Span::raw("(untitled)").fg(DIM).italic()],
            };
            let user = e.get_username().unwrap_or("");
            if !user.is_empty() {
                spans.push(Span::raw(format!("  {user}")).fg(DIM));
            }
            if searching {
                let path = v.group_path(e.parent().id());
                if !path.is_empty() {
                    spans.push(Span::raw(format!("  · {path}")).fg(DIM).italic());
                }
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let selected = app
        .entry_rows
        .iter()
        .position(|e| Some(*e) == app.sel_entry);
    let mut state = ListState::default()
        .with_offset(app.entry_offset.get())
        .with_selected(selected);
    let list = List::new(items).highlight_style(selection_style(focused));
    frame.render_stateful_widget(list, list_area, &mut state);
    app.entry_offset.set(state.offset());

    for row in 0..list_area.height {
        let Some(id) = app.entry_rows.get(state.offset() + row as usize) else {
            break;
        };
        let r = Rect::new(list_area.x, list_area.y + row, list_area.width, 1);
        hit(app, r, Hit::Entry(*id));
    }
}

fn draw_search_box(frame: &mut Frame, app: &App, area: Rect) {
    let prompt_w = 3;
    match &app.search {
        Some(q) => {
            let chars: Vec<char> = q.chars().collect();
            let avail = area.width.saturating_sub(prompt_w + 1) as usize;
            let (shown, cursor) = scroll_window(&chars, chars.len(), avail);
            let prompt = Span::raw(" / ").fg(ACCENT).bold();
            let query = if app.search_input {
                Span::raw(shown)
            } else {
                Span::raw(shown).bold()
            };
            frame.render_widget(Paragraph::new(Line::from(vec![prompt, query])), area);
            hit(app, area, Hit::Act(Action::Search));
            if app.search_input {
                let x = (area.x + prompt_w + cursor as u16).min(area.right().saturating_sub(1));
                frame.set_cursor_position(Position::new(x, area.y));
                if chars.is_empty() {
                    frame.render_widget(
                        Paragraph::new(Span::raw("title, user, URL, notes, tags…").fg(DIM)),
                        Rect {
                            x: area.x + prompt_w,
                            width: area.width.saturating_sub(prompt_w),
                            ..area
                        },
                    );
                }
            } else {
                buttons_right(
                    frame,
                    app,
                    area,
                    vec![btn("esc", "clear", Hit::Act(Action::Escape))],
                );
            }
        }
        None => {
            let style = if hovered(app, area) { ACCENT } else { DIM };
            let line = Line::from(vec![
                Span::raw(" / ").fg(style).bold(),
                Span::raw("Search all entries").fg(style),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            hit(app, area, Hit::Act(Action::Search));
        }
    }
}

// -- detail -------------------------------------------------------------------

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(v) = &app.vault else { return };
    let entry = app.sel_entry.and_then(|id| v.db.entry(id));
    let title = entry
        .as_ref()
        .and_then(|e| e.get_title())
        .filter(|t| !t.is_empty())
        .unwrap_or("Entry");
    let block = pane_block(format!(" {title} "), false);
    let inner = block.inner(area).inner(Margin::new(1, 0));
    frame.render_widget(block, area);

    let Some(e) = entry else {
        frame.render_widget(
            Paragraph::new(Span::raw("No entry selected.").fg(DIM)),
            inner,
        );
        return;
    };
    if inner.height == 0 {
        return;
    }

    let mut y = inner.y;
    let row = |y: u16| Rect::new(inner.x, y, inner.width, 1);
    let bottom = inner.bottom();

    // Location and entry-level actions.
    let path = v.group_path(e.parent().id());
    let path = if path.is_empty() {
        "(root)".to_string()
    } else {
        path
    };
    // e and d act on the focused pane (in the groups pane they rename and
    // delete the group), so only show them as the entry's keys while the
    // entries pane has focus. Clicking the buttons acts on the entry anyway.
    let (edit_key, delete_key) = if app.pane == Pane::Entries {
        ("e", "d")
    } else {
        ("", "")
    };
    let bx = buttons_right(
        frame,
        app,
        row(y),
        vec![
            btn(edit_key, "edit", Hit::EntryAct(Action::Edit)),
            btn(delete_key, "delete", Hit::EntryAct(Action::Delete)),
        ],
    );
    let path_w = bx.saturating_sub(inner.x + 1) as usize;
    frame.render_widget(
        Paragraph::new(
            Span::raw(truncate(&format!("in {path}"), path_w))
                .fg(DIM)
                .italic(),
        ),
        row(y),
    );
    y += 2;

    let mut field = |r: Row<'_>| {
        if y < bottom {
            field_row(frame, app, row(y), r);
            y += 1;
        }
    };
    let missing = |label| Row::new(label, "—", Style::new().fg(DIM));

    let user = e.get_username().unwrap_or("");
    if user.is_empty() {
        field(missing("Username"));
    } else {
        let copy = Hit::EntryAct(Action::CopyUser);
        field(
            Row::new("Username", user, Style::new())
                .hit(copy.clone())
                .buttons(vec![btn("y", "copy", copy)]),
        );
    }

    let pw = e.get_password().unwrap_or("");
    if pw.is_empty() {
        field(missing("Password"));
    } else {
        let (shown, style) = if app.reveal {
            (pw, Style::new().fg(WARN))
        } else {
            (HIDDEN, Style::new())
        };
        let reveal = if app.reveal { "hide" } else { "show" };
        field(
            Row::new("Password", shown, style)
                .hit(Hit::EntryAct(Action::CopyPass))
                .buttons(vec![
                    btn("r", reveal, Hit::EntryAct(Action::ToggleReveal)),
                    btn("c", "copy", Hit::EntryAct(Action::CopyPass)),
                ]),
        );
    }

    let url = e.get_url().unwrap_or("");
    if url.is_empty() {
        field(missing("URL"));
    } else {
        field(
            Row::new("URL", url, Style::new().fg(ACCENT).underlined())
                .hit(Hit::EntryAct(Action::OpenUrl))
                .buttons(vec![
                    btn("o", "open", Hit::EntryAct(Action::OpenUrl)),
                    btn("u", "copy", Hit::EntryAct(Action::CopyUrl)),
                ]),
        );
    }

    if let Some(raw) = e.get_raw_otp_value() {
        match crate::totp::parse(raw).map(|t| t.value_now()) {
            Ok(Ok(code)) => {
                let left = code.valid_for.as_secs();
                let period = code.period.as_secs().max(1);
                let color = match left {
                    0..=5 => ERR,
                    6..=10 => WARN,
                    _ => OK,
                };
                let filled = ((left * 8).div_ceil(period)) as usize;
                let countdown = vec![
                    Span::raw("  "),
                    Span::raw("━".repeat(filled)).fg(color),
                    Span::raw("━".repeat(8 - filled.min(8))).fg(DIM),
                    Span::raw(format!(" {left:>2}s")).fg(color),
                ];
                let copy = Hit::EntryAct(Action::CopyOtp);
                field(
                    Row::new(
                        "TOTP",
                        group_code(&code.code),
                        Style::new().fg(ACCENT).bold(),
                    )
                    .suffix(countdown)
                    .hit(copy.clone())
                    .buttons(vec![btn("t", "copy", copy)]),
                );
            }
            Ok(Err(_)) => field(Row::new("TOTP", "clock error", Style::new().fg(ERR))),
            Err(_) => field(Row::new("TOTP", "invalid OTP data", Style::new().fg(ERR))),
        }
    }

    // Custom string fields (KeePass "advanced" attributes).
    let mut custom: Vec<(&String, _)> = e
        .fields
        .iter()
        .filter(|(k, _)| !STANDARD_FIELDS.contains(&k.as_str()))
        .collect();
    custom.sort_by(|a, b| a.0.cmp(b.0));
    for (key, value) in custom {
        let text = if value.is_protected() && !app.reveal {
            HIDDEN
        } else {
            value.get().as_str()
        };
        let label = truncate(key, LABEL_W as usize - 1);
        let copy = Hit::CopyField(key.clone());
        field(
            Row::new(&label, text, Style::new())
                .hit(copy.clone())
                .buttons(vec![btn("", "copy", copy)]),
        );
    }

    if !e.tags.is_empty() {
        field(Row::new("Tags", e.tags.join(", "), Style::new().fg(ACCENT)));
    }
    let attachments: Vec<&str> = e.attachments_named().map(|(name, _)| name).collect();
    if !attachments.is_empty() {
        field(Row::new("Files", attachments.join(", "), Style::new()));
    }

    // Footer: timestamps and expiry, pinned to the bottom.
    let footer_y = bottom.saturating_sub(1);
    let footer = footer_line(&e.times);
    if footer_y > y {
        frame.render_widget(Paragraph::new(footer), row(footer_y));
    }

    let notes = e.get(fields::NOTES).unwrap_or("");
    if !notes.is_empty() && y + 2 < footer_y {
        y += 1;
        frame.render_widget(Paragraph::new(Span::raw("Notes").fg(DIM)), row(y));
        y += 1;
        let notes_area = Rect::new(inner.x, y, inner.width, footer_y.saturating_sub(y + 1));
        frame.render_widget(
            Paragraph::new(notes.to_string()).wrap(Wrap { trim: false }),
            notes_area,
        );
    }
}

/// One row of the detail pane: a label, a value, and what the value offers.
struct Row<'a> {
    label: &'a str,
    text: String,
    style: Style,
    /// Shown after the value when there's room, e.g. the TOTP countdown.
    suffix: Vec<Span<'static>>,
    /// What clicking the value does.
    hit: Option<Hit>,
    buttons: Vec<Btn<'a>>,
}

impl<'a> Row<'a> {
    fn new(label: &'a str, text: impl Into<String>, style: Style) -> Self {
        Row {
            label,
            text: text.into(),
            style,
            suffix: Vec::new(),
            hit: None,
            buttons: Vec::new(),
        }
    }

    fn hit(mut self, hit: Hit) -> Self {
        self.hit = Some(hit);
        self
    }

    fn buttons(mut self, buttons: Vec<Btn<'a>>) -> Self {
        self.buttons = buttons;
        self
    }

    fn suffix(mut self, suffix: Vec<Span<'static>>) -> Self {
        self.suffix = suffix;
        self
    }
}

fn field_row(frame: &mut Frame, app: &App, area: Rect, row: Row<'_>) {
    let Row {
        label,
        text,
        style,
        suffix,
        hit: value_hit,
        buttons: btns,
    } = row;
    frame.render_widget(
        Paragraph::new(Span::raw(label.to_string()).fg(DIM)),
        Rect {
            width: LABEL_W.min(area.width),
            ..area
        },
    );
    let value_x = area.x + LABEL_W;
    if value_x >= area.right() {
        return;
    }
    let after_label = Rect::new(value_x, area.y, area.right() - value_x, 1);
    // When space is tight, drop the key hints from the buttons so the
    // value stays readable.
    let mut btns = btns;
    if buttons_width(&btns) + 12 > after_label.width {
        btns.iter_mut().for_each(|b| b.key = "");
    }
    let bx = buttons_right(frame, app, after_label, btns);
    let value_w = bx.saturating_sub(value_x + 1);
    let suffix_w: u16 = suffix.iter().map(|s| width(&s.content)).sum();
    let text_w = if suffix_w + 4 <= value_w {
        value_w - suffix_w
    } else {
        value_w
    };
    let text = truncate(&text, text_w as usize);
    let value_rect = Rect::new(value_x, area.y, value_w, 1);
    let mut style = style;
    if value_hit.is_some()
        && hovered(
            app,
            Rect {
                width: width(&text),
                ..value_rect
            },
        )
    {
        style = style.underlined();
    }
    let mut spans = vec![Span::styled(text.clone(), style)];
    if text_w < value_w {
        spans.extend(suffix);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), value_rect);
    if let Some(h) = value_hit {
        hit(
            app,
            Rect {
                width: width(&text).min(value_w),
                ..value_rect
            },
            h,
        );
    }
}

fn footer_line(times: &Times) -> Line<'static> {
    let fmt = |t: NaiveDateTime| super::local_time(t, "%Y-%m-%d %H:%M");
    let mut spans = Vec::new();
    if times.expires == Some(true)
        && let Some(exp) = times.expiry
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if exp.and_utc().timestamp() <= now {
            spans.push(Span::raw(format!("expired {}", fmt(exp))).fg(ERR).bold());
        } else {
            spans.push(Span::raw(format!("expires {}", fmt(exp))).fg(WARN));
        }
        spans.push(Span::raw(" · ").fg(DIM));
    }
    if let Some(t) = times.last_modification {
        spans.push(Span::raw(format!("modified {}", fmt(t))).fg(DIM));
    }
    if let Some(t) = times.creation {
        spans.push(Span::raw(format!(" · created {}", fmt(t))).fg(DIM));
    }
    Line::from(spans)
}

fn group_code(code: &str) -> String {
    if code.len() == 6 || code.len() == 8 {
        let (a, b) = code.split_at(code.len() / 2);
        format!("{a} {b}")
    } else {
        code.to_string()
    }
}

// -- status line and key bar -----------------------------------------------------

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let Some(v) = &app.vault else { return };

    let right = app
        .clipboard
        .countdown()
        .map(|(label, secs)| format!("⧉ {label} in clipboard · clears in {secs}s "))
        .unwrap_or_default();
    let [l, r] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(width(&right))]).areas(area);
    frame.render_widget(Paragraph::new(Span::raw(right).fg(WARN)), r);

    let name = format!(" {}", v.file_name());
    let x = l.x + width(&name);
    let mut spans = vec![Span::raw(name).bold()];
    if app.dirty {
        let label = "● unsaved (^s)";
        let rect = Rect::new(x + 2, l.y, width(label), 1).intersection(l);
        hit(app, rect, Hit::Act(Action::Save));
        let style = if hovered(app, rect) {
            Style::new().fg(WARN).bold().underlined()
        } else {
            Style::new().fg(WARN).bold()
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(label, style));
    }
    if let Some((msg, kind, _)) = &app.status {
        spans.push(Span::raw("   "));
        spans.push(match kind {
            StatusKind::Info => Span::raw(msg.clone()).fg(OK),
            StatusKind::Error => Span::raw(format!("✗ {msg}")).fg(ERR).bold(),
        });
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), l);
}

fn draw_keys(frame: &mut Frame, app: &App, area: Rect) {
    if app.overlay.is_some() || !matches!(app.screen, Screen::Browser) {
        return;
    }
    let act = Hit::Act;
    let mut items = Vec::new();
    if app.search_input {
        use ratatui::crossterm::event::KeyCode;
        items.push(btn("↑↓", "select", key(KeyCode::Down)));
        items.push(btn("⏎", "done", key(KeyCode::Enter)));
        items.push(btn("esc", "cancel", key(KeyCode::Esc)));
    } else if app.pane == Pane::Entries {
        let e = app
            .sel_entry
            .and_then(|id| app.vault.as_ref()?.db.entry(id));
        if e.is_some() {
            items.push(btn("⏎", "copy password", act(Action::CopyPass)));
            items.push(btn("y", "copy user", act(Action::CopyUser)));
        }
        if e.as_ref().is_some_and(|e| e.get_raw_otp_value().is_some()) {
            items.push(btn("t", "copy TOTP", act(Action::CopyOtp)));
        }
        if e.as_ref()
            .is_some_and(|e| e.get_url().is_some_and(|u| !u.is_empty()))
        {
            items.push(btn("o", "open URL", act(Action::OpenUrl)));
        }
        if e.is_some() {
            items.push(btn("e", "edit", act(Action::Edit)));
        }
        items.push(btn("a", "new", act(Action::NewEntry)));
        if app.search.is_some() {
            items.push(btn("esc", "end search", act(Action::Escape)));
        } else {
            items.push(btn("/", "search", act(Action::Search)));
        }
        items.push(btn("←", "groups", act(Action::Left)));
    } else {
        items.push(btn("⏎", "open", act(Action::Activate)));
        items.push(btn("␣", "expand", act(Action::ToggleExpand)));
        items.push(btn("a", "new entry", act(Action::NewEntry)));
        items.push(btn("A", "new group", act(Action::NewGroup)));
        items.push(btn("e", "rename", act(Action::Edit)));
        items.push(btn("d", "delete", act(Action::Delete)));
        items.push(btn("/", "search", act(Action::Search)));
    }
    if !app.search_input {
        items.push(btn("^g", "generate", act(Action::Generator)));
        items.push(btn("?", "help", act(Action::Help)));
        items.push(btn("q", "quit", act(Action::Quit)));
    }
    key_bar(frame, app, area, items);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_labels_each_time() {
        let at = |day| {
            chrono::NaiveDate::from_ymd_opt(2026, 10, day)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
        };
        // Times is non_exhaustive: no struct literal.
        let mut times = Times::default();
        times.creation = Some(at(1));
        times.last_modification = Some(at(2));
        times.expires = Some(true);
        times.expiry = Some(at(3));
        let line = footer_line(&times);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let shown = |t| super::super::local_time(t, "%Y-%m-%d %H:%M");
        assert_eq!(
            text,
            format!(
                "expired {} · modified {} · created {}",
                shown(at(3)),
                shown(at(2)),
                shown(at(1))
            )
        );
    }
}
