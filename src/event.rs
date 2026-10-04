//! Keymap for the browser screen: crossterm key events -> Actions.
//!
//! Actions are also what the clickable key bar and detail-pane buttons
//! dispatch, so mouse and keyboard share one code path.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    /// Groups: collapse / go to parent. Entries: back to groups.
    Left,
    /// Groups: expand / go to entries.
    Right,
    NextPane,
    /// Groups: show entries. Entries: copy password.
    Activate,
    ToggleExpand,
    Search,
    Escape,
    CopyUser,
    CopyPass,
    CopyOtp,
    CopyUrl,
    OpenUrl,
    ToggleReveal,
    NewEntry,
    NewGroup,
    /// Entries: edit entry. Groups: rename group.
    Edit,
    RenameGroup,
    Delete,
    Generator,
    Save,
    Quit,
    Help,
}

pub fn browser_action(key: KeyEvent) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Char('s') => Some(Action::Save),
            KeyCode::Char('g') => Some(Action::Generator),
            KeyCode::Char('f') => Some(Action::Search),
            KeyCode::Char('n') => Some(Action::Down),
            KeyCode::Char('p') => Some(Action::Up),
            KeyCode::Char('d') => Some(Action::PageDown),
            KeyCode::Char('u') => Some(Action::PageUp),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => Some(Action::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::Up),
        KeyCode::PageDown => Some(Action::PageDown),
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::Char('g') | KeyCode::Home => Some(Action::Top),
        KeyCode::Char('G') | KeyCode::End => Some(Action::Bottom),
        KeyCode::Char('h') | KeyCode::Left => Some(Action::Left),
        KeyCode::Char('l') | KeyCode::Right => Some(Action::Right),
        KeyCode::Tab | KeyCode::BackTab => Some(Action::NextPane),
        KeyCode::Enter => Some(Action::Activate),
        KeyCode::Char(' ') => Some(Action::ToggleExpand),
        KeyCode::Char('/') => Some(Action::Search),
        KeyCode::Esc => Some(Action::Escape),
        KeyCode::Char('c') | KeyCode::Char('p') | KeyCode::Char('Y') => Some(Action::CopyPass),
        KeyCode::Char('y') => Some(Action::CopyUser),
        KeyCode::Char('t') => Some(Action::CopyOtp),
        KeyCode::Char('u') => Some(Action::CopyUrl),
        KeyCode::Char('o') | KeyCode::Char('U') => Some(Action::OpenUrl),
        KeyCode::Char('r') | KeyCode::Char('v') => Some(Action::ToggleReveal),
        KeyCode::Char('a') => Some(Action::NewEntry),
        KeyCode::Char('A') => Some(Action::NewGroup),
        KeyCode::Char('e') => Some(Action::Edit),
        KeyCode::Char('R') => Some(Action::RenameGroup),
        KeyCode::Char('d') | KeyCode::Delete => Some(Action::Delete),
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') | KeyCode::F(1) => Some(Action::Help),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn maps_basic_navigation() {
        assert_eq!(browser_action(key(KeyCode::Char('j'))), Some(Action::Down));
        assert_eq!(browser_action(key(KeyCode::Char('k'))), Some(Action::Up));
        assert_eq!(
            browser_action(key(KeyCode::Char('/'))),
            Some(Action::Search)
        );
        assert_eq!(browser_action(key(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn maps_copy_and_open() {
        assert_eq!(
            browser_action(key(KeyCode::Char('c'))),
            Some(Action::CopyPass)
        );
        assert_eq!(
            browser_action(key(KeyCode::Char('p'))),
            Some(Action::CopyPass)
        );
        assert_eq!(browser_action(key(KeyCode::Enter)), Some(Action::Activate));
        assert_eq!(
            browser_action(key(KeyCode::Char('o'))),
            Some(Action::OpenUrl)
        );
    }

    #[test]
    fn ctrl_combinations() {
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(browser_action(save), Some(Action::Save));
        let generator = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(browser_action(generator), Some(Action::Generator));
        // Ctrl must not fall through to the plain-letter bindings.
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(browser_action(ctrl_e), None);
    }
}
