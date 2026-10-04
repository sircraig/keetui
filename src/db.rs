//! Vault: wrapper around the keepass crate's `Database` handling unlock,
//! atomic save with backup, and search.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow};
use keepass::{
    Database, DatabaseKey,
    config::{DatabaseConfig, DatabaseVersion, KdfConfig},
    db::{DatabaseOpenError, EntryId, GroupId, fields},
};
use zeroize::Zeroizing;

pub struct Vault {
    pub db: Database,
    // Retained for the whole session because `Database::save` requires the key
    // again. DatabaseKey is ZeroizeOnDrop.
    key: DatabaseKey,
    /// The real file, symlinks resolved.
    pub path: PathBuf,
    /// The (encrypted) file contents as last read or written, to notice when
    /// another program changes the file underneath us.
    on_disk: Vec<u8>,
}

/// Saves in progress. A termination signal waits for them to finish
/// rather than cutting a save short.
static SAVES_IN_PROGRESS: AtomicUsize = AtomicUsize::new(0);

pub fn saving() -> bool {
    SAVES_IN_PROGRESS.load(Ordering::SeqCst) > 0
}

/// Counts a save as in progress for as long as it lives.
struct SaveInProgress;

impl SaveInProgress {
    fn start() -> Self {
        SAVES_IN_PROGRESS.fetch_add(1, Ordering::SeqCst);
        SaveInProgress
    }
}

impl Drop for SaveInProgress {
    fn drop(&mut self) {
        SAVES_IN_PROGRESS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Saving would overwrite changes another program (KeePassXC, a sync
/// client, a second keetui) made to the file since keetui read it.
#[derive(Debug)]
pub struct ChangedOnDisk;

impl std::fmt::Display for ChangedOnDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the file was changed by another program since it was opened")
    }
}

impl std::error::Error for ChangedOnDisk {}

impl Vault {
    pub fn open(path: &Path, password: &str, keyfile: Option<&Path>) -> Result<Self> {
        // Work on the real file, so that saving through a symlink updates its
        // target instead of replacing the link with a regular file.
        let path =
            fs::canonicalize(path).with_context(|| format!("cannot read {}", path.display()))?;
        // Reading a FIFO or a device would block forever or never end.
        if !fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            return Err(anyhow!("{} is not a regular file", path.display()));
        }
        let data = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let key = build_key(password, keyfile)?;
        let db = Database::parse(&data, key.clone()).map_err(friendly_open_error)?;

        Ok(Vault {
            db,
            key,
            path,
            on_disk: data,
        })
    }

    /// Create a new, empty database at `path` and write it to disk.
    /// Refuses to overwrite an existing file.
    pub fn create(path: &Path, password: &str, keyfile: Option<&Path>) -> Result<Self> {
        if path.exists() {
            return Err(anyhow!("{} already exists", path.display()));
        }
        let key = build_key(password, keyfile)?;
        let name = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Passwords".into());

        let mut db = Database::new();
        db.config = new_db_config();
        db.meta.database_name = Some(name.clone());
        db.root_mut().name = name;

        let mut vault = Vault {
            db,
            key,
            path: path.to_path_buf(),
            on_disk: Vec::new(),
        };
        vault.save()?;
        Ok(vault)
    }

    /// Does this password and key file make the key the vault was opened
    /// with? Lets a locked session resume without re-reading the file.
    pub fn key_matches(&self, password: &str, keyfile: Option<&Path>) -> Result<bool> {
        Ok(build_key(password, keyfile)? == self.key)
    }

    /// The database was opened from a pre-KDBX4 file and will be converted on save.
    pub fn needs_kdbx4_upgrade(&self) -> bool {
        !matches!(self.db.config.version, DatabaseVersion::KDB4(_))
    }

    /// Serialize, verify, back up the old file, then atomically replace it.
    /// Fails with [`ChangedOnDisk`] if another program changed the file.
    pub fn save(&mut self) -> Result<()> {
        self.write(false)
    }

    /// Save even though another program changed the file since keetui read
    /// it, discarding those changes (the old file still goes to the backup).
    pub fn save_overwriting(&mut self) -> Result<()> {
        self.write(true)
    }

    fn write(&mut self, overwrite: bool) -> Result<()> {
        let _saving = SaveInProgress::start();
        if self.needs_kdbx4_upgrade() {
            // The crate can only write KDBX4; adopt keetui's config for new files.
            self.db.config = new_db_config();
        }

        let mut buf = Vec::new();
        self.db
            .save(&mut buf, self.key.clone())
            .context("failed to serialize database")?;

        // Verify the output reopens before touching the file on disk.
        Database::parse(&buf, self.key.clone()).map_err(|e| {
            anyhow!("verification of saved data failed ({e}); original file untouched")
        })?;

        if self.path.is_file() {
            let old = fs::read(&self.path)
                .with_context(|| format!("cannot read {}", self.path.display()))?;
            // Checked as late as possible, right before replacing the file.
            if !overwrite && old != self.on_disk {
                return Err(ChangedOnDisk.into());
            }
            let bak = backup_path(&self.path);
            write_atomic(&bak, &old)
                .with_context(|| format!("failed to write backup {}", bak.display()))?;
        }
        write_atomic(&self.path, &buf)
            .with_context(|| format!("failed to replace {}", self.path.display()))?;
        self.on_disk = buf;

        Ok(())
    }

    /// Case-insensitive search: every whitespace-separated term must appear in
    /// the title, username, URL, notes, tags or group path. Entries in the
    /// recycle bin are skipped. Results are sorted by title.
    pub fn search(&self, query: &str) -> Vec<EntryId> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let bin = self.db.recycle_bin().map(|g| g.id());
        let mut hits: Vec<(String, EntryId)> = self
            .db
            .iter_all_entries()
            .filter(|e| bin.is_none_or(|bin| !self.group_in(e.parent().id(), bin)))
            .filter(|e| {
                let haystack = [
                    e.get_title(),
                    e.get_username(),
                    e.get_url(),
                    e.get(fields::NOTES),
                    Some(e.tags.join(" ").as_str()),
                    Some(self.group_path(e.parent().id()).as_str()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
                terms.iter().all(|t| haystack.contains(t.as_str()))
            })
            .map(|e| (e.get_title().unwrap_or("").to_lowercase(), e.id()))
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        hits.into_iter().map(|(_, id)| id).collect()
    }

    /// "Work / Dev" style path of a group, excluding the root group.
    pub fn group_path(&self, group: GroupId) -> String {
        let root = self.db.root().id();
        let mut names = Vec::new();
        let mut cur = Some(group);
        while let Some(id) = cur.filter(|&id| id != root) {
            let Some(g) = self.db.group(id) else { break };
            names.push(g.name.clone());
            cur = g.parent().map(|p| p.id());
        }
        names.reverse();
        names.join(" / ")
    }

    /// Is `group` equal to or nested inside `ancestor`?
    pub fn group_in(&self, group: GroupId, ancestor: GroupId) -> bool {
        let mut cur = Some(group);
        while let Some(id) = cur {
            if id == ancestor {
                return true;
            }
            cur = self.db.group(id).and_then(|g| g.parent().map(|p| p.id()));
        }
        false
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

/// Settings for databases keetui writes from scratch: KDBX4 with Argon2d
/// using 64 MiB, in line with KeePassXC. (The keepass crate's default uses
/// only 1 MiB, which gives little protection against GPU cracking.)
pub fn new_db_config() -> DatabaseConfig {
    let mut config = DatabaseConfig::default();
    if let KdfConfig::Argon2 {
        iterations, memory, ..
    } = &mut config.kdf_config
    {
        *memory = 64 * 1024 * 1024;
        *iterations = 10;
    }
    config
}

fn build_key(password: &str, keyfile: Option<&Path>) -> Result<DatabaseKey> {
    let mut key = DatabaseKey::new();
    if !password.is_empty() {
        key = key.with_password(password);
    }
    if let Some(kf) = keyfile {
        let kf_data = Zeroizing::new(
            fs::read(kf).with_context(|| format!("cannot read key file {}", kf.display()))?,
        );
        key = key.with_keyfile(&mut kf_data.as_slice())?;
    }
    if key.is_empty() {
        return Err(anyhow!("a password or key file is required"));
    }
    Ok(key)
}

/// Write `data` to a fresh temp file next to `path` (0600 on unix) and
/// rename it over `path`. Readers see the old or the new file, never a
/// partial one, and a symlink at `path` is replaced rather than written
/// through (`fs::copy` would follow it and clobber the link's target).
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir).context("failed to create temp file")?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.bak"))
}

fn friendly_open_error(e: DatabaseOpenError) -> anyhow::Error {
    match e {
        DatabaseOpenError::Key(_) | DatabaseOpenError::Cryptography(_) => {
            anyhow!("wrong password or key file")
        }
        DatabaseOpenError::Format(inner) => {
            // A wrong key usually surfaces as an HMAC/integrity failure while parsing.
            let msg = inner.to_string();
            if msg.to_lowercase().contains("hmac") || msg.to_lowercase().contains("checksum") {
                anyhow!("wrong password or key file")
            } else {
                anyhow!("invalid database: {msg}")
            }
        }
        other => anyhow!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vault saved with cheap key derivation (password "pw"), opened.
    fn cheap_vault(path: &Path) -> Vault {
        let mut db = Database::new();
        if let KdfConfig::Argon2 {
            iterations,
            memory,
            parallelism,
            ..
        } = &mut db.config.kdf_config
        {
            (*iterations, *memory, *parallelism) = (1, 64 * 1024, 1);
        }
        db.root_mut()
            .add_entry()
            .edit(|e| e.set_protected(fields::PASSWORD, "secret"));
        let mut file = fs::File::create(path).unwrap();
        db.save(&mut file, DatabaseKey::new().with_password("pw"))
            .unwrap();
        Vault::open(path, "pw", None).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn saves_through_a_symlinked_vault() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sync")).unwrap();
        let real = dir.path().join("sync/v.kdbx");
        drop(cheap_vault(&real));
        let link = dir.path().join("v.kdbx");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut vault = Vault::open(&link, "pw", None).unwrap();
        vault.db.root_mut().add_entry();
        vault.save().unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(Vault::open(&real, "pw", None).unwrap().db.num_entries(), 2);
        assert!(dir.path().join("sync/v.kdbx.bak").is_file());
        assert!(!dir.path().join("v.kdbx.bak").exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_open_fifos_and_devices() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.kdbx");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            // Would block forever in fs::read without the check.
            let err = Vault::open(&fifo, "pw", None).err().unwrap();
            assert!(format!("{err:#}").contains("not a regular file"));
        }
        let err = Vault::open(Path::new("/dev/zero"), "pw", None)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("not a regular file"));
    }

    #[test]
    fn asks_before_overwriting_changes_made_by_another_program() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.kdbx");
        let mut ours = cheap_vault(&path);
        let mut theirs = Vault::open(&path, "pw", None).unwrap();
        theirs.db.root_mut().add_entry();
        theirs.save().unwrap();

        ours.db.root_mut().add_entry();
        let err = ours.save().unwrap_err();
        assert!(err.is::<ChangedOnDisk>(), "{err:#}");
        let theirs_on_disk = fs::read(&path).unwrap();

        ours.save_overwriting().unwrap();
        assert_eq!(fs::read(backup_path(&path)).unwrap(), theirs_on_disk);
        // Our own write is now the known state: plain saves work again.
        ours.save().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn backup_replaces_a_symlink_instead_of_writing_through_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.kdbx");
        let mut vault = cheap_vault(&path);
        let before = fs::read(&path).unwrap();

        // Someone who can write to the folder plants v.kdbx.bak -> victim.
        let victim = dir.path().join("victim");
        fs::write(&victim, b"precious").unwrap();
        let bak = dir.path().join("v.kdbx.bak");
        std::os::unix::fs::symlink(&victim, &bak).unwrap();

        vault.save().unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        let meta = fs::symlink_metadata(&bak).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&bak).unwrap(), before);
    }

    #[test]
    fn a_save_counts_as_in_progress() {
        // Other tests save concurrently, so only this side is checkable.
        let save = SaveInProgress::start();
        assert!(saving());
        drop(save);
    }

    #[test]
    fn create_writes_a_reopenable_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Fresh.kdbx");
        let vault = Vault::create(&path, "pw", None).unwrap();
        assert_eq!(vault.db.root().name, "Fresh");
        assert!(
            Vault::create(&path, "pw", None).is_err(),
            "must not overwrite"
        );
        assert!(!dir.path().join("Fresh.kdbx.bak").exists());

        let reopened = Vault::open(&path, "pw", None).unwrap();
        assert_eq!(reopened.db.root().name, "Fresh");
        assert!(matches!(
            reopened.db.config.kdf_config,
            KdfConfig::Argon2 { memory, .. } if memory == 64 * 1024 * 1024
        ));
        assert!(Vault::open(&path, "wrong", None).is_err());
    }

    #[test]
    fn backup_path_appends_bak() {
        assert_eq!(
            backup_path(Path::new("/x/vault.kdbx")),
            PathBuf::from("/x/vault.kdbx.bak")
        );
    }
}
