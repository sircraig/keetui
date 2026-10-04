//! Open entry URLs in the desktop's default handler via xdg-open.
//!
//! Only web-ish schemes are opened. KeePass `cmd://` URLs (which run a
//! command) and `file://` URLs are refused rather than handed to xdg-open.

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

const ALLOWED_SCHEMES: [&str; 5] = ["http", "https", "ftp", "sftp", "mailto"];

/// Turn the stored URL into something safe to open, adding `https://` to
/// bare hostnames like KeePassXC does.
pub fn normalize_url(raw: &str) -> Result<String> {
    let url = raw.trim();
    if url.is_empty() {
        bail!("URL is empty");
    }
    if url.contains('{') {
        bail!("URL contains KeePass placeholders; not supported");
    }
    match url.split_once(':') {
        Some((scheme, _))
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
                && !url[scheme.len()..].starts_with(":/") =>
        {
            // "host:port" without a scheme, e.g. "example.com:8443"
            if url[scheme.len() + 1..]
                .split('/')
                .next()
                .is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            {
                return Ok(format!("https://{url}"));
            }
            check_scheme(scheme)?;
            Ok(url.to_string())
        }
        Some((scheme, _)) if url[scheme.len()..].starts_with("://") => {
            check_scheme(scheme)?;
            Ok(url.to_string())
        }
        _ => Ok(format!("https://{url}")),
    }
}

fn check_scheme(scheme: &str) -> Result<()> {
    let s = scheme.to_ascii_lowercase();
    if ALLOWED_SCHEMES.contains(&s.as_str()) {
        Ok(())
    } else {
        bail!("refusing to open {s}:// URL")
    }
}

pub fn open_url(raw: &str) -> Result<String> {
    let url = normalize_url(raw)?;
    let mut child = Command::new("xdg-open")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run xdg-open")?;
    // Reap in the background so a slow handler never blocks the UI.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls() {
        assert_eq!(normalize_url("https://x.com/a").unwrap(), "https://x.com/a");
        assert_eq!(normalize_url("github.com").unwrap(), "https://github.com");
        assert_eq!(normalize_url(" example.com:8443/x ").unwrap(), "https://example.com:8443/x");
        assert_eq!(normalize_url("mailto:a@b.c").unwrap(), "mailto:a@b.c");
        assert!(normalize_url("cmd://rm -rf ~").is_err());
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("").is_err());
        assert!(normalize_url("{REF:U@I:123}").is_err());
    }
}
