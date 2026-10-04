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
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use clap::Parser;
use ratatui::crossterm::event::{
    self as cevent, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;

const TICK: Duration = Duration::from_millis(250);

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
}

fn main() -> Result<()> {
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
    let mut app = app::App::new(args.database, args.keyfile);
    let mut terminal = ratatui::init();
    if mouse {
        // ratatui's panic hook restores the terminal but knows nothing about
        // mouse capture; release it first so a crash leaves a usable shell.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(stdout(), DisableMouseCapture);
            hook(info);
        }));
        execute!(stdout(), EnableMouseCapture)?;
    }
    let res = run(&mut terminal, &mut app);
    if mouse {
        let _ = execute!(stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    res
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut app::App) -> Result<()> {
    let mut last_tick = Instant::now();
    let mut redraw = true;
    while !app.should_quit {
        if redraw {
            terminal.draw(|f| ui::draw(f, app))?;
        }
        let timeout = TICK.saturating_sub(last_tick.elapsed());
        redraw = false;
        if cevent::poll(timeout)? {
            match cevent::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    app.on_key(key);
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
        if last_tick.elapsed() >= TICK {
            app.on_tick();
            last_tick = Instant::now();
            redraw = true;
        }
    }
    Ok(())
}
