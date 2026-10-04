//! Application state machine: screens, selection, editing, dirty tracking.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use keepass::db::{EntryId, GroupId, fields};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use zeroize::Zeroizing;

use crate::clipboard::{Clipboard, DEFAULT_TTL};
use crate::db::Vault;
use crate::event::{Action, browser_action};
use crate::generator::{self, GenOpts, MAX_LENGTH, MIN_LENGTH};
use crate::picker::{self, PickerState};
use crate::{open, totp};

const STATUS_TTL: Duration = Duration::from_secs(5);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const SCROLL_STEP: isize = 3;

// ---------------------------------------------------------------------------
// Text editing primitive shared by all forms. The buffer is Zeroizing so
// secrets typed into any field are wiped when the form is dropped.

#[derive(Default)]
pub struct TextField {
    pub text: Zeroizing<String>,
    pub cursor: usize, // char index
}

impl TextField {
    pub fn with_text(s: &str) -> Self {
        TextField {
            cursor: s.chars().count(),
            text: Zeroizing::new(s.to_string()),
        }
    }

    fn byte_idx(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    pub fn insert(&mut self, c: char) {
        let i = self.byte_idx();
        self.text.insert(i, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        let i = self.byte_idx();
        self.text.remove(i);
        true
    }

    pub fn delete(&mut self) -> bool {
        let i = self.byte_idx();
        if i < self.text.len() {
            self.text.remove(i);
            true
        } else {
            false
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    pub fn set_text(&mut self, s: &str) {
        self.text = Zeroizing::new(s.to_string());
        self.cursor = s.chars().count();
    }

    pub fn clear(&mut self) -> bool {
        let changed = !self.text.is_empty();
        self.set_text("");
        changed
    }
}

// ---------------------------------------------------------------------------
// Screens and overlays

pub struct UnlockState {
    pub password: TextField,
    pub keyfile: TextField,
    pub focus_keyfile: bool,
    pub error: Option<String>,
    /// Set when Enter was pressed; the (slow) unlock runs on the next tick so
    /// the "unlocking…" frame gets drawn first.
    pub working: bool,
}

impl UnlockState {
    fn new(keyfile: &str) -> Self {
        UnlockState {
            password: TextField::default(),
            keyfile: TextField::with_text(keyfile),
            focus_keyfile: false,
            error: None,
            working: false,
        }
    }
}

pub const C_PATH: usize = 0;
pub const C_PASS: usize = 1;
pub const C_CONFIRM: usize = 2;
pub const C_KEYFILE: usize = 3;
pub const CREATE_FIELD_LABELS: [&str; 4] = ["File", "Password", "Confirm", "Key file"];

/// The "new database" screen.
pub struct CreateState {
    pub fields: Vec<TextField>,
    pub focus: usize,
    pub reveal: bool,
    pub error: Option<String>,
    /// As in `UnlockState`: creation (key derivation) runs on the next tick.
    pub working: bool,
}

impl CreateState {
    fn new(path: &Path, keyfile: &str) -> Self {
        let mut fields: Vec<TextField> = (0..4).map(|_| TextField::default()).collect();
        fields[C_PATH].set_text(&path.display().to_string());
        fields[C_KEYFILE].set_text(keyfile);
        CreateState {
            fields,
            focus: C_PASS,
            reveal: false,
            error: None,
            working: false,
        }
    }

    /// The target file: `~` expanded, `.kdbx` appended when missing.
    pub fn path(&self) -> PathBuf {
        resolve_db_path(&self.fields[C_PATH].text)
    }

    fn keyfile(&self) -> Option<PathBuf> {
        let kf = self.fields[C_KEYFILE].text.trim();
        (!kf.is_empty()).then(|| expand_home(kf))
    }

    fn validate(&self) -> Result<(), String> {
        let raw = self.fields[C_PATH].text.trim();
        if raw.is_empty() {
            return Err("enter a file name for the new database".into());
        }
        let path = self.path();
        if path.exists() {
            return Err(format!(
                "{} already exists — choose another name",
                path.display()
            ));
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
            && !dir.is_dir()
        {
            return Err(format!("folder {} does not exist", dir.display()));
        }
        let pw = &self.fields[C_PASS].text;
        let keyfile = self.keyfile();
        if pw.is_empty() && keyfile.is_none() {
            return Err("choose a master password".into());
        }
        if *pw != self.fields[C_CONFIRM].text {
            return Err("the passwords don't match".into());
        }
        if let Some(kf) = keyfile
            && !kf.is_file()
        {
            return Err(format!("key file {} not found", kf.display()));
        }
        Ok(())
    }
}

fn expand_home(s: &str) -> PathBuf {
    match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(s),
    }
}

fn resolve_db_path(s: &str) -> PathBuf {
    let mut path = expand_home(s.trim());
    if path
        .extension()
        .is_none_or(|e| !e.eq_ignore_ascii_case("kdbx"))
    {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".kdbx");
        path.set_file_name(name);
    }
    path
}

/// A file name for a new database in `dir` that isn't taken yet.
fn suggest_new_path(dir: &Path) -> PathBuf {
    (1..)
        .map(|n| match n {
            1 => dir.join("New Database.kdbx"),
            n => dir.join(format!("New Database {n}.kdbx")),
        })
        .find(|p| !p.exists())
        .expect("unbounded search")
}

pub const F_TITLE: usize = 0;
pub const F_USER: usize = 1;
pub const F_PASS: usize = 2;
pub const F_URL: usize = 3;
pub const F_OTP: usize = 4;
pub const F_NOTES: usize = 5;
pub const ENTRY_FIELD_LABELS: [&str; 6] = ["Title", "Username", "Password", "URL", "OTP", "Notes"];

pub struct EntryForm {
    pub target: Option<EntryId>, // None = new entry
    pub group: GroupId,
    pub fields: Vec<TextField>,
    pub focus: usize,
    pub reveal: bool,
    pub modified: bool,
}

pub struct GroupForm {
    pub target: Option<GroupId>, // None = new group
    pub parent: GroupId,
    pub name: TextField,
    pub modified: bool,
}

pub enum Screen {
    /// Choose a database file.
    Picker(PickerState),
    Unlock(UnlockState),
    Create(CreateState),
    Browser,
    EntryEdit(EntryForm),
    GroupEdit(GroupForm),
}

pub struct GenState {
    pub opts: GenOpts,
    pub preview: Zeroizing<String>,
}

impl GenState {
    fn new() -> Self {
        let opts = GenOpts::default();
        let preview = generator::generate(&opts);
        GenState { opts, preview }
    }

    fn regen(&mut self) {
        self.preview = generator::generate(&self.opts);
    }
}

#[derive(Clone, Copy)]
pub enum PendingAction {
    DeleteEntry(EntryId),
    DeleteGroup(GroupId),
    DiscardForm,
    ConvertKdbx3 { then_quit: bool },
    QuitDirty,
}

pub struct ConfirmState {
    pub prompt: String,
    pub pending: PendingAction,
}

pub enum Overlay {
    Help,
    Generator(GenState),
    Confirm(ConfirmState),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Groups,
    Entries,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
}

/// A clickable region, recorded by the UI while drawing and resolved
/// against mouse clicks on the next event.
#[derive(Clone)]
pub enum Hit {
    /// Background of a browser pane: focuses it; scroll target.
    Pane(Pane),
    Group(GroupId),
    GroupToggle(GroupId),
    Entry(EntryId),
    /// Run a browser action (key bar buttons).
    Act(Action),
    /// Run an action on the selected entry (detail-pane buttons).
    EntryAct(Action),
    /// Copy a custom string field of the selected entry.
    CopyField(String),
    /// Replay a key press (buttons inside dialogs and forms).
    Key(KeyEvent),
    EditorField(usize),
    /// A field on the unlock or new-database screen.
    LoginField(usize),
    /// A row in the file picker (index into its visible items).
    PickerItem(usize),
    /// Click closes the current overlay.
    Dismiss,
}

// ---------------------------------------------------------------------------

pub struct App {
    pub screen: Screen,
    pub overlay: Option<Overlay>,
    pub vault: Option<Vault>,
    pub db_path: PathBuf,
    keyfile_arg: String,
    pub should_quit: bool,
    pub dirty: bool,
    kdbx3_ack: bool,

    pub pane: Pane,
    pub expanded: HashSet<GroupId>,
    pub sel_group: Option<GroupId>,
    pub sel_entry: Option<EntryId>,
    pub group_rows: Vec<(GroupId, usize)>,
    pub entry_rows: Vec<EntryId>,
    pub search: Option<String>,
    pub search_input: bool,
    pub reveal: bool,
    pub status: Option<(String, StatusKind, Instant)>,
    pub clipboard: Clipboard,

    // Mouse support. `hits` and the scroll offsets are written by the UI
    // during drawing, hence the interior mutability.
    pub hits: RefCell<Vec<(Rect, Hit)>>,
    pub mouse: Option<Position>,
    last_click: Option<(Position, Instant)>,
    pub group_offset: Cell<usize>,
    pub entry_offset: Cell<usize>,
    pub page_rows: Cell<usize>,
}

impl App {
    /// `db_path` None starts in the file picker.
    pub fn new(db_path: Option<PathBuf>, keyfile: Option<PathBuf>) -> Self {
        let keyfile_text = keyfile
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        // A path that doesn't exist yet opens the new-database screen.
        let screen = match &db_path {
            None => Screen::Picker(PickerState::new(Path::new("."), None)),
            Some(p) if p.exists() => Screen::Unlock(UnlockState::new(&keyfile_text)),
            Some(p) => Screen::Create(CreateState::new(p, &keyfile_text)),
        };
        let db_path = db_path.unwrap_or_default();
        App {
            screen,
            overlay: None,
            vault: None,
            db_path,
            keyfile_arg: keyfile_text,
            should_quit: false,
            dirty: false,
            kdbx3_ack: false,
            pane: Pane::Groups,
            expanded: HashSet::new(),
            sel_group: None,
            sel_entry: None,
            group_rows: Vec::new(),
            entry_rows: Vec::new(),
            search: None,
            search_input: false,
            reveal: false,
            status: None,
            clipboard: Clipboard::new(),
            hits: RefCell::new(Vec::new()),
            mouse: None,
            last_click: None,
            group_offset: Cell::new(0),
            entry_offset: Cell::new(0),
            page_rows: Cell::new(10),
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), StatusKind::Info, Instant::now()));
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), StatusKind::Error, Instant::now()));
    }

    pub fn on_tick(&mut self) {
        if let Some((_, _, at)) = &self.status
            && at.elapsed() >= STATUS_TTL
        {
            self.status = None;
        }
        self.try_unlock();
        self.try_create();
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.overlay.is_some() {
            self.on_overlay_key(key);
            return;
        }
        match self.screen {
            Screen::Picker(_) => self.on_picker_key(key),
            Screen::Unlock(_) => self.on_unlock_key(key),
            Screen::Create(_) => self.on_create_key(key),
            Screen::Browser => self.on_browser_key(key),
            Screen::EntryEdit(_) => self.on_entry_edit_key(key),
            Screen::GroupEdit(_) => self.on_group_edit_key(key),
        }
    }

    // -- mouse ----------------------------------------------------------------

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        let pos = Position::new(ev.column, ev.row);
        self.mouse = Some(pos);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let double = self
                    .last_click
                    .is_some_and(|(p, t)| p == pos && t.elapsed() < DOUBLE_CLICK);
                // A double click consumes the pair, so a triple click is
                // double + single rather than two doubles.
                self.last_click = (!double).then(|| (pos, Instant::now()));
                if let Some(hit) = self.hit_at(pos) {
                    self.on_click(hit, double);
                }
            }
            MouseEventKind::ScrollDown => self.on_scroll(pos, SCROLL_STEP),
            MouseEventKind::ScrollUp => self.on_scroll(pos, -SCROLL_STEP),
            _ => {}
        }
    }

    fn hit_at(&self, pos: Position) -> Option<Hit> {
        // Later regions are drawn on top, so search from the end.
        self.hits
            .borrow()
            .iter()
            .rev()
            .find(|(r, _)| r.contains(pos))
            .map(|(_, h)| h.clone())
    }

    fn on_click(&mut self, hit: Hit, double: bool) {
        match hit {
            Hit::Pane(pane) => self.set_pane(pane),
            Hit::Group(g) => {
                self.set_pane(Pane::Groups);
                self.select_group(g);
                if double {
                    self.toggle_expand(g);
                }
            }
            Hit::GroupToggle(g) => {
                self.set_pane(Pane::Groups);
                self.select_group(g);
                if !double {
                    self.toggle_expand(g);
                }
            }
            Hit::Entry(e) => {
                self.search_input = false;
                self.pane = Pane::Entries;
                self.select_entry(e);
                if double {
                    self.copy_entry_field(Action::CopyPass);
                }
            }
            Hit::Act(action) => {
                if action != Action::Search {
                    self.search_input = false;
                }
                self.do_action(action);
            }
            Hit::EntryAct(action) => {
                self.search_input = false;
                self.pane = Pane::Entries;
                self.do_action(action);
            }
            Hit::CopyField(key) => self.copy_custom_field(&key),
            Hit::Key(key) => self.on_key(key),
            Hit::EditorField(i) => {
                if let Screen::EntryEdit(f) = &mut self.screen {
                    f.focus = i;
                }
            }
            Hit::PickerItem(i) => {
                if let Screen::Picker(st) = &mut self.screen {
                    st.select(i);
                    if double {
                        let outcome = st.activate();
                        self.on_picker_outcome(outcome);
                    }
                }
            }
            Hit::LoginField(i) => match &mut self.screen {
                Screen::Unlock(st) => st.focus_keyfile = i == 1,
                Screen::Create(st) => st.focus = i,
                _ => {}
            },
            Hit::Dismiss => self.overlay = None,
        }
    }

    fn on_scroll(&mut self, pos: Position, delta: isize) {
        if let Screen::Picker(st) = &mut self.screen {
            st.move_by(delta);
            return;
        }
        if self.overlay.is_some() || !matches!(self.screen, Screen::Browser) {
            return;
        }
        let pane = self.hits.borrow().iter().rev().find_map(|(r, h)| match h {
            Hit::Pane(p) if r.contains(pos) => Some(*p),
            _ => None,
        });
        if let Some(pane) = pane {
            self.move_in(pane, delta);
        }
    }

    // -- unlock -------------------------------------------------------------

    fn on_unlock_key(&mut self, key: KeyEvent) {
        let Screen::Unlock(st) = &mut self.screen else {
            return;
        };
        if st.working {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('k') if ctrl => st.focus_keyfile = !st.focus_keyfile,
            KeyCode::Char('n') if ctrl => {
                let path = suggest_new_path(&self.db_dir());
                self.screen = Screen::Create(CreateState::new(&path, ""));
            }
            KeyCode::Char('o') if ctrl => {
                self.screen = Screen::Picker(PickerState::new(&self.db_dir(), Some(&self.db_path)));
            }
            KeyCode::Char('u') if ctrl => {
                let field = if st.focus_keyfile {
                    &mut st.keyfile
                } else {
                    &mut st.password
                };
                field.clear();
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                st.focus_keyfile = !st.focus_keyfile
            }
            KeyCode::Enter => {
                st.error = None;
                st.working = true;
            }
            _ => {
                let field = if st.focus_keyfile {
                    &mut st.keyfile
                } else {
                    &mut st.password
                };
                edit_field(field, key);
            }
        }
    }

    /// Runs the blocking unlock scheduled by Enter on the unlock screen.
    fn try_unlock(&mut self) {
        let Screen::Unlock(st) = &mut self.screen else {
            return;
        };
        if !st.working {
            return;
        }
        st.working = false;
        let password = st.password.text.clone();
        let keyfile_text = st.keyfile.text.trim().to_string();
        let keyfile = (!keyfile_text.is_empty()).then(|| PathBuf::from(&keyfile_text));

        match Vault::open(&self.db_path, &password, keyfile.as_deref()) {
            Ok(vault) => {
                self.vault = Some(vault);
                self.screen = Screen::Browser;
                self.init_after_unlock();
            }
            Err(e) => {
                if let Screen::Unlock(st) = &mut self.screen {
                    st.error = Some(format!("{e:#}"));
                    st.password.set_text("");
                }
            }
        }
    }

    // -- file picker --------------------------------------------------------

    fn on_picker_key(&mut self, key: KeyEvent) {
        let page = self.page_rows.get();
        let Screen::Picker(st) = &mut self.screen else {
            return;
        };
        let outcome = st.on_key(key, page);
        self.on_picker_outcome(outcome);
    }

    fn on_picker_outcome(&mut self, outcome: picker::Outcome) {
        match outcome {
            picker::Outcome::Stay => {}
            picker::Outcome::Open(path) => {
                self.db_path = path;
                self.screen = Screen::Unlock(UnlockState::new(&self.keyfile_arg));
            }
            picker::Outcome::CreateIn(dir) => {
                let path = suggest_new_path(&dir);
                self.screen = Screen::Create(CreateState::new(&path, &self.keyfile_arg));
            }
            picker::Outcome::Back => {
                if self.db_path.is_file() {
                    self.screen = Screen::Unlock(UnlockState::new(&self.keyfile_arg));
                } else {
                    self.should_quit = true;
                }
            }
            picker::Outcome::Quit => self.should_quit = true,
        }
    }

    /// Folder of the current database (for the picker and new files).
    fn db_dir(&self) -> PathBuf {
        self.db_path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    // -- new database -------------------------------------------------------

    fn on_create_key(&mut self, key: KeyEvent) {
        let Screen::Create(st) = &mut self.screen else {
            return;
        };
        if st.working {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                // Back to the unlock screen when there is a database to
                // unlock; otherwise to the file picker.
                if self.db_path.is_file() {
                    self.screen = Screen::Unlock(UnlockState::new(&self.keyfile_arg));
                } else {
                    let dir = st
                        .path()
                        .parent()
                        .filter(|d| d.is_dir())
                        .map(Path::to_path_buf);
                    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
                    self.screen = Screen::Picker(PickerState::new(&dir, None));
                }
            }
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('r') if ctrl => st.reveal = !st.reveal,
            KeyCode::Char('u') if ctrl => {
                st.fields[st.focus].clear();
            }
            KeyCode::Tab | KeyCode::Down => st.focus = (st.focus + 1) % 4,
            KeyCode::BackTab | KeyCode::Up => st.focus = (st.focus + 3) % 4,
            KeyCode::Enter if st.focus < C_CONFIRM => st.focus += 1,
            KeyCode::Enter => match st.validate() {
                Ok(()) => {
                    st.error = None;
                    st.working = true;
                }
                Err(msg) => st.error = Some(msg),
            },
            _ => {
                if edit_field(&mut st.fields[st.focus], key) {
                    st.error = None;
                }
            }
        }
    }

    /// Runs the blocking database creation scheduled by Enter.
    fn try_create(&mut self) {
        let Screen::Create(st) = &mut self.screen else {
            return;
        };
        if !st.working {
            return;
        }
        st.working = false;
        let path = st.path();
        let password = st.fields[C_PASS].text.clone();
        let keyfile = st.keyfile();

        match Vault::create(&path, &password, keyfile.as_deref()) {
            Ok(vault) => {
                let name = vault.file_name();
                self.db_path = path;
                self.vault = Some(vault);
                self.screen = Screen::Browser;
                self.init_after_unlock();
                self.set_status(format!(
                    "✓ created {name} — press a to add your first entry"
                ));
            }
            Err(e) => {
                if let Screen::Create(st) = &mut self.screen {
                    st.error = Some(format!("{e:#}"));
                }
            }
        }
    }

    fn init_after_unlock(&mut self) {
        let Some(v) = &self.vault else { return };
        let root = v.db.root().id();
        self.expanded =
            v.db.iter_all_groups()
                .filter(|g| g.is_expanded)
                .map(|g| g.id())
                .collect();
        self.expanded.insert(root);
        self.sel_group = Some(root);
        self.rebuild();
    }

    // -- browser ------------------------------------------------------------

    fn on_browser_key(&mut self, key: KeyEvent) {
        if self.search_input {
            self.on_search_key(key);
            return;
        }
        if let Some(action) = browser_action(key) {
            self.do_action(action);
        }
    }

    /// Browser actions, shared by the keymap and clickable buttons.
    fn do_action(&mut self, action: Action) {
        match action {
            Action::Down => self.move_in(self.pane, 1),
            Action::Up => self.move_in(self.pane, -1),
            Action::PageDown => self.move_in(self.pane, self.page_rows.get() as isize),
            Action::PageUp => self.move_in(self.pane, -(self.page_rows.get() as isize)),
            Action::Top => self.move_in(self.pane, isize::MIN / 2),
            Action::Bottom => self.move_in(self.pane, isize::MAX / 2),
            Action::Left => match self.pane {
                Pane::Groups => self.collapse_or_parent(),
                Pane::Entries => self.set_pane(Pane::Groups),
            },
            Action::Right => match self.pane {
                Pane::Groups => match self.sel_group {
                    Some(g) if self.has_subgroups(g) && !self.expanded.contains(&g) => {
                        self.toggle_expand(g)
                    }
                    _ => self.set_pane(Pane::Entries),
                },
                Pane::Entries => {}
            },
            Action::NextPane => self.set_pane(match self.pane {
                Pane::Groups => Pane::Entries,
                Pane::Entries => Pane::Groups,
            }),
            Action::Activate => match self.pane {
                Pane::Groups => self.set_pane(Pane::Entries),
                Pane::Entries => self.copy_entry_field(Action::CopyPass),
            },
            Action::ToggleExpand => {
                if self.pane == Pane::Groups
                    && let Some(g) = self.sel_group
                {
                    self.toggle_expand(g);
                }
            }
            Action::Search => {
                self.search_input = true;
                self.pane = Pane::Entries;
                if self.search.is_none() {
                    self.search = Some(String::new());
                }
            }
            Action::Escape => {
                if self.search.is_some() {
                    self.cancel_search();
                }
            }
            Action::CopyUser | Action::CopyPass | Action::CopyOtp | Action::CopyUrl => {
                self.copy_entry_field(action)
            }
            Action::OpenUrl => self.open_entry_url(),
            Action::ToggleReveal => {
                if self.sel_entry.is_some() {
                    self.reveal = !self.reveal;
                }
            }
            Action::NewEntry => self.open_new_entry(),
            Action::NewGroup => self.open_group_editor(None),
            Action::Edit => match self.pane {
                Pane::Groups => self.rename_selected_group(),
                Pane::Entries => self.open_entry_editor(self.sel_entry),
            },
            Action::RenameGroup => self.rename_selected_group(),
            Action::Delete => self.request_delete(),
            Action::Generator => self.overlay = Some(Overlay::Generator(GenState::new())),
            Action::Save => self.save_flow(false),
            Action::Quit => self.request_quit(),
            Action::Help => self.overlay = Some(Overlay::Help),
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Enter | KeyCode::Tab => {
                self.search_input = false;
                if self.search.as_deref().is_some_and(|q| q.trim().is_empty()) {
                    self.cancel_search();
                }
            }
            KeyCode::Down => self.move_in(Pane::Entries, 1),
            KeyCode::Up => self.move_in(Pane::Entries, -1),
            KeyCode::PageDown => self.move_in(Pane::Entries, self.page_rows.get() as isize),
            KeyCode::PageUp => self.move_in(Pane::Entries, -(self.page_rows.get() as isize)),
            KeyCode::Char('n') if ctrl => self.move_in(Pane::Entries, 1),
            KeyCode::Char('p') if ctrl => self.move_in(Pane::Entries, -1),
            KeyCode::Char('c') if ctrl => self.cancel_search(),
            KeyCode::Char('u') if ctrl => self.edit_search(|q| q.clear()),
            KeyCode::Char('w') if ctrl => self.edit_search(|q| {
                let keep = q.trim_end().rfind(' ').map(|i| i + 1).unwrap_or(0);
                q.truncate(keep);
            }),
            KeyCode::Backspace => self.edit_search(|q| {
                q.pop();
            }),
            KeyCode::Char(c) if !ctrl => self.edit_search(|q| q.push(c)),
            _ => {}
        }
    }

    fn edit_search(&mut self, f: impl FnOnce(&mut String)) {
        if let Some(q) = &mut self.search {
            f(q);
        }
        self.rebuild();
    }

    fn cancel_search(&mut self) {
        self.search_input = false;
        if self.search.take().is_some() {
            self.rebuild();
        }
    }

    /// Focus a pane. Going back to the groups pane ends any search, so the
    /// entries pane shows the selected group again.
    fn set_pane(&mut self, pane: Pane) {
        self.search_input = false;
        if pane == Pane::Groups {
            self.cancel_search();
        }
        self.pane = pane;
    }

    fn select_group(&mut self, g: GroupId) {
        if self.sel_group != Some(g) {
            self.sel_group = Some(g);
            self.reveal = false;
            self.entry_offset.set(0);
            self.rebuild();
        }
    }

    fn select_entry(&mut self, e: EntryId) {
        if self.sel_entry != Some(e) {
            self.sel_entry = Some(e);
            self.reveal = false;
        }
    }

    fn toggle_expand(&mut self, g: GroupId) {
        if !self.expanded.remove(&g) {
            self.expanded.insert(g);
        }
        self.rebuild();
    }

    fn has_subgroups(&self, g: GroupId) -> bool {
        self.vault
            .as_ref()
            .and_then(|v| v.db.group(g))
            .is_some_and(|g| g.group_ids().next().is_some())
    }

    fn collapse_or_parent(&mut self) {
        let Some(g) = self.sel_group else { return };
        if self.expanded.contains(&g) && self.has_subgroups(g) {
            self.toggle_expand(g);
            return;
        }
        let parent = self
            .vault
            .as_ref()
            .and_then(|v| v.db.group(g))
            .and_then(|g| g.parent().map(|p| p.id()));
        if let Some(p) = parent {
            self.select_group(p);
        }
    }

    fn rename_selected_group(&mut self) {
        match self.sel_group {
            Some(g) => self.open_group_editor(Some(g)),
            None => self.set_error("no group selected"),
        }
    }

    /// Move the selection in `pane` by `delta` rows, clamped to the list.
    fn move_in(&mut self, pane: Pane, delta: isize) {
        match pane {
            Pane::Groups => {
                if self.search.is_some() {
                    self.cancel_search();
                }
                let idx = self
                    .group_rows
                    .iter()
                    .position(|(g, _)| Some(*g) == self.sel_group)
                    .unwrap_or(0);
                let new = clamp_move(idx, delta, self.group_rows.len());
                if let Some((g, _)) = self.group_rows.get(new).copied() {
                    self.select_group(g);
                }
            }
            Pane::Entries => {
                let idx = self
                    .entry_rows
                    .iter()
                    .position(|e| Some(*e) == self.sel_entry)
                    .unwrap_or(0);
                let new = clamp_move(idx, delta, self.entry_rows.len());
                if let Some(e) = self.entry_rows.get(new).copied() {
                    self.select_entry(e);
                }
            }
        }
    }

    fn copy_entry_field(&mut self, which: Action) {
        let result: Result<(Zeroizing<String>, &'static str), String> = (|| {
            let id = self.sel_entry.ok_or("no entry selected")?;
            let v = self.vault.as_ref().ok_or("no vault")?;
            let e = v.db.entry(id).ok_or("entry not found")?;
            let (val, label) = match which {
                Action::CopyUser => (e.get_username().unwrap_or("").to_string(), "username"),
                Action::CopyPass => (e.get_password().unwrap_or("").to_string(), "password"),
                Action::CopyUrl => (e.get_url().unwrap_or("").to_string(), "URL"),
                Action::CopyOtp => {
                    let raw = e.get_raw_otp_value().ok_or("entry has no TOTP set up")?;
                    let code = totp::parse(raw)?.value_now().map_err(|e| e.to_string())?;
                    (code.code, "TOTP code")
                }
                _ => return Err("not a copy action".into()),
            };
            if val.is_empty() {
                return Err(format!("this entry has no {label}"));
            }
            Ok((Zeroizing::new(val), label))
        })();

        match result {
            Ok((val, label)) => self.copy_to_clipboard(&val, label),
            Err(msg) => self.set_error(msg),
        }
    }

    fn copy_custom_field(&mut self, key: &str) {
        let val = self
            .sel_entry
            .and_then(|id| self.vault.as_ref()?.db.entry(id))
            .and_then(|e| e.get(key).map(|s| Zeroizing::new(s.to_string())));
        match val {
            Some(val) if !val.is_empty() => self.copy_to_clipboard(&val, key),
            _ => self.set_error(format!("{key} is empty")),
        }
    }

    fn copy_to_clipboard(&mut self, val: &str, label: &str) {
        match self.clipboard.copy(val, label, DEFAULT_TTL) {
            Ok(()) => self.set_status(format!(
                "✓ copied {label} — clears in {}s",
                DEFAULT_TTL.as_secs()
            )),
            Err(e) => self.set_error(format!("clipboard error: {e:#}")),
        }
    }

    fn open_entry_url(&mut self) {
        let url = self
            .sel_entry
            .and_then(|id| self.vault.as_ref()?.db.entry(id))
            .map(|e| e.get_url().unwrap_or("").to_string());
        let Some(url) = url else {
            self.set_error("no entry selected");
            return;
        };
        if url.trim().is_empty() {
            self.set_error("this entry has no URL");
            return;
        }
        match open::open_url(&url) {
            Ok(opened) => self.set_status(format!("↗ opening {opened}")),
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    // -- editing ------------------------------------------------------------

    fn open_new_entry(&mut self) {
        let Some(group) = self.sel_group else {
            self.set_error("no group selected");
            return;
        };
        self.screen = Screen::EntryEdit(EntryForm {
            target: None,
            group,
            fields: (0..6).map(|_| TextField::default()).collect(),
            focus: F_TITLE,
            reveal: false,
            modified: false,
        });
    }

    fn open_entry_editor(&mut self, id: Option<EntryId>) {
        let Some(id) = id else {
            self.set_error("no entry selected");
            return;
        };
        let Some(v) = &self.vault else { return };
        let Some(e) = v.db.entry(id) else { return };
        let group = e.parent().id();
        let fields = vec![
            TextField::with_text(e.get_title().unwrap_or("")),
            TextField::with_text(e.get_username().unwrap_or("")),
            TextField::with_text(e.get_password().unwrap_or("")),
            TextField::with_text(e.get_url().unwrap_or("")),
            TextField::with_text(e.get_raw_otp_value().unwrap_or("")),
            TextField::with_text(e.get(fields::NOTES).unwrap_or("")),
        ];
        self.screen = Screen::EntryEdit(EntryForm {
            target: Some(id),
            group,
            fields,
            focus: F_TITLE,
            reveal: false,
            modified: false,
        });
    }

    fn open_group_editor(&mut self, target: Option<GroupId>) {
        let info = {
            let Some(v) = &self.vault else { return };
            match target {
                Some(id) => {
                    v.db.group(id)
                        .map(|g| (g.name.clone(), g.parent().map(|p| p.id()).unwrap_or(id)))
                }
                None => self.sel_group.map(|p| (String::new(), p)),
            }
        };
        let Some((name, parent)) = info else {
            self.set_error("no group selected");
            return;
        };
        self.screen = Screen::GroupEdit(GroupForm {
            target,
            parent,
            name: TextField::with_text(&name),
            modified: false,
        });
    }

    fn on_entry_edit_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                let modified = matches!(&self.screen, Screen::EntryEdit(f) if f.modified);
                if modified {
                    self.overlay = Some(Overlay::Confirm(ConfirmState {
                        prompt: "Discard your changes to this entry?".into(),
                        pending: PendingAction::DiscardForm,
                    }));
                } else {
                    self.screen = Screen::Browser;
                }
                return;
            }
            KeyCode::Char('s') if ctrl => {
                self.commit_entry_form();
                return;
            }
            KeyCode::Char('g') if ctrl => {
                self.overlay = Some(Overlay::Generator(GenState::new()));
                return;
            }
            KeyCode::Char('r') if ctrl => {
                if let Screen::EntryEdit(f) = &mut self.screen {
                    f.reveal = !f.reveal;
                }
                return;
            }
            _ => {}
        }

        let Screen::EntryEdit(form) = &mut self.screen else {
            return;
        };
        match key.code {
            KeyCode::Tab => form.focus = (form.focus + 1) % 6,
            KeyCode::BackTab => form.focus = (form.focus + 5) % 6,
            // In the multi-line notes field the arrows stay in the text.
            KeyCode::Down if form.focus != F_NOTES => form.focus += 1,
            KeyCode::Up if form.focus != F_NOTES => form.focus = (form.focus + 5) % 6,
            KeyCode::Up | KeyCode::Down => {
                let f = &mut form.fields[F_NOTES];
                let moved = notes_vertical(f, key.code == KeyCode::Up);
                if !moved && key.code == KeyCode::Up {
                    form.focus = F_OTP;
                }
            }
            KeyCode::Enter => {
                if form.focus == F_NOTES {
                    form.fields[F_NOTES].insert('\n');
                    form.modified = true;
                } else {
                    form.focus += 1;
                }
            }
            KeyCode::Char('u') if ctrl => {
                if form.fields[form.focus].clear() {
                    form.modified = true;
                }
            }
            _ => {
                if edit_field(&mut form.fields[form.focus], key) {
                    form.modified = true;
                }
            }
        }
    }

    fn commit_entry_form(&mut self) {
        // OTP settings that can't produce codes would only fail later (or,
        // before validation existed, crash the detail pane): keep the form
        // open on the OTP field instead.
        let otp_error = match &self.screen {
            Screen::EntryEdit(form) => {
                let raw = form.fields[F_OTP].text.trim();
                (!raw.is_empty())
                    .then(|| {
                        let otp =
                            Zeroizing::new(totp::normalize_otp(raw, &form.fields[F_TITLE].text));
                        totp::parse(&otp).err()
                    })
                    .flatten()
            }
            _ => None,
        };
        if let Some(msg) = otp_error {
            if let Screen::EntryEdit(form) = &mut self.screen {
                form.focus = F_OTP;
            }
            self.set_error(msg);
            return;
        }

        let Screen::EntryEdit(form) = mem::replace(&mut self.screen, Screen::Browser) else {
            return;
        };
        let Some(v) = &mut self.vault else { return };

        let title = form.fields[F_TITLE].text.to_string();
        let username = form.fields[F_USER].text.to_string();
        let password = Zeroizing::new(form.fields[F_PASS].text.to_string());
        let url = form.fields[F_URL].text.to_string();
        let otp_raw = Zeroizing::new(form.fields[F_OTP].text.trim().to_string());
        let notes = form.fields[F_NOTES].text.to_string();
        let otp =
            (!otp_raw.is_empty()).then(|| Zeroizing::new(totp::normalize_otp(&otp_raw, &title)));

        let id = match form.target {
            Some(id) => Some(id),
            None => v.db.group_mut(form.group).map(|mut g| g.add_entry().id()),
        };
        let Some(id) = id else {
            self.set_error("parent group vanished");
            return;
        };
        let Some(v) = &mut self.vault else { return };
        let Some(mut e) = v.db.entry_mut(id) else {
            return;
        };

        if form.target.is_some() {
            // Record the previous state in entry history, like KeePassXC does.
            e.edit_tracking(|e| {
                e.set_unprotected(fields::TITLE, &title);
                e.set_unprotected(fields::USERNAME, &username);
                e.set_protected(fields::PASSWORD, password.as_str());
                e.set_unprotected(fields::URL, &url);
                e.set_unprotected(fields::NOTES, &notes);
                if let Some(otp) = &otp {
                    e.set_protected(fields::OTP, otp.as_str());
                }
            });
        } else {
            e.edit(|e| {
                e.set_unprotected(fields::TITLE, &title);
                e.set_unprotected(fields::USERNAME, &username);
                e.set_protected(fields::PASSWORD, password.as_str());
                e.set_unprotected(fields::URL, &url);
                e.set_unprotected(fields::NOTES, &notes);
                if let Some(otp) = &otp {
                    e.set_protected(fields::OTP, otp.as_str());
                }
            });
        }
        if otp.is_none() {
            e.fields.remove(fields::OTP);
        }

        self.dirty = true;
        self.sel_entry = Some(id);
        self.pane = Pane::Entries;
        self.rebuild();
        let verb = if form.target.is_some() {
            "updated"
        } else {
            "created"
        };
        self.set_status(format!("entry {verb} — Ctrl-s to save to disk"));
    }

    fn on_group_edit_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                let modified = matches!(&self.screen, Screen::GroupEdit(f) if f.modified);
                if modified {
                    self.overlay = Some(Overlay::Confirm(ConfirmState {
                        prompt: "Discard your changes to this group?".into(),
                        pending: PendingAction::DiscardForm,
                    }));
                } else {
                    self.screen = Screen::Browser;
                }
            }
            KeyCode::Enter => self.commit_group_form(),
            KeyCode::Char('s') if ctrl => self.commit_group_form(),
            _ => {
                if let Screen::GroupEdit(form) = &mut self.screen {
                    let changed = if ctrl && key.code == KeyCode::Char('u') {
                        form.name.clear()
                    } else {
                        edit_field(&mut form.name, key)
                    };
                    form.modified |= changed;
                }
            }
        }
    }

    fn commit_group_form(&mut self) {
        let name_ok =
            matches!(&self.screen, Screen::GroupEdit(f) if !f.name.text.trim().is_empty());
        if !name_ok {
            self.set_error("group name must not be empty");
            return;
        }
        let Screen::GroupEdit(form) = mem::replace(&mut self.screen, Screen::Browser) else {
            return;
        };
        let name = form.name.text.trim().to_string();
        let mut status = "group updated — Ctrl-s to save to disk";
        {
            let Some(v) = &mut self.vault else { return };
            match form.target {
                Some(id) => {
                    if let Some(mut g) = v.db.group_mut(id) {
                        g.name = name;
                    }
                }
                None => match v.db.group_mut(form.parent) {
                    Some(mut parent) => {
                        let id = parent
                            .add_group()
                            .edit(|g| {
                                g.name = name;
                            })
                            .id();
                        self.expanded.insert(form.parent);
                        self.sel_group = Some(id);
                        status = "group created — Ctrl-s to save to disk";
                    }
                    None => status = "parent group vanished",
                },
            }
        }
        self.dirty = true;
        self.rebuild();
        self.set_status(status);
    }

    // -- delete -------------------------------------------------------------

    fn request_delete(&mut self) {
        let request: Result<ConfirmState, &'static str> = (|| {
            let v = self.vault.as_ref().ok_or("no vault")?;
            match self.pane {
                Pane::Entries => {
                    let id = self.sel_entry.ok_or("no entry selected")?;
                    let e = v.db.entry(id).ok_or("entry not found")?;
                    let title = e.get_title().unwrap_or("(untitled)").to_string();
                    let prompt = if self.entry_goes_to_bin(id) {
                        format!("Move entry '{title}' to the recycle bin?")
                    } else {
                        format!("Permanently delete entry '{title}'? This cannot be undone.")
                    };
                    Ok(ConfirmState {
                        prompt,
                        pending: PendingAction::DeleteEntry(id),
                    })
                }
                Pane::Groups => {
                    let id = self.sel_group.ok_or("no group selected")?;
                    if id == v.db.root().id() {
                        return Err("the root group cannot be deleted");
                    }
                    let name = v.db.group(id).map(|g| g.name.clone()).unwrap_or_default();
                    let prompt = if self.group_goes_to_bin(id) {
                        format!("Move group '{name}' and its contents to the recycle bin?")
                    } else {
                        format!(
                            "Permanently delete group '{name}' and everything in it? This cannot be undone."
                        )
                    };
                    Ok(ConfirmState {
                        prompt,
                        pending: PendingAction::DeleteGroup(id),
                    })
                }
            }
        })();
        match request {
            Ok(cs) => self.overlay = Some(Overlay::Confirm(cs)),
            Err(msg) => self.set_error(msg),
        }
    }

    fn bin_id(&self) -> Option<GroupId> {
        self.vault.as_ref()?.db.recycle_bin().map(|g| g.id())
    }

    /// Is `group` the recycle bin or inside it?
    fn in_bin(&self, group: GroupId) -> bool {
        match (self.bin_id(), &self.vault) {
            (Some(bin), Some(v)) => v.group_in(group, bin),
            _ => false,
        }
    }

    fn entry_goes_to_bin(&self, id: EntryId) -> bool {
        let Some(v) = &self.vault else { return false };
        let Some(parent) = v.db.entry(id).map(|e| e.parent().id()) else {
            return false;
        };
        self.bin_id().is_some() && !self.in_bin(parent)
    }

    fn group_goes_to_bin(&self, id: GroupId) -> bool {
        let (Some(bin), Some(v)) = (self.bin_id(), &self.vault) else {
            return false;
        };
        // Can't move into the bin if the bin lives inside the deleted group.
        !self.in_bin(id) && !v.group_in(bin, id)
    }

    fn delete_entry(&mut self, id: EntryId) {
        let to_bin = self.entry_goes_to_bin(id).then(|| self.bin_id()).flatten();
        let res = {
            let Some(v) = &mut self.vault else { return };
            let Some(mut e) = v.db.entry_mut(id) else {
                return;
            };
            match to_bin {
                Some(bin) => e
                    .move_to(bin)
                    .map(|_| "entry moved to recycle bin")
                    .map_err(|_| "failed to move entry to recycle bin"),
                None => {
                    e.remove();
                    Ok("entry deleted")
                }
            }
        };
        self.dirty = true;
        self.rebuild();
        match res {
            Ok(msg) => self.set_status(msg),
            Err(msg) => self.set_error(msg),
        }
    }

    fn delete_group(&mut self, id: GroupId) {
        let to_bin = self.group_goes_to_bin(id).then(|| self.bin_id()).flatten();
        let res = {
            let Some(v) = &mut self.vault else { return };
            let Some(mut g) = v.db.group_mut(id) else {
                return;
            };
            match to_bin {
                Some(bin) => g
                    .move_to(bin)
                    .map(|_| "group moved to recycle bin")
                    .map_err(|_| "failed to move group to recycle bin"),
                None => {
                    g.remove();
                    Ok("group deleted")
                }
            }
        };
        if self.sel_group == Some(id) {
            self.sel_group = self.vault.as_ref().map(|v| v.db.root().id());
        }
        self.dirty = true;
        self.rebuild();
        match res {
            Ok(msg) => self.set_status(msg),
            Err(msg) => self.set_error(msg),
        }
    }

    // -- save / quit ----------------------------------------------------------

    fn save_flow(&mut self, then_quit: bool) {
        let Some(v) = &self.vault else { return };
        if v.needs_kdbx4_upgrade() && !self.kdbx3_ack {
            self.overlay = Some(Overlay::Confirm(ConfirmState {
                prompt: "This database is KDBX3; keetui saves as KDBX4 (KeePassXC-compatible). Continue?"
                    .into(),
                pending: PendingAction::ConvertKdbx3 { then_quit },
            }));
            return;
        }
        self.do_save(then_quit);
    }

    fn do_save(&mut self, then_quit: bool) {
        let Some(v) = &mut self.vault else { return };
        let res = v.save();
        let name = v.file_name();
        match res {
            Ok(()) => {
                self.dirty = false;
                self.set_status(format!("✓ saved {name}"));
                if then_quit {
                    self.should_quit = true;
                }
            }
            Err(e) => self.set_error(format!("save failed: {e:#}")),
        }
    }

    fn request_quit(&mut self) {
        if self.dirty {
            self.overlay = Some(Overlay::Confirm(ConfirmState {
                prompt: "You have unsaved changes.".into(),
                pending: PendingAction::QuitDirty,
            }));
        } else {
            self.should_quit = true;
        }
    }

    // -- overlays -------------------------------------------------------------

    fn on_overlay_key(&mut self, key: KeyEvent) {
        // Take the overlay out so handlers may freely mutate self; it is put
        // back at the end unless the handler closed or replaced it.
        let Some(mut overlay) = mem::take(&mut self.overlay) else {
            return;
        };
        if matches!(overlay, Overlay::Generator(_)) && key.code == KeyCode::Enter {
            if let Overlay::Generator(st) = overlay {
                self.accept_generated(st);
            }
            return;
        }
        match &mut overlay {
            // Any key closes help.
            Overlay::Help => return,
            Overlay::Generator(st) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return,
                KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('l') | KeyCode::Right => {
                    st.opts.length = (st.opts.length + 1).min(MAX_LENGTH);
                    st.regen();
                }
                KeyCode::Char('-') | KeyCode::Char('h') | KeyCode::Left => {
                    st.opts.length = (st.opts.length.saturating_sub(1)).max(MIN_LENGTH);
                    st.regen();
                }
                KeyCode::Char('1') => {
                    st.opts.lower = !st.opts.lower;
                    st.regen();
                }
                KeyCode::Char('2') => {
                    st.opts.upper = !st.opts.upper;
                    st.regen();
                }
                KeyCode::Char('3') => {
                    st.opts.digits = !st.opts.digits;
                    st.regen();
                }
                KeyCode::Char('4') => {
                    st.opts.symbols = !st.opts.symbols;
                    st.regen();
                }
                KeyCode::Char('r') | KeyCode::Char(' ') => st.regen(),
                _ => {}
            },
            Overlay::Confirm(cs) => {
                let pending = cs.pending;
                match pending {
                    PendingAction::QuitDirty => match key.code {
                        KeyCode::Char('s') | KeyCode::Char('S') => {
                            self.save_flow(true);
                            return;
                        }
                        KeyCode::Char('d') | KeyCode::Char('D') => {
                            self.should_quit = true;
                            return;
                        }
                        KeyCode::Char('c') | KeyCode::Esc | KeyCode::Char('n') => return,
                        _ => {}
                    },
                    _ => match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                            self.perform(pending);
                            return;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return,
                        _ => {}
                    },
                }
            }
        }
        self.overlay = Some(overlay);
    }

    fn accept_generated(&mut self, st: GenState) {
        if let Screen::EntryEdit(form) = &mut self.screen {
            form.fields[F_PASS].set_text(&st.preview);
            form.modified = true;
            form.focus = F_PASS;
        } else {
            self.copy_to_clipboard(&st.preview, "generated password");
        }
    }

    fn perform(&mut self, pending: PendingAction) {
        match pending {
            PendingAction::DeleteEntry(id) => self.delete_entry(id),
            PendingAction::DeleteGroup(id) => self.delete_group(id),
            PendingAction::DiscardForm => self.screen = Screen::Browser,
            PendingAction::ConvertKdbx3 { then_quit } => {
                self.kdbx3_ack = true;
                self.do_save(then_quit);
            }
            PendingAction::QuitDirty => {}
        }
    }

    // -- derived rows ---------------------------------------------------------

    /// Rebuild the flattened group tree and the entry list from the database.
    /// Selections are kept by ID where possible.
    pub fn rebuild(&mut self) {
        let prev_entry = self.sel_entry;
        let Some(v) = &self.vault else { return };

        let mut rows = Vec::new();
        let mut stack: Vec<(GroupId, usize)> = vec![(v.db.root().id(), 0)];
        while let Some((id, depth)) = stack.pop() {
            rows.push((id, depth));
            if self.expanded.contains(&id)
                && let Some(g) = v.db.group(id)
            {
                let children: Vec<GroupId> = g.group_ids().collect();
                for c in children.into_iter().rev() {
                    stack.push((c, depth + 1));
                }
            }
        }
        self.group_rows = rows;

        if !self
            .group_rows
            .iter()
            .any(|(g, _)| Some(*g) == self.sel_group)
        {
            self.sel_group = self.group_rows.first().map(|(g, _)| *g);
        }

        self.entry_rows = match self.search.as_deref() {
            Some(q) if !q.trim().is_empty() => v.search(q),
            _ => {
                let mut es: Vec<(String, EntryId)> = self
                    .sel_group
                    .and_then(|g| v.db.group(g))
                    .map(|g| {
                        g.entries()
                            .map(|e| (e.get_title().unwrap_or("").to_lowercase(), e.id()))
                            .collect()
                    })
                    .unwrap_or_default();
                es.sort_by(|a, b| a.0.cmp(&b.0));
                es.into_iter().map(|(_, id)| id).collect()
            }
        };

        if !self.entry_rows.iter().any(|e| Some(*e) == self.sel_entry) {
            self.sel_entry = self.entry_rows.first().copied();
        }
        if self.sel_entry != prev_entry {
            self.reveal = false;
        }
    }
}

/// Generic single-field editing; returns true when the content changed.
fn edit_field(field: &mut TextField, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            field.insert(c);
            true
        }
        KeyCode::Backspace => field.backspace(),
        KeyCode::Delete => field.delete(),
        KeyCode::Left => {
            field.left();
            false
        }
        KeyCode::Right => {
            field.right();
            false
        }
        KeyCode::Home => {
            field.home();
            false
        }
        KeyCode::End => {
            field.end();
            false
        }
        _ => false,
    }
}

/// Move the cursor one line up/down in a multi-line field, keeping the
/// column where possible. Returns false at the first/last line.
fn notes_vertical(field: &mut TextField, up: bool) -> bool {
    let chars: Vec<char> = field.text.chars().collect();
    let line_start = |i: usize| {
        chars[..i]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |p| p + 1)
    };
    let line_end = |i: usize| {
        chars[i..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |p| i + p)
    };
    let start = line_start(field.cursor);
    let col = field.cursor - start;
    if up {
        if start == 0 {
            return false;
        }
        let prev_start = line_start(start - 1);
        field.cursor = (prev_start + col).min(start - 1);
    } else {
        let end = line_end(field.cursor);
        if end == chars.len() {
            return false;
        }
        let next_start = end + 1;
        field.cursor = (next_start + col).min(line_end(next_start));
    }
    true
}

fn clamp_move(idx: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (idx as isize)
        .saturating_add(delta)
        .clamp(0, len as isize - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// An App that has unlocked a fresh vault (password "pw", cheap key
    /// derivation) whose contents `fill` sets up.
    fn unlocked(fill: impl FnOnce(&mut keepass::Database)) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.kdbx");
        let mut db = keepass::Database::new();
        if let keepass::config::KdfConfig::Argon2 {
            iterations,
            memory,
            parallelism,
            ..
        } = &mut db.config.kdf_config
        {
            (*iterations, *memory, *parallelism) = (1, 64 * 1024, 1);
        }
        fill(&mut db);
        let mut file = std::fs::File::create(&path).unwrap();
        db.save(&mut file, keepass::DatabaseKey::new().with_password("pw"))
            .unwrap();

        let mut app = App::new(Some(path), None);
        let Screen::Unlock(st) = &mut app.screen else {
            panic!("expected the unlock screen");
        };
        st.password.set_text("pw");
        st.working = true;
        app.on_tick();
        assert!(matches!(app.screen, Screen::Browser), "unlock failed");
        (dir, app)
    }

    #[test]
    fn unusable_totp_settings_do_not_crash() {
        let (_dir, mut app) = unlocked(|db| {
            db.root_mut().add_entry().edit(|e| {
                e.set_unprotected(fields::TITLE, "Broken");
                e.set_protected(
                    fields::OTP,
                    "otpauth://totp/x?secret=JBSWY3DPEHPK3PXP&period=0",
                );
            });
        });
        // The entry is selected right after unlock; drawing it used to panic.
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();

        app.on_key(key(KeyCode::Char('t')));
        assert!(
            matches!(&app.status, Some((msg, StatusKind::Error, _)) if msg.contains("period")),
            "copying the TOTP code should report the bad period"
        );
    }

    #[test]
    fn editor_rejects_unusable_totp_settings() {
        let (_dir, mut app) = unlocked(|_| {});
        app.on_key(key(KeyCode::Char('a')));
        let Screen::EntryEdit(form) = &mut app.screen else {
            panic!("expected the entry editor");
        };
        form.fields[F_OTP].set_text("otpauth://totp/x?secret=JBSWY3DPEHPK3PXP&digits=64");
        app.on_key(ctrl('s'));
        assert!(matches!(&app.screen, Screen::EntryEdit(f) if f.focus == F_OTP));
        assert!(!app.dirty);

        let Screen::EntryEdit(form) = &mut app.screen else {
            unreachable!()
        };
        form.fields[F_OTP].set_text("JBSWY3DPEHPK3PXP");
        app.on_key(ctrl('s'));
        assert!(matches!(app.screen, Screen::Browser));
        assert!(app.dirty);
    }

    #[test]
    fn text_field_editing() {
        let mut f = TextField::default();
        f.insert('a');
        f.insert('b');
        f.insert('c');
        assert_eq!(f.text.as_str(), "abc");
        f.left();
        f.backspace();
        assert_eq!(f.text.as_str(), "ac");
        f.end();
        f.insert('é');
        f.insert('x');
        assert_eq!(f.text.as_str(), "acéx");
        f.left();
        f.left();
        f.delete();
        assert_eq!(f.text.as_str(), "acx");
    }

    #[test]
    fn new_database_paths() {
        assert_eq!(resolve_db_path("/x/vault"), PathBuf::from("/x/vault.kdbx"));
        assert_eq!(resolve_db_path(" /x/v.KDBX "), PathBuf::from("/x/v.KDBX"));
        assert_eq!(resolve_db_path("/x/my.db"), PathBuf::from("/x/my.db.kdbx"));
        let dir = tempfile::tempdir().unwrap();
        let first = suggest_new_path(dir.path());
        assert_eq!(first, dir.path().join("New Database.kdbx"));
        std::fs::write(&first, b"").unwrap();
        let second = suggest_new_path(dir.path());
        assert_eq!(second, dir.path().join("New Database 2.kdbx"));
    }

    #[test]
    fn create_form_validation() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = CreateState::new(&dir.path().join("New.kdbx"), "");
        assert!(st.validate().unwrap_err().contains("master password"));
        st.fields[C_PASS].set_text("hunter2");
        st.fields[C_CONFIRM].set_text("hunter3");
        assert!(st.validate().unwrap_err().contains("match"));
        st.fields[C_CONFIRM].set_text("hunter2");
        assert!(st.validate().is_ok());
        st.fields[C_PATH].set_text(&dir.path().join("nope/New.kdbx").display().to_string());
        assert!(st.validate().unwrap_err().contains("does not exist"));
    }

    #[test]
    fn clamp_move_bounds() {
        assert_eq!(clamp_move(0, -1, 5), 0);
        assert_eq!(clamp_move(4, 1, 5), 4);
        assert_eq!(clamp_move(2, 1, 5), 3);
        assert_eq!(clamp_move(0, 1, 0), 0);
        assert_eq!(clamp_move(2, isize::MAX / 2, 5), 4);
        assert_eq!(clamp_move(2, isize::MIN / 2, 5), 0);
    }

    #[test]
    fn notes_cursor_moves_between_lines() {
        let mut f = TextField::with_text("abcd\nef\nghij");
        f.cursor = 3; // "abc|d"
        assert!(notes_vertical(&mut f, false));
        assert_eq!(f.cursor, 7); // end of "ef"
        assert!(notes_vertical(&mut f, false));
        assert_eq!(f.cursor, 10); // "gh|ij"
        assert!(!notes_vertical(&mut f, false));
        assert!(notes_vertical(&mut f, true));
        assert!(notes_vertical(&mut f, true));
        assert_eq!(f.cursor, 2);
        assert!(!notes_vertical(&mut f, true));
    }
}
