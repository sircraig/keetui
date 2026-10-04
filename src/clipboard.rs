//! Wayland clipboard via wl-copy, with auto-clear after a timeout.
//!
//! The secret is written to wl-copy's stdin (never argv, which would be
//! visible in /proc). Each copy bumps a token; the clear thread only clears
//! if its token is still current, so a newer keetui copy is never clobbered.
//! wl-copy keeps serving the selection after keetui exits, so a pending
//! clear is also run on exit: see [`Clipboard::clear_now`].
//! A copy made by another application during the TTL will still be cleared —
//! an accepted tradeoff, documented in the README.

use std::ffi::OsString;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub const DEFAULT_TTL: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Inner {
    token: u64,
    clear_at: Option<Instant>,
    label: String,
}

#[derive(Clone)]
pub struct Clipboard {
    inner: Arc<Mutex<Inner>>,
    /// The wl-copy command line; tests substitute a stand-in script.
    program: Arc<[OsString]>,
}

impl Clipboard {
    pub fn new() -> Self {
        Self::with_program(["wl-copy"])
    }

    fn with_program<S: Into<OsString>>(program: impl IntoIterator<Item = S>) -> Self {
        Clipboard {
            inner: Arc::default(),
            program: program.into_iter().map(Into::into).collect(),
        }
    }

    pub fn copy(&self, secret: &str, label: &str, ttl: Duration) -> Result<()> {
        let mut child = command(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to run wl-copy (is this a Wayland session?)")?;
        child
            .stdin
            .take()
            .context("wl-copy stdin unavailable")?
            .write_all(secret.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("wl-copy exited with {status}");
        }

        let my_token;
        {
            let mut inner = self.lock();
            inner.token += 1;
            inner.clear_at = Some(Instant::now() + ttl);
            inner.label = label.to_string();
            my_token = inner.token;
        }

        let this = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(ttl);
            let mut inner = this.lock();
            if inner.token == my_token {
                clear(&this.program);
                inner.clear_at = None;
            }
        });

        Ok(())
    }

    /// Clear the clipboard right away if a copied secret is still waiting
    /// for its timer. The timer thread dies with the process while wl-copy
    /// lives on, so this runs when keetui quits, crashes or is signalled.
    pub fn clear_now(&self) {
        let mut inner = self.lock();
        if inner.clear_at.take().is_some() {
            // Retire the pending timer thread.
            inner.token += 1;
            clear(&self.program);
        }
    }

    /// (label, seconds remaining) while a copied secret is pending auto-clear.
    pub fn countdown(&self) -> Option<(String, u64)> {
        let inner = self.lock();
        let clear_at = inner.clear_at?;
        let left = clear_at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            None
        } else {
            Some((inner.label.clone(), left.as_secs() + 1))
        }
    }

    /// Lock the shared state, ignoring poisoning: this also runs from the
    /// panic hook, where giving up would leave the secret in the clipboard.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn command(program: &[OsString]) -> Command {
    let mut cmd = Command::new(&program[0]);
    cmd.args(&program[1..]);
    cmd
}

fn clear(program: &[OsString]) {
    let _ = command(program)
        .arg("--clear")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A clipboard whose "wl-copy" is a shell script logging its arguments,
    /// one invocation per line. Run via `sh` rather than exec'd directly, so
    /// a concurrently forking test can't make the fresh script ETXTBSY.
    fn fake() -> (tempfile::TempDir, PathBuf, Clipboard) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let script = dir.path().join("wl-copy");
        fs::write(
            &script,
            format!("cat >/dev/null\necho \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        let clip = Clipboard::with_program([PathBuf::from("sh"), script]);
        (dir, log, clip)
    }

    fn calls(log: &PathBuf) -> Vec<String> {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Wait up to two seconds for the log to reach `n` calls.
    fn wait_for_calls(log: &PathBuf, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while calls(log).len() < n && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        calls(log)
    }

    #[test]
    fn clear_now_clears_a_pending_copy() {
        let (_dir, log, clip) = fake();
        clip.copy("hunter2", "password", Duration::from_secs(60))
            .unwrap();
        assert!(clip.countdown().is_some());

        clip.clear_now();
        assert_eq!(calls(&log), ["", "--clear"]);
        assert!(clip.countdown().is_none());

        // Nothing pending any more: no second clear.
        clip.clear_now();
        assert_eq!(calls(&log).len(), 2);
    }

    #[test]
    fn clear_now_without_a_copy_does_nothing() {
        let (_dir, log, clip) = fake();
        clip.clear_now();
        assert!(calls(&log).is_empty());
    }

    #[test]
    fn timer_clears_once_and_newer_copies_win() {
        let (_dir, log, clip) = fake();
        // Generous TTL so the second copy lands before the first timer fires.
        clip.copy("old", "password", Duration::from_millis(500))
            .unwrap();
        clip.copy("new", "password", Duration::from_secs(60))
            .unwrap();
        // The first timer fires but its token is stale: no clear.
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(calls(&log), ["", ""]);

        let (_dir, log, clip) = fake();
        clip.copy("secret", "password", Duration::from_millis(20))
            .unwrap();
        assert_eq!(wait_for_calls(&log, 2), ["", "--clear"]);
        // Already cleared by the timer, so exiting doesn't clear again.
        clip.clear_now();
        assert_eq!(calls(&log).len(), 2);
    }
}
