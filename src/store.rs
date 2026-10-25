//! File-backed artifact store.
//!
//! Layout under the data dir:
//!
//! ```text
//! artifacts/<name>/
//!   current          # current version content
//!   meta.json        # rev, content_type, timestamps, size, sha256
//!   versions/
//!     2026-10-09T18-22-33-123Z       # previous content
//!     2026-10-09T18-22-33-123Z.json  # its meta at the time of saving
//! images/<name>/     # same structure, image namespace (no rename, no aliases)
//! aliases/<old-name> # renamed away: file content is the new name
//! ```
//!
//! All writes are atomic (tmp file + rename). Artifact names are strictly
//! validated, so they are always a single safe path component.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Names must match `^[a-z0-9][a-z0-9-]{0,63}$`.
pub const NAME_RULE: &str = "artifact name must match ^[a-z0-9][a-z0-9-]{0,63}$";

pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    (1..=64).contains(&name.len())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Meta {
    /// Monotonic mutation counter: 1 on create, +1 on every content-changing
    /// update and rollback. The optimistic-concurrency token a conditional
    /// put is checked against; artifacts written before it existed read as 0,
    /// and their first write gives them a real rev.
    #[serde(default)]
    pub rev: u64,
    pub content_type: String,
    pub created_at: String,
    pub updated_at: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VersionInfo {
    pub version: String,
    pub content_type: String,
    pub size: u64,
    pub sha256: String,
    pub saved_at: String,
}

#[derive(Serialize, Debug)]
pub struct ListItem {
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub updated_at: String,
}

#[derive(Debug, PartialEq)]
pub enum PutOutcome {
    Created,
    Updated,
    Unchanged,
}

/// Why a put failed.
#[derive(Debug)]
pub enum PutError {
    /// A conditional put named a rev that is not the current one; nothing was
    /// written. Carries the current rev.
    RevConflict { current: u64 },
    Io(io::Error),
}

impl From<io::Error> for PutError {
    fn from(e: io::Error) -> Self {
        PutError::Io(e)
    }
}

#[derive(Debug, PartialEq)]
pub enum RenameOutcome {
    Renamed(Meta),
    SourceMissing,
    TargetExists,
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn atomic_write(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

#[derive(Clone)]
pub struct FileStore {
    root: PathBuf,
    /// Namespace directory under the root: `artifacts` or `images`.
    collection: &'static str,
    /// Aliases directory under the root; images have no rename, hence none.
    aliases: &'static str,
    /// Serializes mutations (put / rollback / rename / delete) so a
    /// conditional put's rev check cannot interleave with another mutation
    /// of the same artifact.
    write_lock: Arc<Mutex<()>>,
}

impl FileStore {
    /// The artifact store: `<root>/artifacts/<name>`, renames tracked in
    /// `<root>/aliases`.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let store = FileStore {
            root: root.into(),
            collection: "artifacts",
            aliases: "aliases",
            write_lock: Arc::new(Mutex::new(())),
        };
        fs::create_dir_all(store.collection_root())?;
        fs::create_dir_all(store.aliases_dir())?;
        Ok(store)
    }

    /// The image store shares the data dir but keeps its own namespace,
    /// `<root>/images/<name>`; there is no rename for images, hence no aliases.
    pub fn image_store(root: impl Into<PathBuf>) -> io::Result<Self> {
        let store = FileStore {
            root: root.into(),
            collection: "images",
            aliases: "image-aliases",
            write_lock: Arc::new(Mutex::new(())),
        };
        fs::create_dir_all(store.collection_root())?;
        Ok(store)
    }

    /// Guard serializing mutations. Poisoning is ignored on purpose: one
    /// panicking writer must not wedge every later one.
    fn write_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn collection_root(&self) -> PathBuf {
        self.root.join(self.collection)
    }

    fn aliases_dir(&self) -> PathBuf {
        self.root.join(self.aliases)
    }

    fn alias_path(&self, name: &str) -> PathBuf {
        self.aliases_dir().join(name)
    }

    fn artifact_dir(&self, name: &str) -> PathBuf {
        self.collection_root().join(name)
    }

    fn versions_dir(&self, name: &str) -> PathBuf {
        self.artifact_dir(name).join("versions")
    }

    pub fn exists(&self, name: &str) -> bool {
        self.artifact_dir(name).join("current").is_file()
    }

    fn read_meta(&self, name: &str) -> io::Result<Meta> {
        let raw = fs::read(self.artifact_dir(name).join("meta.json"))?;
        Ok(serde_json::from_slice(&raw)?)
    }

    /// The name an old public URL should redirect to, if any: `name` was
    /// renamed away and no artifact lives under it anymore.
    pub fn resolve_alias(&self, name: &str) -> io::Result<Option<String>> {
        match fs::read_to_string(self.alias_path(name)) {
            Ok(target) => {
                let target = target.trim();
                Ok(valid_name(target).then(|| target.to_string()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn set_alias(&self, from: &str, to: &str) -> io::Result<()> {
        atomic_write(&self.alias_path(from), to.as_bytes())
    }

    fn remove_alias(&self, name: &str) {
        // best-effort: a missing alias file is not an error
        let _ = fs::remove_file(self.alias_path(name));
    }

    /// Create or update an artifact. Updating moves the previous `current`
    /// into `versions/`; identical content+type is a no-op. Creating under a
    /// name that used to redirect drops the redirect. With `expect_rev` the
    /// write applies only when that is the current rev — the check and the
    /// write share the store's write lock, so a guarded put can never
    /// interleave with another mutation. Returns the outcome and the meta now
    /// stored; its `rev` is the token for the next conditional put.
    pub fn put(
        &self,
        name: &str,
        content: &[u8],
        content_type: &str,
        expect_rev: Option<u64>,
    ) -> Result<(PutOutcome, Meta), PutError> {
        let _guard = self.write_lock();
        let dir = self.artifact_dir(name);
        if !self.exists(name) {
            if expect_rev.is_some() {
                // a conditional put targets a revision; of a missing artifact
                // the current rev is 0
                return Err(PutError::RevConflict { current: 0 });
            }
            fs::create_dir_all(&dir)?;
            self.remove_alias(name);
            let meta = Meta {
                rev: 1,
                content_type: content_type.to_string(),
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                size: content.len() as u64,
                sha256: sha256_hex(content),
            };
            atomic_write(&dir.join("current"), content)?;
            atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta).map_err(io::Error::from)?)?;
            return Ok((PutOutcome::Created, meta));
        }

        let old_meta = self.read_meta(name)?;
        if let Some(expected) = expect_rev {
            if old_meta.rev != expected {
                return Err(PutError::RevConflict { current: old_meta.rev });
            }
        }
        if old_meta.content_type == content_type && old_meta.sha256 == sha256_hex(content) {
            return Ok((PutOutcome::Unchanged, old_meta));
        }

        let old_content = fs::read(dir.join("current"))?;
        let ts = version_id();
        let versions = self.versions_dir(name);
        fs::create_dir_all(&versions)?;
        atomic_write(&versions.join(&ts), &old_content)?;
        let version_info = VersionInfo {
            version: ts.clone(),
            content_type: old_meta.content_type,
            size: old_meta.size,
            sha256: old_meta.sha256,
            saved_at: now_rfc3339(),
        };
        atomic_write(
            &versions.join(format!("{ts}.json")),
            &serde_json::to_vec_pretty(&version_info).map_err(io::Error::from)?,
        )?;

        let meta = Meta {
            rev: old_meta.rev + 1,
            content_type: content_type.to_string(),
            created_at: old_meta.created_at,
            updated_at: now_rfc3339(),
            size: content.len() as u64,
            sha256: sha256_hex(content),
        };
        atomic_write(&dir.join("current"), content)?;
        atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta).map_err(io::Error::from)?)?;
        Ok((PutOutcome::Updated, meta))
    }

    pub fn get_current(&self, name: &str) -> io::Result<Option<(Vec<u8>, Meta)>> {
        if !self.exists(name) {
            return Ok(None);
        }
        Ok(Some((fs::read(self.artifact_dir(name).join("current"))?, self.read_meta(name)?)))
    }

    pub fn get_meta(&self, name: &str) -> io::Result<Option<(Meta, Vec<VersionInfo>)>> {
        if !self.exists(name) {
            return Ok(None);
        }
        let mut versions: Vec<VersionInfo> = Vec::new();
        let vdir = self.versions_dir(name);
        if vdir.is_dir() {
            for entry in fs::read_dir(&vdir)? {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "json") || path.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".tmp")) {
                    continue;
                }
                let raw = fs::read(path.with_extension("json"))?;
                versions.push(serde_json::from_slice(&raw)?);
            }
        }
        // File names are fixed-width timestamps, so lexicographic == newest last.
        versions.sort_by(|a, b| a.version.cmp(&b.version));
        Ok(Some((self.read_meta(name)?, versions)))
    }

    pub fn get_version(&self, name: &str, version: &str) -> io::Result<Option<(Vec<u8>, VersionInfo)>> {
        let content_path = self.versions_dir(name).join(version);
        if !content_path.is_file() {
            return Ok(None);
        }
        let meta_path = content_path.with_extension("json");
        if !meta_path.is_file() {
            return Ok(None);
        }
        Ok(Some((
            fs::read(&content_path)?,
            serde_json::from_slice(&fs::read(&meta_path)?)?,
        )))
    }

    /// Make `version` the current content; the outgoing current is archived
    /// as a new version entry. A rollback changes the content, so it bumps
    /// the rev like any other mutation.
    pub fn rollback(&self, name: &str, version: &str) -> io::Result<Option<Meta>> {
        let _guard = self.write_lock();
        let Some((content, vinfo)) = self.get_version(name, version)? else {
            return Ok(None);
        };
        let old_meta = self.read_meta(name)?;
        let old_content = fs::read(self.artifact_dir(name).join("current"))?;

        let ts = version_id();
        let versions = self.versions_dir(name);
        fs::create_dir_all(&versions)?;
        atomic_write(&versions.join(&ts), &old_content)?;
        let archived = VersionInfo {
            version: ts.clone(),
            content_type: old_meta.content_type,
            size: old_meta.size,
            sha256: old_meta.sha256,
            saved_at: now_rfc3339(),
        };
        atomic_write(
            &versions.join(format!("{ts}.json")),
            &serde_json::to_vec_pretty(&archived)?,
        )?;

        let meta = Meta {
            rev: old_meta.rev + 1,
            content_type: vinfo.content_type,
            created_at: old_meta.created_at,
            updated_at: now_rfc3339(),
            size: content.len() as u64,
            sha256: sha256_hex(&content),
        };
        let dir = self.artifact_dir(name);
        atomic_write(&dir.join("current"), &content)?;
        atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta)?)?;
        Ok(Some(meta))
    }

    /// Move an artifact to a new name: one atomic directory rename, so the
    /// content, history and timestamps come along unchanged. Renaming to the
    /// same name is a no-op. The old name keeps redirecting to the new one
    /// while the artifact lives there.
    pub fn rename(&self, from: &str, to: &str) -> io::Result<RenameOutcome> {
        let _guard = self.write_lock();
        if !self.exists(from) {
            return Ok(RenameOutcome::SourceMissing);
        }
        if from == to {
            return Ok(RenameOutcome::Renamed(self.read_meta(from)?));
        }
        if self.exists(to) {
            return Ok(RenameOutcome::TargetExists);
        }
        fs::rename(self.artifact_dir(from), self.artifact_dir(to))?;
        // The artifact itself now lives under `to`, so its stale alias goes;
        // aliases pointing at `from` follow the artifact to `to`; and `from`
        // starts redirecting.
        self.remove_alias(to);
        let aliases = self.aliases_dir();
        if aliases.is_dir() {
            for entry in fs::read_dir(&aliases)? {
                let path = entry?.path();
                if fs::read_to_string(&path).map(|t| t.trim() == from).unwrap_or(false) {
                    atomic_write(&path, to.as_bytes())?;
                }
            }
        }
        self.set_alias(from, to)?;
        Ok(RenameOutcome::Renamed(self.read_meta(to)?))
    }

    pub fn delete(&self, name: &str) -> io::Result<bool> {
        let _guard = self.write_lock();
        if !self.exists(name) {
            return Ok(false);
        }
        fs::remove_dir_all(self.artifact_dir(name))?;
        Ok(true)
    }

    pub fn list(&self, prefix: &str) -> io::Result<Vec<ListItem>> {
        let mut items = Vec::new();
        for entry in fs::read_dir(self.collection_root())? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !prefix.is_empty() && !name.starts_with(prefix) {
                continue;
            }
            if !path.join("current").is_file() {
                continue;
            }
            if let Ok(meta) = self.read_meta(name) {
                items.push(ListItem {
                    name: name.to_string(),
                    content_type: meta.content_type,
                    size: meta.size,
                    updated_at: meta.updated_at,
                });
            }
        }
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(items)
    }
}

/// Fixed-width filesystem-safe version id: 2026-10-09T18-22-33-123Z
fn version_id() -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]-[minute]-[second]-[subsecond digits:3]Z"
    );
    OffsetDateTime::now_utc().format(&fmt).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, FileStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::new(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn name_validation() {
        assert!(valid_name("a"));
        assert!(valid_name("report-2026"));
        assert!(valid_name("0"));
        assert!(!valid_name("A"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("a_b"));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"x".repeat(65)));
        assert!(valid_name(&"x".repeat(64)));
    }

    #[test]
    fn create_read_update_versions() {
        let (_dir, s) = store();
        let (outcome, meta) = s.put("demo", b"v1", "text/html", None).unwrap();
        assert_eq!(outcome, PutOutcome::Created);
        assert_eq!(meta.rev, 1);
        let (content, meta) = s.get_current("demo").unwrap().unwrap();
        assert_eq!(content, b"v1");
        assert_eq!(meta.content_type, "text/html");

        let (outcome, meta) = s.put("demo", b"v2", "text/html", None).unwrap();
        assert_eq!(outcome, PutOutcome::Updated);
        assert_eq!(meta.rev, 2);
        let (content, _) = s.get_current("demo").unwrap().unwrap();
        assert_eq!(content, b"v2");

        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].sha256, sha256_hex(b"v1"));

        // identical content is a no-op, no version churn, rev stays
        let (outcome, meta) = s.put("demo", b"v2", "text/html", None).unwrap();
        assert_eq!(outcome, PutOutcome::Unchanged);
        assert_eq!(meta.rev, 2);
        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
    }

    #[test]
    fn conditional_put_is_guarded_by_rev() {
        let (_dir, s) = store();

        // a conditional put on a missing artifact conflicts with rev 0
        match s.put("nope", b"x", "text/html", Some(0)) {
            Err(PutError::RevConflict { current }) => assert_eq!(current, 0),
            other => panic!("expected a rev conflict, got {other:?}"),
        }
        assert!(!s.exists("nope"));

        let (_, meta) = s.put("demo", b"v1", "text/html", None).unwrap();
        assert_eq!(meta.rev, 1);

        // matching rev updates and bumps
        let (outcome, meta) = s.put("demo", b"v2", "text/html", Some(1)).unwrap();
        assert_eq!(outcome, PutOutcome::Updated);
        assert_eq!(meta.rev, 2);

        // stale rev is a conflict carrying the current one; nothing written
        match s.put("demo", b"v3", "text/html", Some(1)) {
            Err(PutError::RevConflict { current }) => assert_eq!(current, 2),
            other => panic!("expected a rev conflict, got {other:?}"),
        }
        let (content, meta) = s.get_current("demo").unwrap().unwrap();
        assert_eq!(content, b"v2");
        assert_eq!(meta.rev, 2);
        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);

        // matching rev with identical content is Unchanged, rev stays
        let (outcome, meta) = s.put("demo", b"v2", "text/html", Some(2)).unwrap();
        assert_eq!(outcome, PutOutcome::Unchanged);
        assert_eq!(meta.rev, 2);
    }

    #[test]
    fn legacy_meta_without_rev_defaults_to_zero_and_migrates() {
        let (dir, s) = store();
        let art = dir.path().join("artifacts").join("legacy");
        fs::create_dir_all(&art).unwrap();
        fs::write(art.join("current"), b"old").unwrap();
        let legacy = format!(
            r#"{{"content_type":"text/html","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","size":3,"sha256":"{}"}}"#,
            sha256_hex(b"old")
        );
        fs::write(art.join("meta.json"), legacy).unwrap();

        // reads as rev 0, so a conditional update expecting 0 applies
        let (meta, _) = s.get_meta("legacy").unwrap().unwrap();
        assert_eq!(meta.rev, 0);
        let (outcome, meta) = s.put("legacy", b"new", "text/html", Some(0)).unwrap();
        assert_eq!(outcome, PutOutcome::Updated);
        assert_eq!(meta.rev, 1);

        // and the next writer sees the real rev
        match s.put("legacy", b"x", "text/html", Some(0)) {
            Err(PutError::RevConflict { current }) => assert_eq!(current, 1),
            other => panic!("expected a rev conflict, got {other:?}"),
        }
    }

    #[test]
    fn rollback_restores_content_and_bumps_rev() {
        let (_dir, s) = store();
        s.put("demo", b"v1", "text/html", None).unwrap();
        s.put("demo", b"v2", "text/html", None).unwrap();
        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        let first = versions[0].version.clone();

        let meta = s.rollback("demo", &first).unwrap().unwrap();
        let (content, _) = s.get_current("demo").unwrap().unwrap();
        assert_eq!(content, b"v1");
        assert_eq!(meta.created_at, s.read_meta("demo").unwrap().created_at);
        assert_eq!(meta.rev, 3);

        // rollback archived the outgoing current
        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        assert_eq!(versions.len(), 2);
    }

    #[test]
    fn rename_keeps_content_history_and_timestamps() {
        let (_dir, s) = store();
        s.put("demo", b"v1", "text/html", None).unwrap();
        s.put("demo", b"v2", "text/html", None).unwrap();
        s.put("taken", b"x", "text/plain", None).unwrap();
        let (_, created_at) = s.get_current("demo").unwrap().unwrap();
        let created_at = created_at.created_at;

        assert_eq!(s.rename("demo", "taken").unwrap(), RenameOutcome::TargetExists);
        assert!(s.exists("demo"));
        assert_eq!(s.rename("nope", "fresh").unwrap(), RenameOutcome::SourceMissing);
        // renaming to the same name is a no-op
        assert!(matches!(s.rename("demo", "demo").unwrap(), RenameOutcome::Renamed(_)));

        assert!(matches!(
            s.rename("demo", "renamed").unwrap(),
            RenameOutcome::Renamed(_)
        ));
        assert!(!s.exists("demo"));
        let (content, meta) = s.get_current("renamed").unwrap().unwrap();
        assert_eq!(content, b"v2");
        assert_eq!(meta.created_at, created_at);
        let (_, versions) = s.get_meta("renamed").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].sha256, sha256_hex(b"v1"));
    }

    #[test]
    fn rename_leaves_a_followable_redirect() {
        let (_dir, s) = store();
        s.put("a", b"v1", "text/html", None).unwrap();
        s.rename("a", "b").unwrap();
        assert_eq!(s.resolve_alias("a").unwrap().as_deref(), Some("b"));
        assert_eq!(s.resolve_alias("b").unwrap(), None);
        assert_eq!(s.resolve_alias("nope").unwrap(), None);

        // aliases pointing at the old name follow the artifact along
        s.rename("b", "c").unwrap();
        assert_eq!(s.resolve_alias("a").unwrap().as_deref(), Some("c"));
        assert_eq!(s.resolve_alias("b").unwrap().as_deref(), Some("c"));

        // renaming back consumes the alias sitting on the target name
        s.rename("c", "a").unwrap();
        assert_eq!(s.resolve_alias("a").unwrap(), None);
        assert_eq!(s.resolve_alias("c").unwrap().as_deref(), Some("a"));

        // recreating a name replaces its redirect with the artifact
        s.put("c", b"reincarnated", "text/html", None).unwrap();
        assert_eq!(s.resolve_alias("c").unwrap(), None);
    }

    #[test]
    fn list_with_prefix_and_delete() {
        let (_dir, s) = store();
        s.put("report-a", b"1", "text/plain", None).unwrap();
        s.put("report-b", b"22", "text/plain", None).unwrap();
        s.put("other", b"3", "text/plain", None).unwrap();

        let all = s.list("").unwrap();
        assert_eq!(all.len(), 3);
        let reports = s.list("report-").unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].size, 1);

        assert!(s.delete("other").unwrap());
        assert!(!s.exists("other"));
        assert!(!s.delete("other").unwrap());
        assert_eq!(s.list("").unwrap().len(), 2);
    }

    #[test]
    fn missing_artifact_is_none() {
        let (_dir, s) = store();
        assert!(s.get_current("nope").unwrap().is_none());
        assert!(s.get_meta("nope").unwrap().is_none());
        assert!(s.rollback("nope", "x").unwrap().is_none());
        assert_eq!(s.rename("nope", "x").unwrap(), RenameOutcome::SourceMissing);
    }

    #[test]
    fn image_store_is_a_separate_namespace() {
        let (dir, s) = store();
        let images = FileStore::image_store(dir.path()).unwrap();
        s.put("x-dup", b"page", "text/html", None).unwrap();
        images.put("x-dup", b"png-bytes", "image/png", None).unwrap();

        let (content, meta) = images.get_current("x-dup").unwrap().unwrap();
        assert_eq!(content, b"png-bytes");
        assert_eq!(meta.content_type, "image/png");
        let (content, meta) = s.get_current("x-dup").unwrap().unwrap();
        assert_eq!(content, b"page");
        assert_eq!(meta.content_type, "text/html");

        // each namespace lists only its own entries
        assert_eq!(s.list("").unwrap().len(), 1);
        assert_eq!(images.list("").unwrap().len(), 1);

        // deleting an image never touches the artifact under the same name
        assert!(images.delete("x-dup").unwrap());
        assert!(!images.exists("x-dup"));
        assert!(s.exists("x-dup"));
    }
}
