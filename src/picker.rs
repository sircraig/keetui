//! A small file browser for choosing a database: folders and `.kdbx` files,
//! filtered by typing.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Parent,
    Dir,
    File,
}

pub struct Item {
    pub name: String,
    pub path: PathBuf,
    pub kind: ItemKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// What the app should do after a key press in the picker.
pub enum Outcome {
    Stay,
    Open(PathBuf),
    /// Create a new database in this folder.
    CreateIn(PathBuf),
    /// Esc with nothing left to clear.
    Back,
    Quit,
}

pub struct PickerState {
    pub dir: PathBuf,
    pub items: Vec<Item>,
    pub filter: String,
    /// Index into `visible()`.
    pub selected: usize,
    /// Show dotfiles and files of any type, not only `.kdbx`.
    pub show_all: bool,
    pub error: Option<String>,
    /// Scroll offset, kept by the UI between frames.
    pub offset: Cell<usize>,
}

impl PickerState {
    /// Open the browser in `dir`, preselecting `select` if it is listed.
    pub fn new(dir: &Path, select: Option<&Path>) -> Self {
        let mut st = PickerState {
            dir: PathBuf::new(),
            items: Vec::new(),
            filter: String::new(),
            selected: 0,
            show_all: false,
            error: None,
            offset: Cell::new(0),
        };
        if let Err(e) = st.load(dir) {
            st.error = Some(e);
            let _ = st.load(&home_dir());
        }
        st.select_path(select);
        st
    }

    /// Read `dir` and make it the current folder. On failure the current
    /// listing is left alone.
    fn load(&mut self, dir: &Path) -> Result<(), String> {
        let dir = fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let read = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut items: Vec<Item> = read
            .filter_map(Result::ok)
            .filter_map(|de| {
                let name = de.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') && !self.show_all {
                    return None;
                }
                let path = de.path();
                // Follow symlinks so a linked folder or vault behaves normally.
                let meta = fs::metadata(&path).ok()?;
                let kind = if meta.is_dir() {
                    ItemKind::Dir
                // Regular files only: reading a FIFO or device named *.kdbx
                // would block (or never end).
                } else if meta.is_file() && (self.show_all || is_kdbx(&path)) {
                    ItemKind::File
                } else {
                    return None;
                };
                Some(Item {
                    name,
                    path,
                    kind,
                    size: meta.len(),
                    modified: meta.modified().ok(),
                })
            })
            .collect();
        items.sort_by(|a, b| {
            (a.kind != ItemKind::Dir)
                .cmp(&(b.kind != ItemKind::Dir))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        if let Some(parent) = dir.parent() {
            items.insert(
                0,
                Item {
                    name: "..".into(),
                    path: parent.to_path_buf(),
                    kind: ItemKind::Parent,
                    size: 0,
                    modified: None,
                },
            );
        }
        // Start on the first database, else the first folder.
        self.selected = [ItemKind::File, ItemKind::Dir]
            .iter()
            .find_map(|k| items.iter().position(|it| it.kind == *k))
            .unwrap_or(0);
        self.dir = dir;
        self.items = items;
        self.filter.clear();
        self.offset.set(0);
        Ok(())
    }

    fn reload(&mut self) {
        let dir = self.dir.clone();
        let current = self.current().map(|i| i.path.clone());
        if let Err(e) = self.load(&dir) {
            self.error = Some(e);
        }
        self.select_path(current.as_deref());
    }

    fn select_path(&mut self, path: Option<&Path>) {
        let Some(path) = path else { return };
        let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(i) = self
            .visible()
            .iter()
            .position(|&i| self.items[i].kind != ItemKind::Parent && self.items[i].path == target)
        {
            self.selected = i;
        }
    }

    /// Indices into `items` that match the filter.
    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                if f.is_empty() {
                    true
                } else {
                    it.kind != ItemKind::Parent && it.name.to_lowercase().contains(&f)
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn current(&self) -> Option<&Item> {
        self.visible().get(self.selected).map(|&i| &self.items[i])
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.visible().len();
        if len > 0 {
            self.selected = (self.selected as isize)
                .saturating_add(delta)
                .clamp(0, len as isize - 1) as usize;
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.visible().len() {
            self.selected = index;
        }
    }

    fn enter(&mut self, dir: &Path) {
        match self.load(dir) {
            Ok(()) => self.error = None,
            Err(e) => self.error = Some(e),
        }
    }

    fn go_up(&mut self) {
        let from = self.dir.clone();
        if let Some(parent) = from.parent().map(Path::to_path_buf) {
            self.enter(&parent);
            // Land on the folder we just left.
            self.select_path(Some(&from));
        }
    }

    /// Enter the selected folder or open the selected file.
    pub fn activate(&mut self) -> Outcome {
        let Some(item) = self.current() else {
            return Outcome::Stay;
        };
        match item.kind {
            ItemKind::Parent => self.go_up(),
            ItemKind::Dir => {
                let path = item.path.clone();
                self.enter(&path);
            }
            ItemKind::File => return Outcome::Open(item.path.clone()),
        }
        Outcome::Stay
    }

    pub fn on_key(&mut self, key: KeyEvent, page: usize) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => return Outcome::Quit,
            KeyCode::Char('n') if ctrl => return Outcome::CreateIn(self.dir.clone()),
            KeyCode::Char('a') if ctrl => {
                self.show_all = !self.show_all;
                self.reload();
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.selected = 0;
            }
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    return Outcome::Back;
                }
                self.filter.clear();
                self.selected = 0;
            }
            KeyCode::Enter => return self.activate(),
            KeyCode::Right => {
                if self.current().is_some_and(|i| i.kind != ItemKind::File) {
                    return self.activate();
                }
            }
            KeyCode::Left => self.go_up(),
            KeyCode::Backspace => {
                if self.filter.pop().is_none() {
                    self.go_up();
                } else {
                    self.selected = 0;
                }
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(page as isize)),
            KeyCode::PageDown => self.move_by(page as isize),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.move_by(isize::MAX / 2),
            KeyCode::Char('~') if self.filter.is_empty() => self.enter(&home_dir()),
            KeyCode::Char('/') if self.filter.is_empty() => self.enter(Path::new("/")),
            KeyCode::Char(c) if !ctrl => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        Outcome::Stay
    }
}

pub fn is_kdbx(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("kdbx"))
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `~/x` style display of a path under the home directory.
pub fn tilde(path: &Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Human-readable file size.
pub fn size_label(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("vaults")).unwrap();
        fs::create_dir(dir.path().join(".hidden")).unwrap();
        fs::write(dir.path().join("Work.kdbx"), b"x").unwrap();
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        fs::write(dir.path().join("vaults/Home.KDBX"), b"x").unwrap();
        dir
    }

    fn names(st: &PickerState) -> Vec<String> {
        st.visible()
            .iter()
            .map(|&i| st.items[i].name.clone())
            .collect()
    }

    #[test]
    fn lists_folders_then_databases() {
        let dir = setup();
        let st = PickerState::new(dir.path(), None);
        assert_eq!(names(&st), ["..", "vaults", "Work.kdbx"]);
        assert_eq!(st.current().unwrap().name, "Work.kdbx");
    }

    #[test]
    fn show_all_includes_hidden_and_other_files() {
        let dir = setup();
        let mut st = PickerState::new(dir.path(), None);
        st.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 10);
        assert_eq!(
            names(&st),
            ["..", ".hidden", "vaults", "notes.txt", "Work.kdbx"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn never_lists_fifos_or_devices() {
        let dir = setup();
        let fifo = dir.path().join("pipe.kdbx");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            return; // no mkfifo here
        }
        let mut st = PickerState::new(dir.path(), None);
        assert!(!names(&st).contains(&"pipe.kdbx".to_string()));
        st.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 10);
        assert!(!names(&st).contains(&"pipe.kdbx".to_string()));

        let mut st = PickerState::new(Path::new("/dev"), None);
        st.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 10);
        assert!(!names(&st).contains(&"zero".to_string()));
    }

    #[test]
    fn filter_navigate_and_open() {
        let dir = setup();
        let mut st = PickerState::new(dir.path(), None);
        for c in "vau".chars() {
            st.on_key(key(KeyCode::Char(c)), 10);
        }
        assert_eq!(names(&st), ["vaults"]);
        assert!(matches!(st.on_key(key(KeyCode::Enter), 10), Outcome::Stay));
        assert!(st.dir.ends_with("vaults"));
        assert!(st.filter.is_empty());
        match st.on_key(key(KeyCode::Enter), 10) {
            Outcome::Open(p) => assert!(p.ends_with("vaults/Home.KDBX")),
            _ => panic!("expected Open"),
        }
        // Backspace with an empty filter goes up and lands on the folder.
        st.on_key(key(KeyCode::Backspace), 10);
        assert_eq!(st.current().unwrap().name, "vaults");
    }

    #[test]
    fn preselects_given_file_and_esc_backs_out() {
        let dir = setup();
        let mut st = PickerState::new(dir.path(), Some(&dir.path().join("Work.kdbx")));
        assert_eq!(st.current().unwrap().name, "Work.kdbx");
        assert!(matches!(st.on_key(key(KeyCode::Esc), 10), Outcome::Back));
    }

    #[test]
    fn sizes() {
        assert_eq!(size_label(512), "512 B");
        assert_eq!(size_label(12897), "12.6 KB");
    }
}
