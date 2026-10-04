mod app;
mod clipboard;
mod db;
mod event;
mod generator;
mod open;
mod picker;
mod totp;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Parser;
use ratatui::crossterm::event::{
    self as cevent, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
    EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use zeroize::Zeroizing;

use crate::clipboard::Clipboard;

const TICK: Duration = Duration::from_millis(250);
/// Tick faster while an unlock runs in the background, to pick it up soon.
const BUSY_TICK: Duration = Duration::from_millis(25);
/// How long a termination signal waits for the main loop to exit by itself.
const SIGNAL_GRACE: Duration = Duration::from_millis(500);
/// How long it then waits for a save in progress, which may run a slow key
/// derivation twice.
const SAVE_GRACE: Duration = Duration::from_secs(120);

#[derive(Parser)]
#[command(version, about = "A KeePass-compatible TUI password manager")]
struct Args {
    /// Path to the .kdbx database file (a new one is offered if it doesn't
    /// exist; without it, a file browser opens)
    database: Option<PathBuf>,

    /// Path to an optional key file
    #[arg(short, long)]
    keyfile: Option<PathBuf>,

    /// Disable mouse support (keeps the terminal's own text selection)
    #[arg(long)]
    no_mouse: bool,

    /// Lock after this many minutes without input (0 = never)
    #[arg(long, value_name = "MINUTES", default_value_t = 5)]
    lock_after: u64,
}

fn main() -> Result<()> {
    harden_process();
    let args = Args::parse();

    // A missing database is fine: the app offers to create it.
    if let Some(db) = &args.database
        && db.exists()
        && !db.is_file()
    {
        bail!("not a database file: {}", db.display());
    }
    if let Some(kf) = &args.keyfile
        && !kf.is_file()
    {
        bail!("key file not found: {}", kf.display());
    }

    let mouse = !args.no_mouse;
    let lock_after =
        (args.lock_after > 0).then(|| Duration::from_secs(args.lock_after.saturating_mul(60)));
    let mut app = app::App::new(args.database, args.keyfile, lock_after);
    let clipboard = app.clipboard.clone();
    let quit = Arc::new(AtomicBool::new(false));
    watch_signals(clipboard.clone(), Arc::clone(&quit), mouse)?;

    // The panic hook clears the clipboard and restores the terminal (mouse
    // capture and bracketed paste included), so a crash leaves neither a
    // secret nor an unusable shell behind. It replaces ratatui's hook, whose
    // restore() reports failure with eprintln!: once the terminal is gone
    // (SIGHUP) that panics inside the hook and aborts the process.
    let default_hook = std::panic::take_hook();
    let mut terminal = ratatui::init();
    let on_panic = clipboard.clone();
    std::panic::set_hook(Box::new(move |info| {
        on_panic.clear_now();
        release_terminal(mouse);
        let _ = ratatui::try_restore();
        default_hook(info);
    }));
    // Bracketed paste delivers a paste as one event instead of keystrokes,
    // so pasted text can't run as commands.
    execute!(stdout(), EnableBracketedPaste)?;
    if mouse {
        execute!(stdout(), EnableMouseCapture)?;
    }
    let res = run(&mut terminal, &mut app, &quit);
    app.finish_pending_save();
    clipboard.clear_now();
    release_terminal(mouse);
    // Not `restore()`: after SIGHUP the terminal is gone, and its error
    // report would panic writing to it.
    let _ = ratatui::try_restore();
    // Dropping the Terminal re-shows a hidden cursor and reports failure the
    // same way; when that fails, skip the drop.
    if terminal.show_cursor().is_err() {
        std::mem::forget(terminal);
    }
    res
}

/// Keep secrets out of crash dumps and away from other processes: while a
/// vault is open, protected fields and the master password sit in memory as
/// plain text.
fn harden_process() {
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{
            DumpableBehavior, Resource, Rlimit, getrlimit, set_dumpable_behavior, setrlimit,
        };
        // No core file. The kernel ignores RLIMIT_CORE when core_pattern
        // pipes to a handler, but systemd-coredump honors a zero soft limit.
        // Children inherit it with the hard limit intact, so they can raise
        // it again.
        let maximum = getrlimit(Resource::Core).maximum;
        let _ = setrlimit(
            Resource::Core,
            Rlimit {
                current: Some(0),
                maximum,
            },
        );
        // Not dumpable: other processes of the same user can't ptrace keetui
        // or read its /proc/<pid>/mem, and a dump that happens anyway is
        // readable by root only.
        let _ = set_dumpable_behavior(DumpableBehavior::NotDumpable);
    }
}

/// Undo the terminal modes keetui enables on top of ratatui's own setup.
fn release_terminal(mouse: bool) {
    let _ = execute!(stdout(), DisableBracketedPaste);
    if mouse {
        let _ = execute!(stdout(), DisableMouseCapture);
    }
}

/// SIGHUP (terminal closed), SIGTERM and SIGINT would kill keetui before it
/// clears the clipboard. Ask the main loop to stop instead; if it hasn't
/// within a moment, it is busy: let a save in progress finish (cutting it
/// short would lose it), then clean up from here.
fn watch_signals(clipboard: Clipboard, quit: Arc<AtomicBool>, mouse: bool) -> Result<()> {
    let mut signals = Signals::new([SIGHUP, SIGINT, SIGTERM])?;
    std::thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            quit.store(true, Ordering::Relaxed);
            std::thread::sleep(SIGNAL_GRACE);
            let deadline = Instant::now() + SAVE_GRACE;
            while db::saving() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            clipboard.clear_now();
            release_terminal(mouse);
            let _ = ratatui::try_restore();
            std::process::exit(128 + sig);
        }
    });
    Ok(())
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut app::App,
    quit: &AtomicBool,
) -> Result<()> {
    let mut last_tick = Instant::now();
    let mut redraw = true;
    while !app.should_quit && !quit.load(Ordering::Relaxed) {
        if redraw {
            terminal.draw(|f| ui::draw(f, app))?;
            // Work the last input scheduled (unlocking, creating a database)
            // starts now that the frame announcing it is on screen.
            if app.run_pending() {
                continue;
            }
        }
        let tick = if app.busy() { BUSY_TICK } else { TICK };
        let timeout = tick.saturating_sub(last_tick.elapsed());
        redraw = false;
        if cevent::poll(timeout)? {
            match cevent::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    app.on_key(key);
                    redraw = true;
                }
                Event::Paste(text) => {
                    // Possibly a password; wipe it once handled.
                    let text = Zeroizing::new(text);
                    app.on_paste(&text);
                    redraw = true;
                }
                Event::Mouse(m) => {
                    app.on_mouse(m);
                    // Motion only changes hover highlights; redrawing is
                    // cheap since ratatui diffs the buffer.
                    redraw = true;
                }
                Event::Resize(..) | Event::FocusGained => redraw = true,
                _ => {}
            }
        }
        // Tick on wall-clock time rather than on idle timeouts, so constant
        // mouse motion can't starve status expiry, unlock, or TOTP refresh.
        if last_tick.elapsed() >= tick {
            app.on_tick();
            last_tick = Instant::now();
            redraw = true;
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use rustix::process::{DumpableBehavior, Resource, dumpable_behavior, getrlimit};

    #[test]
    fn hardening_disables_core_dumps() {
        super::harden_process();
        assert_eq!(getrlimit(Resource::Core).current, Some(0));
        assert!(matches!(
            dumpable_behavior(),
            Ok(DumpableBehavior::NotDumpable)
        ));
    }
}
