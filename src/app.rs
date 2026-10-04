//! Application state machine: screens, selection, editing, dirty tracking.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use keepass::db::{EntryId, GroupId, Times, Value, fields};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use zeroize::Zeroizing;

use crate::clipboard::{Clipboard, DEFAULT_TTL};
use crate::db::{ChangedOnDisk, Vault};
use crate::event::{Action, browser_action};
use crate::generator::{self, GenOpts, MAX_LENGTH, MIN_LENGTH};
use crate::picker::{self, PickerState};
use crate::{open, totp};

const STATUS_TTL: Duration = Duration::from_secs(5);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const SCROLL_STEP: isize = 3;
/// A revealed password hides itself again after this long.
const REVEAL_TTL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Text editing primitive shared by all forms. The buffer is Zeroizing so
// secrets typed into any field are wiped when the form is dropped, and it
// never lets `String` reallocate: that would free the old buffer unwiped,
// leaving a copy of what was typed so far on the heap.

/// Every field starts with room for this much, so typical input never has
/// to grow the buffer at all.
const FIELD_CAPACITY: usize = 256;

pub struct TextField {
    pub text: Zeroizing<String>,
    pub cursor: usize, // char index
}

impl Default for TextField {
    fn default() -> Self {
        TextField {
            text: Zeroizing::new(String::with_capacity(FIELD_CAPACITY)),
            cursor: 0,
        }
    }
}

impl TextField {
    pub fn with_text(s: &str) -> Self {
        let mut field = TextField::default();
        field.set_text(s);
        field
    }

    /// Make room for `extra` more bytes by moving to a bigger buffer; the
    /// old one is zeroized as it drops.
    fn reserve(&mut self, extra: usize) {
        let needed = self.text.len() + extra;
        if needed > self.text.capacity() {
            let capacity = needed.max(2 * self.text.capacity());
            let mut grown = Zeroizing::new(String::with_capacity(capacity));
            grown.push_str(&self.text);
            self.text = grown;
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
        self.reserve(c.len_utf8());
        let i = self.byte_idx();
        self.text.insert(i, c);
        self.cursor += 1;
    }

    /// Insert `s` at the cursor; returns true if anything was inserted.
    pub fn insert_str(&mut self, s: &str) -> bool {
        self.reserve(s.len());
        let i = self.byte_idx();
        self.text.insert_str(i, s);
        self.cursor += s.chars().count();
        !s.is_empty()
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
        let mut text = Zeroizing::new(String::with_capacity(s.len().max(FIELD_CAPACITY)));
        text.push_str(s);
        self.text = text;
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
    /// Set when Enter was pressed; the unlock starts on the next tick so the
    /// "unlocking…" frame gets drawn first.
    pub working: bool,
    /// The unlock running on a worker thread. Key derivation takes a moment
    /// by design, and a hostile file can make it take forever, so the UI
    /// stays live and Esc can abandon it.
    job: Option<Receiver<anyhow::Result<Vault>>>,
    /// Set when the session locked itself, saying why. Unlocking then picks
    /// up where the session left off.
    pub locked: Option<String>,
}

impl UnlockState {
    fn new(keyfile: &str) -> Self {
        UnlockState {
            password: TextField::default(),
            keyfile: TextField::with_text(keyfile),
            focus_keyfile: false,
            error: None,
            working: false,
            job: None,
            locked: None,
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

/// What saving the entry form does to the OTP field.
enum OtpChange {
    /// Untouched: keep the stored value exactly as it is, even if keetui
    /// can't use it (it may be a format another client understands).
    Keep,
    Remove,
    Set(Zeroizing<String>),
}

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
    /// Save a database in a format keepass-rs can't write as KDBX 4.1.
    ConvertFormat {
        then_quit: bool,
    },
    /// Save over changes another program made to the file.
    OverwriteExternal {
        then_quit: bool,
    },
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
    /// The user agreed to save this database in another format.
    convert_ack: bool,

    pub pane: Pane,
    pub expanded: HashSet<GroupId>,
    pub sel_group: Option<GroupId>,
    pub sel_entry: Option<EntryId>,
    pub group_rows: Vec<(GroupId, usize)>,
    pub entry_rows: Vec<EntryId>,
    pub search: Option<String>,
    pub search_input: bool,
    pub reveal: bool,
    /// When the password was last revealed, for hiding it again.
    revealed_at: Instant,
    pub status: Option<(String, StatusKind, Instant)>,
    pub clipboard: Clipboard,

    /// Lock after this long without input; None never locks.
    lock_after: Option<Duration>,
    /// Last key press, click, scroll or paste.
    last_input: Instant,
    /// Locked with unsaved work: the screen and overlay to return to once
    /// the master password is re-entered. The vault stays loaded meanwhile.
    resume: Option<(Screen, Option<Overlay>)>,

    // Mouse support. `hits` and the scroll offsets are written by the UI
    // during drawing, hence the interior mutability.
    pub hits: RefCell<Vec<(Rect, Hit)>>,
    pub mouse: Option<Position>,
    last_click: Option<(Position, Instant)>,
    pub group_offset: Cell<usize>,
    pub entry_offset: Cell<usize>,
    pub page_rows: Cell<usize>,
    /// The last frame was too small to show the UI; input is ignored until
    /// it fits again, so keys can't act on forms that aren't drawn.
    pub too_small: Cell<bool>,
}

impl App {
    /// `db_path` None starts in the file picker.
    pub fn new(
        db_path: Option<PathBuf>,
        keyfile: Option<PathBuf>,
        lock_after: Option<Duration>,
    ) -> Self {
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
            convert_ack: false,
            pane: Pane::Groups,
            expanded: HashSet::new(),
            sel_group: None,
            sel_entry: None,
            group_rows: Vec::new(),
            entry_rows: Vec::new(),
            search: None,
            search_input: false,
            reveal: false,
            revealed_at: Instant::now(),
            status: None,
            clipboard: Clipboard::new(),
            lock_after,
            last_input: Instant::now(),
            resume: None,
            hits: RefCell::new(Vec::new()),
            mouse: None,
            last_click: None,
            group_offset: Cell::new(0),
            entry_offset: Cell::new(0),
            page_rows: Cell::new(10),
            too_small: Cell::new(false),
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
        if self.reveal && self.revealed_at.elapsed() >= REVEAL_TTL {
            self.reveal = false;
        }
        if let Some(after) = self.lock_after
            && self.last_input.elapsed() >= after
        {
            self.lock(&format!("Locked after {} of inactivity", minutes(after)));
        }
        self.try_unlock();
        self.try_create();
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.last_input = Instant::now();
        if self.too_small.get() {
            // Only quitting, and only when nothing would be lost.
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl_c && !self.has_unsaved_work() {
                self.should_quit = true;
            }
            return;
        }
        if key.code == KeyCode::Char('l')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.is_unlocked()
        {
            self.lock("Locked");
            return;
        }
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

    /// A bracketed paste goes into the focused text field, or the search
    /// box while searching; anywhere else it is ignored. (Without bracketed
    /// paste it would arrive as keystrokes, control bytes included, and run
    /// as commands.) Only the notes field keeps line breaks.
    pub fn on_paste(&mut self, text: &str) {
        self.last_input = Instant::now();
        if self.overlay.is_some() || self.too_small.get() {
            return;
        }
        // A paste may well be a password: build what gets inserted in a
        // zeroizing buffer, sized up front so it never reallocates.
        let mut line = Zeroizing::new(String::with_capacity(text.len()));
        line.extend(text.chars().filter(|c| !c.is_control()));
        if matches!(self.screen, Screen::Browser) {
            if self.search_input {
                self.edit_search(|q| q.push_str(&line));
            }
            return;
        }
        match &mut self.screen {
            Screen::Picker(st) => {
                st.filter.push_str(&line);
                st.selected = 0;
            }
            Screen::Unlock(st) if !st.working => {
                let field = if st.focus_keyfile {
                    &mut st.keyfile
                } else {
                    &mut st.password
                };
                field.insert_str(&line);
            }
            Screen::Create(st) if !st.working => {
                if st.fields[st.focus].insert_str(&line) {
                    st.error = None;
                }
            }
            Screen::EntryEdit(form) => {
                let text = if form.focus == F_NOTES {
                    multiline(text)
                } else {
                    line
                };
                if form.fields[form.focus].insert_str(&text) {
                    form.modified = true;
                }
            }
            Screen::GroupEdit(form) => {
                if form.name.insert_str(&line) {
                    form.modified = true;
                }
            }
            _ => {}
        }
    }

    // -- mouse ----------------------------------------------------------------

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        let pos = Position::new(ev.column, ev.row);
        self.mouse = Some(pos);
        if ev.kind != MouseEventKind::Moved {
            self.last_input = Instant::now();
        }
        if self.too_small.get() {
            return;
        }
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if st.working {
            // Only cancelling and quitting work while unlocking.
            match key.code {
                KeyCode::Esc => {
                    // The worker's result, if it ever comes, is dropped.
                    st.job = None;
                    st.working = false;
                    st.error = Some("unlocking cancelled".into());
                }
                KeyCode::Char('c') if ctrl => self.request_quit(),
                _ => {}
            }
            return;
        }
        match key.code {
            // Asks first when locked with unsaved work.
            KeyCode::Esc => self.request_quit(),
            KeyCode::Char('c') if ctrl => self.request_quit(),
            KeyCode::Char('k') if ctrl => st.focus_keyfile = !st.focus_keyfile,
            KeyCode::Char('n' | 'o') if ctrl && self.resume.is_some() => {
                st.error = Some("unlock first: this session has unsaved changes".into());
            }
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

    /// Drives the unlock scheduled by Enter on the unlock screen: starts it
    /// on a worker thread, then polls it on each tick.
    fn try_unlock(&mut self) {
        let Screen::Unlock(st) = &mut self.screen else {
            return;
        };
        if !st.working {
            return;
        }
        let keyfile_text = st.keyfile.text.trim().to_string();
        let keyfile = (!keyfile_text.is_empty()).then(|| PathBuf::from(&keyfile_text));
        let resuming = st.locked.is_some();

        // Locked with unsaved work: the vault is still loaded, so check the
        // key against it rather than re-reading the file. That needs no key
        // derivation, so no worker either.
        if self.resume.is_some()
            && let Some(v) = &self.vault
        {
            st.working = false;
            let password = st.password.text.clone();
            match v.key_matches(&password, keyfile.as_deref()) {
                Ok(true) => {
                    if let Some((screen, overlay)) = self.resume.take() {
                        self.screen = screen;
                        self.overlay = overlay;
                    }
                }
                Ok(false) => self.unlock_failed("wrong password or key file".into()),
                Err(e) => self.unlock_failed(format!("{e:#}")),
            }
            return;
        }

        if st.job.is_none() {
            let password = st.password.text.clone();
            st.job = Some(spawn_unlock(self.db_path.clone(), password, keyfile));
            return;
        }
        let result = match st.job.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(TryRecvError::Empty)) | None => return,
            Some(Err(TryRecvError::Disconnected)) => Err(anyhow::anyhow!("unlocking failed")),
        };
        st.job = None;
        st.working = false;

        match result {
            Ok(vault) => {
                self.vault = Some(vault);
                self.screen = Screen::Browser;
                self.keyfile_arg = keyfile_text;
                if resuming {
                    // Selection and expanded groups survived the lock.
                    self.rebuild();
                } else {
                    self.init_after_unlock();
                }
            }
            Err(e) => self.unlock_failed(format!("{e:#}")),
        }
    }

    fn unlock_failed(&mut self, msg: String) {
        if let Screen::Unlock(st) = &mut self.screen {
            st.error = Some(msg);
            st.password.set_text("");
        }
    }

    /// Is a database open and on screen (as opposed to locked)?
    fn is_unlocked(&self) -> bool {
        self.vault.is_some()
            && matches!(
                self.screen,
                Screen::Browser | Screen::EntryEdit(_) | Screen::GroupEdit(_)
            )
    }

    /// Locked, with unsaved work waiting behind the master password.
    pub fn locked_with_unsaved_work(&self) -> bool {
        self.resume.is_some()
    }

    /// Changes not yet on disk, including a modified edit form (on screen,
    /// or set aside by a lock).
    fn has_unsaved_work(&self) -> bool {
        let form_modified = |screen: &Screen| match screen {
            Screen::EntryEdit(f) => f.modified,
            Screen::GroupEdit(f) => f.modified,
            _ => false,
        };
        self.dirty
            || form_modified(&self.screen)
            || self.resume.as_ref().is_some_and(|(s, _)| form_modified(s))
    }

    /// Lock the session behind the master password. Without unsaved work
    /// the decrypted database is dropped, zeroizing the key and protected
    /// fields; with it, the database and the screen are set aside instead,
    /// so nothing is lost. Selection and expanded groups (IDs only) stay.
    fn lock(&mut self, reason: &str) {
        if !self.is_unlocked() {
            return;
        }
        let keep = self.has_unsaved_work();
        let mut st = UnlockState::new(&self.keyfile_arg);
        st.locked = Some(reason.to_string());
        let screen = mem::replace(&mut self.screen, Screen::Unlock(st));
        let overlay = self.overlay.take();
        self.reveal = false;
        self.search_input = false;
        self.status = None;
        if keep {
            self.resume = Some((screen, overlay));
        } else {
            self.vault = None;
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
                    self.revealed_at = Instant::now();
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

    /// Work out what saving `form` does to the OTP field. Only a value the
    /// user typed is normalized and validated; settings that can't produce
    /// codes are refused then, rather than failing later.
    fn otp_change(&self, form: &EntryForm) -> Result<OtpChange, String> {
        let text = form.fields[F_OTP].text.as_str();
        let unchanged = form
            .target
            .and_then(|id| self.vault.as_ref()?.db.entry(id))
            .map_or(text.is_empty(), |e| {
                e.get_raw_otp_value().unwrap_or("") == text
            });
        if unchanged {
            return Ok(OtpChange::Keep);
        }
        if text.trim().is_empty() {
            return Ok(OtpChange::Remove);
        }
        let otp = totp::normalize_otp(text, &form.fields[F_TITLE].text);
        totp::parse(&otp)?;
        Ok(OtpChange::Set(otp))
    }

    fn commit_entry_form(&mut self) {
        // An existing entry that wasn't changed: just close the editor. A
        // commit would add a history item, bump the modification time (which
        // can make it win a merge against a real edit elsewhere) and mark
        // the vault unsaved.
        if matches!(&self.screen, Screen::EntryEdit(f) if f.target.is_some() && !f.modified) {
            self.screen = Screen::Browser;
            return;
        }
        let otp = match &self.screen {
            Screen::EntryEdit(form) => self.otp_change(form),
            _ => return,
        };
        let otp = match otp {
            Ok(otp) => otp,
            Err(msg) => {
                // Keep the form open on the OTP field.
                if let Screen::EntryEdit(form) = &mut self.screen {
                    form.focus = F_OTP;
                }
                self.set_error(msg);
                return;
            }
        };

        let Screen::EntryEdit(form) = mem::replace(&mut self.screen, Screen::Browser) else {
            return;
        };
        let Some(v) = &mut self.vault else { return };

        let title = form.fields[F_TITLE].text.to_string();
        let username = form.fields[F_USER].text.to_string();
        let password = Zeroizing::new(form.fields[F_PASS].text.to_string());
        let url = form.fields[F_URL].text.to_string();
        let notes = form.fields[F_NOTES].text.to_string();

        // Keep each field's protection, and add what the vault's memory
        // protection settings ask for: writing a field unprotected would drop
        // it quietly (keepass-rs doesn't re-apply the settings on save).
        let settings = v.db.meta.memory_protection.clone().unwrap_or_default();
        let existing = form.target.and_then(|id| v.db.entry(id));
        let value = |key: &str, text: &str, by_setting: bool| {
            let had_it = existing
                .as_ref()
                .and_then(|e| e.fields.get(key))
                .is_some_and(|value| value.is_protected());
            if by_setting || had_it {
                Value::protected(text)
            } else {
                Value::unprotected(text)
            }
        };
        let updates = [
            (
                fields::TITLE,
                value(fields::TITLE, &title, settings.protect_title),
            ),
            (
                fields::USERNAME,
                value(fields::USERNAME, &username, settings.protect_username),
            ),
            (fields::PASSWORD, Value::protected(password.as_str())),
            (fields::URL, value(fields::URL, &url, settings.protect_url)),
            (
                fields::NOTES,
                value(fields::NOTES, &notes, settings.protect_notes),
            ),
        ];

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
                for (key, value) in updates {
                    e.set(key, value);
                }
                match &otp {
                    OtpChange::Keep => {}
                    OtpChange::Remove => {
                        e.fields.remove(fields::OTP);
                    }
                    OtpChange::Set(otp) => e.set_protected(fields::OTP, otp.as_str()),
                }
            });
        } else {
            e.edit(|e| {
                for (key, value) in updates {
                    e.set(key, value);
                }
                if let OtpChange::Set(otp) = &otp {
                    e.set_protected(fields::OTP, otp.as_str());
                }
            });
        }
        if form.target.is_some() {
            v.prune_history(id);
        }

        self.dirty = true;
        // Through select_entry, which hides a password revealed for the
        // previously selected entry.
        self.select_entry(id);
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
                        // Tracked, so merges see when it was renamed.
                        g.edit_tracking(|g| g.name = name);
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
                // Not the tracked move, which would also add a history item.
                // Merges need the new location's timestamp.
                Some(bin) => e
                    .move_to(bin)
                    .map(|_| {
                        e.times.location_changed = Some(Times::now());
                        "entry moved to recycle bin"
                    })
                    .map_err(|_| "failed to move entry to recycle bin"),
                None => {
                    // Tracked, so the deletion is recorded in DeletedObjects
                    // and a sync with an older copy doesn't bring it back.
                    e.track_changes().remove();
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
                // Tracked, so merges see when it moved.
                Some(bin) => g
                    .track_changes()
                    .move_to(bin)
                    .map(|_| "group moved to recycle bin")
                    .map_err(|_| "failed to move group to recycle bin"),
                // Tracked: records the group and everything in it as deleted.
                None => g
                    .track_changes()
                    .remove()
                    .map(|_| "group deleted")
                    .map_err(|_| "the root group cannot be deleted"),
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
        if let Some(format) = v.format_needing_conversion()
            && !self.convert_ack
        {
            self.overlay = Some(Overlay::Confirm(ConfirmState {
                prompt: format!(
                    "This database is {format}, which keetui can't write. Save it as \
                     KDBX 4.1 (needs KeePassXC 2.7+ or KeePass 2.48+)?"
                ),
                pending: PendingAction::ConvertFormat { then_quit },
            }));
            return;
        }
        self.do_save(then_quit, false);
    }

    fn do_save(&mut self, then_quit: bool, overwrite: bool) {
        let Some(v) = &mut self.vault else { return };
        let res = if overwrite {
            v.save_overwriting()
        } else {
            v.save()
        };
        let name = v.file_name();
        match res {
            Ok(()) => {
                self.dirty = false;
                self.set_status(format!("✓ saved {name}"));
                if then_quit {
                    self.should_quit = true;
                }
            }
            Err(e) if e.is::<ChangedOnDisk>() => {
                self.overlay = Some(Overlay::Confirm(ConfirmState {
                    prompt: format!(
                        "{name} was changed by another program since keetui opened it. \
                         Overwrite those changes with yours? (The current file is \
                         kept as {name}.bak.)"
                    ),
                    pending: PendingAction::OverwriteExternal { then_quit },
                }))
            }
            Err(e) => self.set_error(format!("save failed: {e:#}")),
        }
    }

    fn request_quit(&mut self) {
        if self.has_unsaved_work() {
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
            PendingAction::ConvertFormat { then_quit } => {
                self.convert_ack = true;
                self.do_save(then_quit, false);
            }
            PendingAction::OverwriteExternal { then_quit } => self.do_save(then_quit, true),
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

/// Open the vault on a worker thread. If the unlock is abandoned nobody
/// receives the result, and the vault is dropped (and zeroized) with it.
fn spawn_unlock(
    path: PathBuf,
    password: Zeroizing<String>,
    keyfile: Option<PathBuf>,
) -> Receiver<anyhow::Result<Vault>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(Vault::open(&path, &password, keyfile.as_deref()));
    });
    rx
}

/// Pasted text for a multi-line field: line breaks normalized to `\n`,
/// other control characters dropped. Never longer than the input, so the
/// zeroizing buffer is sized once.
fn multiline(text: &str) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(text.len()));
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push('\n');
            }
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// "5 minutes" style rendering of a lock timeout.
fn minutes(d: Duration) -> String {
    match d.as_secs() {
        60 => "1 minute".into(),
        s if s % 60 == 0 => format!("{} minutes", s / 60),
        s => format!("{s} seconds"),
    }
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

    /// Tick until a pending unlock has finished (it runs on a worker).
    fn finish_unlock(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.on_tick();
            let working = matches!(&app.screen, Screen::Unlock(st) if st.working);
            if !working || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// An App that has unlocked a fresh vault (password "pw", cheap key
    /// derivation) whose contents `fill` sets up.
    fn unlocked(fill: impl FnOnce(&mut keepass::Database)) -> (tempfile::TempDir, App) {
        let (dir, path) = vault_file(fill);
        let mut app = App::new(Some(path), None, None);
        let Screen::Unlock(st) = &mut app.screen else {
            panic!("expected the unlock screen");
        };
        st.password.set_text("pw");
        st.working = true;
        finish_unlock(&mut app);
        assert!(matches!(app.screen, Screen::Browser), "unlock failed");
        (dir, app)
    }

    /// A fresh vault file (password "pw", cheap key derivation).
    fn vault_file(fill: impl FnOnce(&mut keepass::Database)) -> (tempfile::TempDir, PathBuf) {
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
        (dir, path)
    }

    #[test]
    fn unlocking_can_be_cancelled() {
        let (_dir, path) = vault_file(|_| {});
        let mut app = App::new(Some(path), None, None);
        let Screen::Unlock(st) = &mut app.screen else {
            panic!("expected the unlock screen");
        };
        st.password.set_text("pw");
        app.on_key(key(KeyCode::Enter));
        app.on_tick(); // starts the worker
        app.on_key(key(KeyCode::Char('x'))); // ignored while unlocking
        app.on_key(key(KeyCode::Esc));
        let Screen::Unlock(st) = &app.screen else {
            panic!("expected the unlock screen");
        };
        assert!(!st.working);
        assert_eq!(st.error.as_deref(), Some("unlocking cancelled"));
        assert_eq!(st.password.text.as_str(), "pw");

        // Whatever the worker produces is discarded.
        std::thread::sleep(Duration::from_millis(300));
        app.on_tick();
        assert!(matches!(app.screen, Screen::Unlock(_)));
        assert!(app.vault.is_none());
    }

    /// An unlocked app that locks after a minute idle, with two entries.
    fn lockable() -> (tempfile::TempDir, App) {
        let (dir, mut app) = unlocked(|db| {
            for title in ["Alpha-entry", "Bravo-entry"] {
                db.root_mut().add_entry().edit(|e| {
                    e.set_unprotected(fields::TITLE, title);
                    e.set_protected(fields::PASSWORD, "secret");
                });
            }
        });
        app.lock_after = Some(Duration::from_secs(60));
        (dir, app)
    }

    fn go_idle(app: &mut App) {
        app.last_input = Instant::now().checked_sub(Duration::from_secs(61)).unwrap();
        app.on_tick();
    }

    fn enter_password(app: &mut App, password: &str) {
        let Screen::Unlock(st) = &mut app.screen else {
            panic!("expected the unlock screen");
        };
        st.password.set_text(password);
        app.on_key(key(KeyCode::Enter));
        finish_unlock(app);
    }

    fn screen_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        buffer.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn idle_session_locks_and_resumes_in_place() {
        let (_dir, mut app) = lockable();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('j')));
        let selected = app.sel_entry;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        assert!(screen_text(&terminal).contains("Bravo-entry"));

        go_idle(&mut app);
        assert!(matches!(&app.screen, Screen::Unlock(st) if st.locked.is_some()));
        assert!(
            app.vault.is_none(),
            "nothing unsaved, so the vault is dropped"
        );
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("inactivity") && !text.contains("Bravo-entry"));

        enter_password(&mut app, "wrong");
        assert!(app.vault.is_none());
        enter_password(&mut app, "pw");
        assert!(matches!(app.screen, Screen::Browser));
        assert_eq!(app.sel_entry, selected);
        assert!(app.pane == Pane::Entries);
    }

    #[test]
    fn locking_keeps_unsaved_work_behind_the_password() {
        let (_dir, mut app) = lockable();
        app.on_key(key(KeyCode::Char('A')));
        for c in "Draft".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(ctrl('l'));
        assert!(matches!(app.screen, Screen::Unlock(_)));
        assert!(app.vault.is_some() && app.locked_with_unsaved_work());

        // Switching databases from the lock screen would lose the draft.
        app.on_key(ctrl('o'));
        assert!(matches!(&app.screen, Screen::Unlock(st) if st.error.is_some()));

        enter_password(&mut app, "wrong");
        assert!(app.locked_with_unsaved_work());
        enter_password(&mut app, "pw");
        assert!(matches!(&app.screen, Screen::GroupEdit(f) if f.name.text.as_str() == "Draft"));
        assert!(!app.locked_with_unsaved_work());
    }

    #[test]
    fn quitting_while_locked_with_unsaved_work_asks_first() {
        let (_dir, mut app) = lockable();
        app.dirty = true;
        go_idle(&mut app);
        assert!(app.locked_with_unsaved_work());
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Some(Overlay::Confirm(_))));
        assert!(!app.should_quit);
        app.on_key(key(KeyCode::Char('d')));
        assert!(app.should_quit);
    }

    #[test]
    fn pastes_never_run_as_commands() {
        let (_dir, mut app) = lockable();
        // Delete-and-confirm, save and quit, if these were keystrokes.
        app.on_paste("dy\x13q");
        assert!(app.overlay.is_none() && !app.dirty && !app.should_quit);
        assert!(matches!(app.screen, Screen::Browser));

        // While searching, a paste is search text; Ctrl-C (0x03) inside it
        // must not end the search.
        app.on_key(key(KeyCode::Char('/')));
        app.on_paste("Bra\x03vo");
        assert_eq!(app.search.as_deref(), Some("Bravo"));
        assert!(app.search_input);
    }

    #[test]
    fn pastes_go_into_the_focused_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.kdbx");
        std::fs::write(&path, b"").unwrap();
        let mut app = App::new(Some(path), None, None);
        // A trailing newline must not submit the unlock form.
        app.on_paste("hunter2\n");
        let Screen::Unlock(st) = &app.screen else {
            panic!("expected the unlock screen");
        };
        assert_eq!(st.password.text.as_str(), "hunter2");
        assert!(!st.working);

        let (_dir, mut app) = lockable();
        app.on_key(key(KeyCode::Char('a')));
        app.on_paste("Title\twith\ttabs");
        let Screen::EntryEdit(form) = &mut app.screen else {
            panic!("expected the entry editor");
        };
        form.focus = F_NOTES;
        app.on_paste("one\r\ntwo\x1b[31m");
        let Screen::EntryEdit(form) = &app.screen else {
            unreachable!()
        };
        assert_eq!(form.fields[F_TITLE].text.as_str(), "Titlewithtabs");
        assert_eq!(form.fields[F_NOTES].text.as_str(), "one\ntwo[31m");
        assert!(form.modified);
    }

    #[test]
    fn multiline_paste_normalizes_line_breaks() {
        let input = "a\r\nb\rc\nd\x07\t";
        let text = multiline(input);
        assert_eq!(text.as_str(), "a\nb\nc\nd");
        assert_eq!(text.capacity(), input.len(), "sized once, never grown");
        assert_eq!(multiline("\r\n\r\n").as_str(), "\n\n");
    }

    #[test]
    fn asks_before_overwriting_external_changes() {
        let (_dir, mut app) = lockable();
        let path = app.vault.as_ref().unwrap().path.clone();
        std::fs::write(&path, b"written by another program").unwrap();

        app.on_key(ctrl('s'));
        assert!(matches!(
            app.overlay,
            Some(Overlay::Confirm(ConfirmState {
                pending: PendingAction::OverwriteExternal { then_quit: false },
                ..
            }))
        ));
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.overlay.is_none());
        assert!(matches!(&app.status, Some((msg, StatusKind::Info, _)) if msg.contains("saved")));
        let bak = path.with_file_name("test.kdbx.bak");
        assert_eq!(std::fs::read(bak).unwrap(), b"written by another program");
    }

    #[test]
    fn editor_errors_stay_visible_on_a_24_row_terminal() {
        let (_dir, mut app) = unlocked(|_| {});
        app.on_key(key(KeyCode::Char('a')));
        let Screen::EntryEdit(form) = &mut app.screen else {
            panic!("expected the entry editor");
        };
        form.fields[F_OTP].set_text("otpauth://totp/x?secret=JBSWY3DPEHPK3PXP&period=0");
        app.on_key(ctrl('s'));
        assert!(matches!(app.screen, Screen::EntryEdit(_)), "save refused");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        assert!(screen_text(&terminal).contains("invalid OTP period"));
    }

    #[test]
    fn editor_masks_the_totp_secret() {
        let (_dir, mut app) = unlocked(|db| {
            db.root_mut().add_entry().edit(|e| {
                e.set_unprotected(fields::TITLE, "Site");
                e.set_protected(fields::OTP, "otpauth://totp/x?secret=JBSWY3DPEHPK3PXP");
            });
        });
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('e')));
        assert!(matches!(app.screen, Screen::EntryEdit(_)));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        assert!(!screen_text(&terminal).contains("JBSWY3DP"));

        app.on_key(ctrl('r'));
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        assert!(screen_text(&terminal).contains("JBSWY3DP"));
    }

    #[test]
    fn revealed_password_hides_itself() {
        let (_dir, mut app) = lockable();
        app.on_key(key(KeyCode::Char('r')));
        assert!(app.reveal);
        app.revealed_at = Instant::now().checked_sub(REVEAL_TTL).unwrap();
        app.on_tick();
        assert!(!app.reveal);
    }

    #[test]
    fn saving_an_entry_leaves_an_untouched_otp_field_alone() {
        // KeeOTP format (KeePass + KeeOtp; KeePassXC reads it) and a lowercase
        // base32 secret (KeePassXC accepts it).
        let mut failures = Vec::new();
        for stored in [
            "key=JBSWY3DPEHPK3PXP&size=8&step=60",
            "otpauth://totp/x?secret=jbswy3dpehpk3pxp",
        ] {
            let (_dir, mut app) = unlocked(|db| {
                db.root_mut().add_entry().edit(|e| {
                    e.set_unprotected(fields::TITLE, "Site");
                    e.set_protected(fields::OTP, stored);
                });
            });
            app.on_key(key(KeyCode::Tab));
            app.on_key(key(KeyCode::Char('e')));
            let Screen::EntryEdit(form) = &mut app.screen else {
                panic!("expected the entry editor");
            };
            // Change only the password.
            form.focus = F_PASS;
            app.on_key(key(KeyCode::Char('x')));
            app.on_key(ctrl('s'));
            if !matches!(app.screen, Screen::Browser) {
                failures.push(format!("{stored}: saving the entry was refused"));
                continue;
            }
            let v = app.vault.as_ref().unwrap();
            let e = v.db.entry(app.sel_entry.unwrap()).unwrap();
            if e.get_raw_otp_value() != Some(stored) {
                failures.push(format!(
                    "{stored}: rewritten to {:?}",
                    e.get_raw_otp_value()
                ));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn permanent_deletes_are_recorded_as_deleted_objects() {
        // Without these records, syncing with an older copy of the vault
        // (KeePass, Keepass2Android, keepass-rs merge) brings them back.
        let (_dir, mut app) = unlocked(|db| {
            db.root_mut()
                .add_entry()
                .edit(|e| e.set_unprotected(fields::TITLE, "OldBank"));
            let mut root = db.root_mut();
            let mut work = root.add_group();
            work.name = "Work".into();
            work.add_entry()
                .edit(|e| e.set_unprotected(fields::TITLE, "Inside"));
        });
        let ids = {
            let db = &app.vault.as_ref().unwrap().db;
            assert!(db.recycle_bin().is_none(), "deletes must be permanent here");
            let root = db.root();
            let work = root.group_by_name("Work").unwrap();
            [
                root.entry_by_name("OldBank").unwrap().id().uuid(),
                work.id().uuid(),
                work.entry_by_name("Inside").unwrap().id().uuid(),
            ]
        };

        // Delete OldBank from the entries pane, then the Work group.
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));
        let deleted = |app: &App| {
            let db = &app.vault.as_ref().unwrap().db;
            ids.map(|id| db.deleted_objects.contains_key(&id))
        };
        assert_eq!(deleted(&app), [true; 3]);

        // And they survive a save.
        app.on_key(ctrl('s'));
        let path = app.vault.as_ref().unwrap().path.clone();
        app.vault = Some(Vault::open(&path, "pw", None).unwrap());
        assert_eq!(deleted(&app), [true; 3]);
    }

    /// Change the selected entry's password through the editor.
    fn change_password(app: &mut App, password: &str) {
        app.open_entry_editor(app.sel_entry);
        let Screen::EntryEdit(form) = &mut app.screen else {
            panic!("expected the entry editor");
        };
        form.fields[F_PASS].set_text(password);
        form.modified = true;
        app.on_key(ctrl('s'));
        assert!(matches!(app.screen, Screen::Browser), "saving failed");
    }

    fn history_passwords(app: &App) -> Vec<String> {
        let v = app.vault.as_ref().unwrap();
        let e = v.db.entry(app.sel_entry.unwrap()).unwrap();
        let history = e.history.as_ref().map(|h| h.get_entries().as_slice());
        history
            .unwrap_or_default()
            .iter()
            .map(|h| h.get_password().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn edits_keep_history_within_the_vaults_limits() {
        let vault_with = |max_items: Option<isize>, max_size: Option<isize>| {
            unlocked(move |db| {
                db.meta.history_max_items = max_items;
                db.meta.history_max_size = max_size;
                db.root_mut().add_entry().edit(|e| {
                    e.set_unprotected(fields::TITLE, "Site");
                    e.set_protected(fields::PASSWORD, "old-secret");
                });
            })
        };

        let (_dir, mut app) = vault_with(Some(2), Some(-1));
        for password in ["new-1", "new-2", "new-3"] {
            change_password(&mut app, password);
        }
        assert_eq!(history_passwords(&app), ["new-2", "new-1"], "newest first");

        // No history at all, by count or by size.
        for (items, size) in [(Some(0), Some(-1)), (Some(-1), Some(0))] {
            let (_dir, mut app) = vault_with(items, size);
            change_password(&mut app, "new-1");
            assert!(history_passwords(&app).is_empty(), "{items:?} {size:?}");
        }

        // -1 means unlimited.
        let (_dir, mut app) = vault_with(Some(-1), Some(-1));
        for password in ["new-1", "new-2", "new-3"] {
            change_password(&mut app, password);
        }
        assert_eq!(history_passwords(&app).len(), 3);
    }

    #[test]
    fn moves_and_renames_update_their_timestamps() {
        // Merges (KeePass, KeePassXC, keepass-rs) decide which side of a
        // change wins by these timestamps.
        let old = chrono::NaiveDate::from_ymd_opt(2020, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let (_dir, mut app) = unlocked(move |db| {
            let bin = {
                let mut root = db.root_mut();
                let mut bin = root.add_group();
                bin.name = "Recycle Bin".into();
                bin.id()
            };
            db.meta.recyclebin_enabled = Some(true);
            db.meta.recyclebin_uuid = Some(bin.uuid());
            db.root_mut().add_entry().edit(|e| {
                e.set_unprotected(fields::TITLE, "E1");
                e.times.location_changed = Some(old);
            });
            for name in ["G3", "G4"] {
                let mut root = db.root_mut();
                let mut group = root.add_group();
                group.name = name.into();
                group.times.location_changed = Some(old);
                group.times.last_modification = Some(old);
            }
        });
        let (e1, g3, g4) = {
            let root = app.vault.as_ref().unwrap().db.root();
            (
                root.entry_by_name("E1").unwrap().id(),
                root.group_by_name("G3").unwrap().id(),
                root.group_by_name("G4").unwrap().id(),
            )
        };

        // E1 to the recycle bin; rename G3; G4 to the recycle bin.
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));
        app.on_key(key(KeyCode::Tab));
        app.select_group(g3);
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(ctrl('u'));
        for c in "G3b".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        app.select_group(g4);
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));

        let db = &app.vault.as_ref().unwrap().db;
        let bin = db.recycle_bin().unwrap().id();
        assert_eq!(db.entry(e1).unwrap().parent().id(), bin, "E1 moved");
        assert_eq!(db.group(g3).unwrap().name, "G3b", "G3 renamed");
        let mut stale = Vec::new();
        if db.entry(e1).unwrap().times.location_changed == Some(old) {
            stale.push("E1 location_changed");
        }
        if db.group(g3).unwrap().times.last_modification == Some(old) {
            stale.push("G3 last_modification");
        }
        if db.group(g4).unwrap().times.location_changed == Some(old) {
            stale.push("G4 location_changed");
        }
        assert!(stale.is_empty(), "not updated: {stale:?}");
    }

    #[test]
    fn detail_pane_hints_only_keys_that_act_on_the_entry() {
        let (_dir, mut app) = unlocked(|db| {
            let mut root = db.root_mut();
            let mut work = root.add_group();
            work.name = "Work".into();
            work.add_entry()
                .edit(|e| e.set_unprotected(fields::TITLE, "GitHub"));
        });
        // Browsing groups: Work selected, its entry GitHub in the detail pane.
        let work = app
            .vault
            .as_ref()
            .unwrap()
            .db
            .root()
            .group_by_name("Work")
            .unwrap()
            .id();
        app.select_group(work);
        assert!(app.pane == Pane::Groups);
        let draw = |app: &App| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
            terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
            screen_text(&terminal)
        };
        let text = draw(&app);
        assert!(text.contains("GitHub"));
        // Here e and d act on the group, so the entry's buttons mustn't say so.
        assert!(
            !text.contains("[d delete]") && !text.contains("[e edit]"),
            "entry buttons advertise keys that act on the group"
        );
        app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(&app.overlay, Some(Overlay::Confirm(cs)) if cs.prompt.contains("group")));
        app.on_key(key(KeyCode::Char('n')));

        // In the entries pane the keys do act on the entry.
        app.on_key(key(KeyCode::Tab));
        let text = draw(&app);
        assert!(text.contains("[d delete]") && text.contains("[e edit]"));
    }

    #[test]
    fn a_new_entry_starts_with_its_password_hidden() {
        let (_dir, mut app) = lockable();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('r')));
        assert!(app.reveal, "Alpha-entry's password revealed");

        app.on_key(key(KeyCode::Char('a')));
        let Screen::EntryEdit(form) = &mut app.screen else {
            panic!("expected the entry editor");
        };
        form.fields[F_TITLE].set_text("Charlie");
        form.fields[F_PASS].set_text("charlie-secret");
        form.modified = true;
        app.on_key(ctrl('s'));
        let v = app.vault.as_ref().unwrap();
        let selected = v.db.entry(app.sel_entry.unwrap()).unwrap();
        assert_eq!(selected.get_title(), Some("Charlie"));
        assert!(!app.reveal, "the new entry's password is shown in clear");
    }

    #[test]
    fn saving_an_unchanged_entry_records_nothing() {
        let (_dir, mut app) = lockable();
        app.on_key(key(KeyCode::Tab));
        let times = |app: &App| {
            let v = app.vault.as_ref().unwrap();
            v.db.entry(app.sel_entry.unwrap())
                .unwrap()
                .times
                .last_modification
        };
        let before = times(&app);
        for _ in 0..3 {
            app.on_key(key(KeyCode::Char('e')));
            app.on_key(ctrl('s'));
            assert!(matches!(app.screen, Screen::Browser), "editor closed");
        }
        assert!(
            history_passwords(&app).is_empty(),
            "no-op saves added history"
        );
        assert!(!app.dirty, "no-op saves marked the vault unsaved");
        assert_eq!(times(&app), before);
    }

    #[test]
    fn editing_keeps_field_protection() {
        let (_dir, mut app) = unlocked(|db| {
            // Protect user names and notes, as a vault can be set up to.
            db.meta.memory_protection = Some(keepass::db::MemoryProtection {
                protect_username: true,
                protect_notes: true,
                ..Default::default()
            });
            db.root_mut().add_entry().edit(|e| {
                e.set_unprotected(fields::TITLE, "Site");
                e.set_protected(fields::USERNAME, "alice");
                // Protected here although the vault doesn't ask for it.
                e.set_protected(fields::URL, "https://example.com");
                e.set_protected(fields::NOTES, "recovery codes");
                e.set_protected(fields::PASSWORD, "pw1");
            });
        });
        let protection = |app: &App| {
            let v = app.vault.as_ref().unwrap();
            let e = v.db.entry(app.sel_entry.unwrap()).unwrap();
            [fields::TITLE, fields::USERNAME, fields::URL, fields::NOTES]
                .map(|key| e.fields.get(key).is_some_and(|v| v.is_protected()))
        };

        // Changing only the password keeps the other fields protected.
        change_password(&mut app, "pw2");
        assert_eq!(protection(&app), [false, true, true, true]);

        // A new entry follows the vault's settings.
        app.on_key(key(KeyCode::Char('a')));
        let Screen::EntryEdit(form) = &mut app.screen else {
            panic!("expected the entry editor");
        };
        for (i, text) in [
            (F_TITLE, "New"),
            (F_USER, "bob"),
            (F_URL, "x"),
            (F_NOTES, "n"),
        ] {
            form.fields[i].set_text(text);
        }
        form.modified = true;
        app.on_key(ctrl('s'));
        assert_eq!(protection(&app), [false, true, false, true]);
    }

    #[test]
    fn editor_survives_narrow_and_short_terminals() {
        let (_dir, mut app) = unlocked(|_| {});
        app.on_key(key(KeyCode::Char('a')));
        let Screen::EntryEdit(form) = &mut app.screen else {
            panic!("expected the entry editor");
        };
        form.focus = F_NOTES;
        form.fields[F_NOTES].set_text("some notes\nsecond line");
        for width in 1..=40 {
            for height in [1, 5, 12, 20, 30] {
                let backend = ratatui::backend::TestBackend::new(width, height);
                let mut terminal = ratatui::Terminal::new(backend).unwrap();
                terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
            }
        }
    }

    fn draw_at(app: &App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
        screen_text(&terminal)
    }

    #[test]
    fn a_too_small_terminal_shows_why_and_ignores_keys() {
        let (_dir, path) = vault_file(|_| {});
        let mut app = App::new(Some(path), None, None);
        // 80x12 is too short for the unlock form: it isn't drawn...
        let text = draw_at(&app, 80, 12);
        assert!(!text.contains("Password"));
        // ...so typing must not go into a password field nobody can see.
        app.on_key(key(KeyCode::Char('x')));
        let Screen::Unlock(st) = &app.screen else {
            panic!("expected the unlock screen");
        };
        assert!(st.password.text.is_empty(), "typing reached a hidden field");
        assert!(text.contains("too small"), "the screen should say why");

        // Big enough again: back to normal.
        assert!(draw_at(&app, 80, 24).contains("Password"));
        app.on_key(key(KeyCode::Char('x')));
        let Screen::Unlock(st) = &app.screen else {
            panic!("expected the unlock screen");
        };
        assert_eq!(st.password.text.as_str(), "x");
    }

    #[test]
    fn a_hidden_confirmation_cannot_delete() {
        let (_dir, mut app) = lockable();
        app.on_key(key(KeyCode::Tab));
        let entries = |app: &App| app.vault.as_ref().unwrap().db.num_entries();
        let before = entries(&app);
        // In 5 rows the confirm dialog has no room for its question.
        draw_at(&app, 80, 5);
        app.on_key(key(KeyCode::Char('d')));
        draw_at(&app, 80, 5);
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(entries(&app), before, "deleted behind an empty dialog");
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
    fn text_field_never_reallocates_in_place() {
        let mut f = TextField::default();
        let buffer = f.text.as_ptr();
        for _ in 0..FIELD_CAPACITY {
            f.insert('x');
        }
        assert_eq!(f.text.as_ptr(), buffer, "filled without reallocating");
        // Growing moves to a new, bigger buffer (wiping the old one).
        f.insert_str("yz");
        assert_eq!(f.text.len(), FIELD_CAPACITY + 2);
        assert!(f.text.capacity() >= 2 * FIELD_CAPACITY);
        assert!(f.text.ends_with("xyz"));
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
