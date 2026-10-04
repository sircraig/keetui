//! Vault: wrapper around the keepass crate's `Database` handling unlock,
//! atomic save with backup, and search.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use keepass::{
    config::{DatabaseConfig, DatabaseVersion, KdfConfig},
    db::{fields, DatabaseOpenError, EntryId, GroupId},
    Database, DatabaseKey,
};

pub struct Vault {
    pub db: Database,
    // Retained for the whole session because `Database::save` requires the key
    // again. DatabaseKey is ZeroizeOnDrop.
    key: DatabaseKey,
    pub path: PathBuf,
}

impl Vault {
    pub fn open(path: &Path, password: &str, keyfile: Option<&Path>) -> Result<Self> {
        let data = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let key = build_key(password, keyfile)?;
        let db = Database::parse(&data, key.clone()).map_err(friendly_open_error)?;

        Ok(Vault {
            db,
            key,
            path: path.to_path_buf(),
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
        };
        vault.save()?;
        Ok(vault)
    }

    /// The database was opened from a pre-KDBX4 file and will be converted on save.
    pub fn needs_kdbx4_upgrade(&self) -> bool {
        !matches!(self.db.config.version, DatabaseVersion::KDB4(_))
    }

    /// Serialize, verify, back up the old file, then atomically replace it.
    pub fn save(&mut self) -> Result<()> {
        if self.needs_kdbx4_upgrade() {
            // The crate can only write KDBX4; adopt keetui's config for new files.
            self.db.config = new_db_config();
        }

        let mut buf = Vec::new();
        self.db
            .save(&mut buf, self.key.clone())
            .context("failed to serialize database")?;

        // Verify the output reopens before touching the file on disk.
        Database::parse(&buf, self.key.clone())
            .map_err(|e| anyhow!("verification of saved data failed ({e}); original file untouched"))?;

        if self.path.is_file() {
            let bak = backup_path(&self.path);
            fs::copy(&self.path, &bak)
                .with_context(|| format!("failed to write backup {}", bak.display()))?;
        }

        let dir = match self.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        // NamedTempFile is created 0600 on unix.
        let mut tmp = tempfile::NamedTempFile::new_in(dir).context("failed to create temp file")?;
        tmp.write_all(&buf)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path)
            .with_context(|| format!("failed to replace {}", self.path.display()))?;

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
        let kf_data = fs::read(kf).with_context(|| format!("cannot read key file {}", kf.display()))?;
        key = key.with_keyfile(&mut kf_data.as_slice())?;
    }
    if key.is_empty() {
        return Err(anyhow!("a password or key file is required"));
    }
    Ok(key)
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

    #[test]
    fn create_writes_a_reopenable_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Fresh.kdbx");
        let vault = Vault::create(&path, "pw", None).unwrap();
        assert_eq!(vault.db.root().name, "Fresh");
        assert!(Vault::create(&path, "pw", None).is_err(), "must not overwrite");
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
