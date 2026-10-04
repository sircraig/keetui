//! The databases opened recently, remembered between sessions.
//!
//! The list lives in `$XDG_STATE_HOME/keetui/recent` (by default
//! `~/.local/state/keetui/recent`): one absolute path per line, newest
//! first. It says where the vaults are, so only the user can read it.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

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
