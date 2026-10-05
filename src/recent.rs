//! The databases opened recently, remembered between sessions, and the
//! screen keetui starts on without a database path: open one of them,
//! browse for another, or create a new one.
//!
//! The list lives in `$XDG_STATE_HOME/keetui/recent` (by default
//! `~/.local/state/keetui/recent`): one absolute path per line, newest
//! first. It says where the vaults are, so only the user can read it.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::StatusKind;
use crate::picker::tilde;

/// How many databases are remembered.
pub const MAX_RECENT: usize = 10;

/// Where the list is kept; None when neither `XDG_STATE_HOME` nor `HOME`
/// says where that is.
pub fn default_file() -> Option<PathBuf> {
    list_file(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn list_file(state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |dir: OsString| Some(PathBuf::from(dir)).filter(|p| p.is_absolute());
    // A relative XDG_STATE_HOME is invalid and ignored, as the spec says.
    let state = match state_home.and_then(absolute) {
        Some(dir) => dir,
        None => absolute(home?)?.join(".local/state"),
    };
    Some(state.join("keetui/recent"))
}

/// The remembered databases, newest first. A list that is missing or
/// can't be read is empty.
pub fn load(file: &Path) -> Vec<PathBuf> {
    // A regular file only: reading a FIFO would block.
    if !fs::metadata(file).is_ok_and(|m| m.is_file()) {
        return Vec::new();
    }
    let Ok(data) = fs::read(file) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for line in String::from_utf8_lossy(&data).lines() {
        let path = PathBuf::from(line);
        // Anything but an absolute path (a blank line, say) is skipped.
        if path.is_absolute() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths.truncate(MAX_RECENT);
    paths
}

/// Put the database at `db` first in the list kept in `file`.
pub fn remember(file: &Path, db: &Path) -> Result<()> {
    // The real file: a database opened through a relative path or a
    // symlink is listed once, and opens from any folder.
    let db = fs::canonicalize(db)?;
    let mut paths = load(file);
    if paths.first() == Some(&db) || as_line(&db).is_none() {
        return Ok(());
    }
    paths.retain(|p| *p != db);
    paths.insert(0, db);
    paths.truncate(MAX_RECENT);
    store(file, &paths)
}

/// Take `db` off the list kept in `file`. The database itself is left
/// alone.
pub fn forget(file: &Path, db: &Path) -> Result<()> {
    let mut paths = load(file);
    let count = paths.len();
    paths.retain(|p| p != db);
    if paths.len() == count {
        return Ok(());
    }
    store(file, &paths)
}

/// `path` as a line of the list, if it can be one: a path that isn't
/// UTF-8 or holds a line break isn't remembered.
fn as_line(path: &Path) -> Option<&str> {
    path.to_str().filter(|s| !s.contains(['\n', '\r']))
}

fn store(file: &Path, paths: &[PathBuf]) -> Result<()> {
    let mut text = String::new();
    for line in paths.iter().filter_map(|p| as_line(p)) {
        text.push_str(line);
        text.push('\n');
    }
    if let Some(dir) = file.parent() {
        create_private_dir(dir)?;
    }
    // Replaced whole, never left half written; the temp file it is
    // renamed from is created readable by the user only.
    crate::db::write_atomic(file, text.as_bytes())
}

/// Create `dir` and any missing parents, private to the user, as the XDG
/// spec asks.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// The name of a database file, for display.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A database on the recent list.
pub struct RecentItem {
    pub path: PathBuf,
    /// The file is there. One that was deleted, or is on a drive that
    /// isn't mounted, stays listed until it is forgotten.
    pub found: bool,
    pub modified: Option<SystemTime>,
}

/// What the app should do after a key press on the recent databases.
pub enum Outcome {
    Stay,
    Open(PathBuf),
    /// Browse the filesystem for a database.
    Browse,
    /// Create a new database.
    Create,
    Quit,
}

pub struct RecentState {
    /// Where the list is kept; None remembers nothing.
    pub file: Option<PathBuf>,
    pub items: Vec<RecentItem>,
    pub selected: usize,
    /// What the last key did, when that needs saying.
    pub message: Option<(String, StatusKind)>,
    /// Scroll offset, kept by the UI between frames.
    pub offset: Cell<usize>,
}

impl RecentState {
    /// The list kept in `file`, selecting `select` if it is on it, else the
    /// newest database that is there.
    pub fn new(file: Option<PathBuf>, select: Option<&Path>) -> Self {
        let items: Vec<RecentItem> = file
            .as_deref()
            .map(load)
            .unwrap_or_default()
            .into_iter()
            .map(|path| {
                // Regular files only, as in the file picker.
                let meta = fs::metadata(&path).ok().filter(|m| m.is_file());
                RecentItem {
                    found: meta.is_some(),
                    modified: meta.and_then(|m| m.modified().ok()),
                    path,
                }
            })
            .collect();
        let select = select.and_then(|p| fs::canonicalize(p).ok());
        let selected = items
            .iter()
            .position(|it| Some(&it.path) == select.as_ref())
            .or_else(|| items.iter().position(|it| it.found))
            .unwrap_or(0);
        RecentState {
            file,
            items,
            selected,
            message: None,
            offset: Cell::new(0),
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        self.selected = crate::app::clamp_move(self.selected, delta, self.items.len());
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
            self.message = None;
        }
    }

    /// Open the selected database; with none listed, browse for one.
    pub fn activate(&mut self) -> Outcome {
        let Some(item) = self.items.get_mut(self.selected) else {
            return Outcome::Browse;
        };
        // Looked at again: the drive it is on may have been mounted since.
        item.found = item.path.is_file();
        if item.found {
            return Outcome::Open(item.path.clone());
        }
        let msg = format!(
            "{} not found — d removes it from the list",
            tilde(&item.path)
        );
        self.message = Some((msg, StatusKind::Error));
        Outcome::Stay
    }

    /// Take the selected database off the list; the file stays as it is.
    fn forget_selected(&mut self) {
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        if let Some(file) = &self.file
            && let Err(e) = forget(file, &item.path)
        {
            let msg = format!("couldn't update {}: {e:#}", tilde(file));
            self.message = Some((msg, StatusKind::Error));
            return;
        }
        let item = self.items.remove(self.selected);
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        let msg = format!(
            "✓ removed {} from the list (the file is kept)",
            file_name(&item.path)
        );
        self.message = Some((msg, StatusKind::Info));
    }

    pub fn on_key(&mut self, key: KeyEvent, page: usize) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.message = None;
        match key.code {
            KeyCode::Char('c') if ctrl => return Outcome::Quit,
            // With or without Ctrl: ^o and ^n are what the unlock screen and
            // the picker use, and there's nothing to type here.
            KeyCode::Char('o') => return Outcome::Browse,
            KeyCode::Char('n') => return Outcome::Create,
            _ if ctrl => {}
            KeyCode::Enter => return self.activate(),
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Char('d') | KeyCode::Delete => self.forget_selected(),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(page as isize)),
            KeyCode::PageDown => self.move_by(page as isize),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            _ => {}
        }
        Outcome::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp folder holding databases with these names, and a list file
    /// (not created yet) in a state folder that doesn't exist yet either.
    fn setup(names: &[&str]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let dbs = names
            .iter()
            .map(|name| {
                let path = dir.path().join(name);
                fs::write(&path, b"x").unwrap();
                fs::canonicalize(path).unwrap()
            })
            .collect();
        let file = dir.path().join("state/keetui/recent");
        (dir, file, dbs)
    }

    #[test]
    fn remembers_each_database_once_newest_first() {
        let (dir, file, dbs) = setup(&["a.kdbx", "b.kdbx"]);
        assert!(load(&file).is_empty(), "nothing remembered yet");
        remember(&file, &dbs[0]).unwrap();
        remember(&file, &dbs[1]).unwrap();
        assert_eq!(load(&file), [dbs[1].clone(), dbs[0].clone()]);

        // The same file reached another way is the same database.
        fs::create_dir(dir.path().join("sub")).unwrap();
        let roundabout = dir.path().join("sub/../a.kdbx");
        remember(&file, &roundabout).unwrap();
        assert_eq!(load(&file), [dbs[0].clone(), dbs[1].clone()]);
        #[cfg(unix)]
        {
            let link = dir.path().join("link.kdbx");
            std::os::unix::fs::symlink(&dbs[1], &link).unwrap();
            remember(&file, &link).unwrap();
            assert_eq!(load(&file), [dbs[1].clone(), dbs[0].clone()]);
        }
    }

    #[test]
    fn keeps_only_the_most_recent() {
        let names: Vec<String> = (0..MAX_RECENT + 2).map(|i| format!("{i}.kdbx")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (_dir, file, dbs) = setup(&names);
        for db in &dbs {
            remember(&file, db).unwrap();
        }
        let newest_first: Vec<PathBuf> = dbs.iter().rev().take(MAX_RECENT).cloned().collect();
        assert_eq!(load(&file), newest_first);
    }

    #[test]
    fn forgetting_keeps_the_database() {
        let (_dir, file, dbs) = setup(&["a.kdbx", "b.kdbx"]);
        remember(&file, &dbs[0]).unwrap();
        remember(&file, &dbs[1]).unwrap();
        forget(&file, &dbs[1]).unwrap();
        assert_eq!(load(&file), [dbs[0].clone()]);
        assert!(dbs[1].is_file());
        // Forgetting what isn't listed changes nothing.
        forget(&file, &dbs[1]).unwrap();
        assert_eq!(load(&file), [dbs[0].clone()]);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// The screen for a list of `names`, newest first, and their paths; the
    /// databases named `gone` are listed but not there.
    fn screen(names: &[&str], gone: &[&str]) -> (tempfile::TempDir, RecentState, Vec<PathBuf>) {
        let (dir, file, dbs) = setup(names);
        for db in dbs.iter().rev() {
            remember(&file, db).unwrap();
        }
        for name in gone {
            fs::remove_file(dir.path().join(name)).unwrap();
        }
        (dir, RecentState::new(Some(file), None), dbs)
    }

    fn names(st: &RecentState) -> Vec<String> {
        st.items.iter().map(|it| file_name(&it.path)).collect()
    }

    #[test]
    fn starts_on_the_newest_database_that_is_there() {
        let (_dir, st, dbs) = screen(&["gone.kdbx", "a.kdbx", "b.kdbx"], &["gone.kdbx"]);
        assert_eq!(names(&st), ["gone.kdbx", "a.kdbx", "b.kdbx"]);
        assert_eq!(
            st.items.iter().map(|it| it.found).collect::<Vec<_>>(),
            [false, true, true]
        );
        assert_eq!(st.selected, 1);

        // Coming back to the list keeps the database last chosen selected.
        let st = RecentState::new(st.file.clone(), Some(&dbs[2]));
        assert_eq!(st.selected, 2);
    }

    #[test]
    fn enter_opens_a_database_that_is_there() {
        let (_dir, mut st, dbs) = screen(&["gone.kdbx", "a.kdbx"], &["gone.kdbx"]);
        match st.on_key(key(KeyCode::Enter), 10) {
            Outcome::Open(path) => assert_eq!(path, dbs[1]),
            _ => panic!("expected Open"),
        }

        // One that isn't there says so and stays listed...
        st.on_key(key(KeyCode::Up), 10);
        assert!(matches!(st.on_key(key(KeyCode::Enter), 10), Outcome::Stay));
        assert!(matches!(&st.message, Some((msg, StatusKind::Error)) if msg.contains("not found")));
        assert_eq!(st.items.len(), 2);
        // ...and opens once it is back.
        fs::write(&dbs[0], b"x").unwrap();
        assert!(matches!(
            st.on_key(key(KeyCode::Enter), 10),
            Outcome::Open(_)
        ));
    }

    #[test]
    fn forgetting_takes_a_database_off_the_list_only() {
        let (_dir, mut st, dbs) = screen(&["a.kdbx", "b.kdbx"], &[]);
        st.on_key(key(KeyCode::Char('d')), 10);
        assert_eq!(names(&st), ["b.kdbx"]);
        assert!(matches!(&st.message, Some((msg, StatusKind::Info)) if msg.contains("a.kdbx")));
        assert!(dbs[0].is_file(), "the file must stay");
        let file = st.file.clone().unwrap();
        assert_eq!(load(&file), [dbs[1].clone()]);

        // Ctrl-d isn't d.
        st.on_key(ctrl('d'), 10);
        assert_eq!(names(&st), ["b.kdbx"]);
        st.on_key(key(KeyCode::Delete), 10);
        assert!(st.items.is_empty() && load(&file).is_empty());
        // Nothing left to forget.
        st.on_key(key(KeyCode::Delete), 10);
        assert!(st.items.is_empty());
    }

    #[test]
    fn keys_browse_create_and_quit() {
        let (_dir, mut st, _dbs) = screen(&["a.kdbx"], &[]);
        for k in [key(KeyCode::Char('o')), ctrl('o')] {
            assert!(matches!(st.on_key(k, 10), Outcome::Browse));
        }
        for k in [key(KeyCode::Char('n')), ctrl('n')] {
            assert!(matches!(st.on_key(k, 10), Outcome::Create));
        }
        for k in [key(KeyCode::Esc), key(KeyCode::Char('q')), ctrl('c')] {
            assert!(matches!(st.on_key(k, 10), Outcome::Quit));
        }

        // With nothing listed (or nowhere to keep a list), Enter browses.
        let mut st = RecentState::new(None, None);
        assert!(st.items.is_empty());
        assert!(matches!(
            st.on_key(key(KeyCode::Enter), 10),
            Outcome::Browse
        ));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_the_list() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, file, dbs) = setup(&["a.kdbx"]);
        remember(&file, &dbs[0]).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);
        assert_eq!(mode(file.parent().unwrap().parent().unwrap()), 0o700);
    }

    #[test]
    fn loading_skips_what_it_cannot_use() {
        let (dir, file, dbs) = setup(&["a.kdbx"]);
        let a = dbs[0].display().to_string();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, format!("\nrelative.kdbx\n{a}\r\n{a}\n")).unwrap();
        assert_eq!(load(&file), [dbs[0].clone()]);
        assert!(load(&dir.path().join("missing")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn never_blocks_on_a_fifo() {
        let (dir, _file, _dbs) = setup(&[]);
        let fifo = dir.path().join("recent");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            return; // no mkfifo here
        }
        // Would wait forever for a writer in fs::read without the check.
        assert!(load(&fifo).is_empty());
    }

    #[test]
    fn the_list_goes_where_xdg_says() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            list_file(os("/x/state"), os("/home/u")),
            Some(PathBuf::from("/x/state/keetui/recent"))
        );
        let fallback = Some(PathBuf::from("/home/u/.local/state/keetui/recent"));
        assert_eq!(list_file(None, os("/home/u")), fallback);
        // Empty and relative values don't count.
        assert_eq!(list_file(os(""), os("/home/u")), fallback);
        assert_eq!(list_file(os("state"), os("/home/u")), fallback);
        assert_eq!(list_file(None, os("home")), None);
        assert_eq!(list_file(None, None), None);
    }
}
