//! Wayland clipboard via wl-copy, with auto-clear after a timeout.
//!
//! The secret is written to wl-copy's stdin (never argv, which would be
//! visible in /proc). Each copy bumps a token; the clear thread only clears
//! if its token is still current, so a newer keetui copy is never clobbered.
//! A copy made by another application during the TTL will still be cleared —
//! an accepted tradeoff, documented in the README.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub const DEFAULT_TTL: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Inner {
    token: u64,
    clear_at: Option<Instant>,
    label: String,
}

#[derive(Clone, Default)]
pub struct Clipboard {
    inner: Arc<Mutex<Inner>>,
}

impl Clipboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn copy(&self, secret: &str, label: &str, ttl: Duration) -> Result<()> {
        let mut child = Command::new("wl-copy")
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
            let mut inner = self.inner.lock().unwrap();
            inner.token += 1;
            inner.clear_at = Some(Instant::now() + ttl);
            inner.label = label.to_string();
            my_token = inner.token;
        }

        let shared = Arc::clone(&self.inner);
        std::thread::spawn(move || {
            std::thread::sleep(ttl);
            let mut inner = shared.lock().unwrap();
            if inner.token == my_token {
                let _ = Command::new("wl-copy")
                    .arg("--clear")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                inner.clear_at = None;
            }
        });

        Ok(())
    }

    /// (label, seconds remaining) while a copied secret is pending auto-clear.
    pub fn countdown(&self) -> Option<(String, u64)> {
        let inner = self.inner.lock().unwrap();
        let clear_at = inner.clear_at?;
        let left = clear_at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            None
        } else {
            Some((inner.label.clone(), left.as_secs() + 1))
        }
    }
}
