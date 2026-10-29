//! File-backed artifact store.
//!
//! Layout under the data dir:
//!
//! ```text
//! artifacts/<project>/<name>/    # project-scoped artifact
//! artifacts/<name>/              # legacy artifact, stored before projects
//!   current          # current version content
//!   meta.json        # rev, content_type, timestamps, size, sha256
//!   versions/
//!     2026-10-09T18-22-33-123Z       # previous content
//!     2026-10-09T18-22-33-123Z.json  # its meta at the time of saving
//! images/<project>/<name>/       # same structure, image namespace (no rename, no aliases)
//! aliases/[<project>/]<old-name> # renamed away: file content is the new name
//! ```
//!
//! A project is one more path level: the same `name` may exist in several
//! projects and in the legacy flat namespace at once — they are independent
//! artifacts with independent URLs. Projects are addressed by `Option<&str>`
//! (`None` = legacy flat namespace) and validated with the same rule as
//! names, so a project is always a single safe path component too.
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

/// Names must match `^[a-z0-9][a-z0-9-]{0,63}$`. Projects use the same rule.
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
    /// Project namespace, `null` for legacy artifacts stored before projects.
    pub project: Option<String>,
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
    RevConflict {
        current: u64,
    },
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
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
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
        self.write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn collection_root(&self) -> PathBuf {
        self.root.join(self.collection)
    }

    fn aliases_dir(&self) -> PathBuf {
        self.root.join(self.aliases)
    }

    fn alias_path(&self, project: Option<&str>, name: &str) -> PathBuf {
        match project {
            Some(p) => self.aliases_dir().join(p).join(name),
            None => self.aliases_dir().join(name),
        }
    }

    fn artifact_dir(&self, project: Option<&str>, name: &str) -> PathBuf {
        match project {
            Some(p) => self.collection_root().join(p).join(name),
            None => self.collection_root().join(name),
        }
    }

    fn versions_dir(&self, project: Option<&str>, name: &str) -> PathBuf {
        self.artifact_dir(project, name).join("versions")
    }

    pub fn exists(&self, project: Option<&str>, name: &str) -> bool {
        self.artifact_dir(project, name).join("current").is_file()
    }

    fn read_meta(&self, project: Option<&str>, name: &str) -> io::Result<Meta> {
        let raw = fs::read(self.artifact_dir(project, name).join("meta.json"))?;
        Ok(serde_json::from_slice(&raw)?)
    }

    /// The name an old public URL should redirect to, if any: `name` was
    /// renamed away and no artifact lives under it anymore. Project-scoped
    /// renames track their aliases under `aliases/<project>/`, so a name in
    /// one project never redirects into another.
    pub fn resolve_alias(&self, project: Option<&str>, name: &str) -> io::Result<Option<String>> {
        match fs::read_to_string(self.alias_path(project, name)) {
            Ok(target) => {
                let target = target.trim();
                Ok(valid_name(target).then(|| target.to_string()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn set_alias(&self, project: Option<&str>, from: &str, to: &str) -> io::Result<()> {
        let path = self.alias_path(project, from);
        fs::create_dir_all(path.parent().unwrap_or(&self.aliases_dir()))?;
        atomic_write(&path, to.as_bytes())
    }

    fn remove_alias(&self, project: Option<&str>, name: &str) {
        // best-effort: a missing alias file is not an error
        let _ = fs::remove_file(self.alias_path(project, name));
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
        project: Option<&str>,
        name: &str,
        content: &[u8],
        content_type: &str,
        expect_rev: Option<u64>,
    ) -> Result<(PutOutcome, Meta), PutError> {
        let _guard = self.write_lock();
        let dir = self.artifact_dir(project, name);
        if !self.exists(project, name) {
            if expect_rev.is_some() {
                // a conditional put targets a revision; of a missing artifact
                // the current rev is 0
                return Err(PutError::RevConflict { current: 0 });
            }
            fs::create_dir_all(&dir)?;
            self.remove_alias(project, name);
            let meta = Meta {
                rev: 1,
                content_type: content_type.to_string(),
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                size: content.len() as u64,
                sha256: sha256_hex(content),
            };
            atomic_write(&dir.join("current"), content)?;
            atomic_write(
                &dir.join("meta.json"),
                &serde_json::to_vec_pretty(&meta).map_err(io::Error::from)?,
            )?;
            return Ok((PutOutcome::Created, meta));
        }

        let old_meta = self.read_meta(project, name)?;
        if let Some(expected) = expect_rev {
            if old_meta.rev != expected {
                return Err(PutError::RevConflict {
                    current: old_meta.rev,
                });
            }
        }
        if old_meta.content_type == content_type && old_meta.sha256 == sha256_hex(content) {
            return Ok((PutOutcome::Unchanged, old_meta));
        }

        let old_content = fs::read(dir.join("current"))?;
        let ts = version_id();
        let versions = self.versions_dir(project, name);
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
        atomic_write(
            &dir.join("meta.json"),
            &serde_json::to_vec_pretty(&meta).map_err(io::Error::from)?,
        )?;
        Ok((PutOutcome::Updated, meta))
    }

    pub fn get_current(
        &self,
        project: Option<&str>,
        name: &str,
    ) -> io::Result<Option<(Vec<u8>, Meta)>> {
        if !self.exists(project, name) {
            return Ok(None);
        }
        Ok(Some((
            fs::read(self.artifact_dir(project, name).join("current"))?,
            self.read_meta(project, name)?,
        )))
    }

    pub fn get_meta(
        &self,
        project: Option<&str>,
        name: &str,
    ) -> io::Result<Option<(Meta, Vec<VersionInfo>)>> {
        if !self.exists(project, name) {
            return Ok(None);
        }
        let mut versions: Vec<VersionInfo> = Vec::new();
        let vdir = self.versions_dir(project, name);
        if vdir.is_dir() {
            for entry in fs::read_dir(&vdir)? {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "json")
                    || path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().ends_with(".tmp"))
                {
                    continue;
                }
                let raw = fs::read(path.with_extension("json"))?;
                versions.push(serde_json::from_slice(&raw)?);
            }
        }
        // File names are fixed-width timestamps, so lexicographic == newest last.
        versions.sort_by(|a, b| a.version.cmp(&b.version));
        Ok(Some((self.read_meta(project, name)?, versions)))
    }

    pub fn get_version(
        &self,
        project: Option<&str>,
        name: &str,
        version: &str,
    ) -> io::Result<Option<(Vec<u8>, VersionInfo)>> {
        let content_path = self.versions_dir(project, name).join(version);
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
    pub fn rollback(
        &self,
        project: Option<&str>,
        name: &str,
        version: &str,
    ) -> io::Result<Option<Meta>> {
        let _guard = self.write_lock();
        let Some((content, vinfo)) = self.get_version(project, name, version)? else {
            return Ok(None);
        };
        let old_meta = self.read_meta(project, name)?;
        let old_content = fs::read(self.artifact_dir(project, name).join("current"))?;

        let ts = version_id();
        let versions = self.versions_dir(project, name);
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
        let dir = self.artifact_dir(project, name);
        atomic_write(&dir.join("current"), &content)?;
        atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta)?)?;
        Ok(Some(meta))
    }

    /// Move an artifact to a new name in the same project: one atomic
    /// directory rename, so the content, history and timestamps come along
    /// unchanged. Renaming to the same name is a no-op. The old name keeps
    /// redirecting to the new one while the artifact lives there — only
    /// within this project's namespace, never across projects.
    pub fn rename(&self, project: Option<&str>, from: &str, to: &str) -> io::Result<RenameOutcome> {
        let _guard = self.write_lock();
        if !self.exists(project, from) {
            return Ok(RenameOutcome::SourceMissing);
        }
        if from == to {
            return Ok(RenameOutcome::Renamed(self.read_meta(project, from)?));
        }
        if self.exists(project, to) {
            return Ok(RenameOutcome::TargetExists);
        }
        fs::rename(
            self.artifact_dir(project, from),
            self.artifact_dir(project, to),
        )?;
        // The artifact itself now lives under `to`, so its stale alias goes;
        // aliases pointing at `from` follow the artifact to `to`; and `from`
        // starts redirecting. Only this namespace's aliases are scanned: a
        // flat alias pointing at `from` refers to the flat artifact, not this
        // project's one.
        self.remove_alias(project, to);
        let aliases = match project {
            Some(p) => self.aliases_dir().join(p),
            None => self.aliases_dir(),
        };
        if aliases.is_dir() {
            for entry in fs::read_dir(&aliases)? {
                let path = entry?.path();
                if !path.is_file() {
                    continue;
                }
                if fs::read_to_string(&path)
                    .map(|t| t.trim() == from)
                    .unwrap_or(false)
                {
                    atomic_write(&path, to.as_bytes())?;
                }
            }
        }
        self.set_alias(project, from, to)?;
        Ok(RenameOutcome::Renamed(self.read_meta(project, to)?))
    }

    pub fn delete(&self, project: Option<&str>, name: &str) -> io::Result<bool> {
        let _guard = self.write_lock();
        if !self.exists(project, name) {
            return Ok(false);
        }
        fs::remove_dir_all(self.artifact_dir(project, name))?;
        // Best-effort: an emptied project dir serves nothing; drop it so the
        // tree doesn't accumulate hollow dirs. Fails harmlessly when other
        // artifacts still live in the project.
        if let Some(p) = project {
            let _ = fs::remove_dir(self.collection_root().join(p));
        }
        Ok(true)
    }

    /// List artifacts. `project`: `None` lists everything (legacy first, then
    /// each project), `Some("")` lists only the legacy flat namespace,
    /// `Some(p)` lists only project `p`. `prefix` filters by name.
    pub fn list(&self, prefix: &str, project: Option<&str>) -> io::Result<Vec<ListItem>> {
        let mut items = Vec::new();
        for entry in fs::read_dir(self.collection_root())? {
            let path = entry?.path();
            let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if path.join("current").is_file() {
                // legacy flat artifact
                if matches!(project, Some(p) if !p.is_empty()) {
                    continue;
                }
                if let Ok(meta) = self.read_meta(None, dir_name) {
                    items.push(ListItem {
                        name: dir_name.to_string(),
                        project: None,
                        content_type: meta.content_type,
                        size: meta.size,
                        updated_at: meta.updated_at,
                    });
                }
            } else if path.is_dir() && valid_name(dir_name) {
                // a project dir: its children with a `current` file are artifacts.
                // A valid dir name is never empty, so `Some("")` (legacy only)
                // excludes every project dir.
                if matches!(project, Some(p) if p != dir_name) {
                    continue;
                }
                for sub in fs::read_dir(&path)? {
                    let sub_path = sub?.path();
                    let Some(name) = sub_path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !sub_path.join("current").is_file() {
                        continue;
                    }
                    if let Ok(meta) = self.read_meta(Some(dir_name), name) {
                        items.push(ListItem {
                            name: name.to_string(),
                            project: Some(dir_name.to_string()),
                            content_type: meta.content_type,
                            size: meta.size,
                            updated_at: meta.updated_at,
                        });
                    }
                }
            }
        }
        items.sort_by(|a, b| {
            (a.project.as_deref().unwrap_or(""), &a.name)
                .cmp(&(b.project.as_deref().unwrap_or(""), &b.name))
        });
        if !prefix.is_empty() {
            items.retain(|item| item.name.starts_with(prefix));
        }
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
        let (outcome, meta) = s.put(None, "demo", b"v1", "text/html", None).unwrap();
        assert_eq!(outcome, PutOutcome::Created);
        assert_eq!(meta.rev, 1);
        let (content, meta) = s.get_current(None, "demo").unwrap().unwrap();
        assert_eq!(content, b"v1");
        assert_eq!(meta.content_type, "text/html");

        let (outcome, meta) = s.put(None, "demo", b"v2", "text/html", None).unwrap();
        assert_eq!(outcome, PutOutcome::Updated);
        assert_eq!(meta.rev, 2);
        let (content, _) = s.get_current(None, "demo").unwrap().unwrap();
        assert_eq!(content, b"v2");

        let (_, versions) = s.get_meta(None, "demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].sha256, sha256_hex(b"v1"));

        // identical content is a no-op, no version churn, rev stays
        let (outcome, meta) = s.put(None, "demo", b"v2", "text/html", None).unwrap();
        assert_eq!(outcome, PutOutcome::Unchanged);
        assert_eq!(meta.rev, 2);
        let (_, versions) = s.get_meta(None, "demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
    }

    #[test]
    fn conditional_put_is_guarded_by_rev() {
        let (_dir, s) = store();

        // a conditional put on a missing artifact conflicts with rev 0
        match s.put(None, "nope", b"x", "text/html", Some(0)) {
            Err(PutError::RevConflict { current }) => assert_eq!(current, 0),
            other => panic!("expected a rev conflict, got {other:?}"),
        }
        assert!(!s.exists(None, "nope"));

        let (_, meta) = s.put(None, "demo", b"v1", "text/html", None).unwrap();
        assert_eq!(meta.rev, 1);

        // matching rev updates and bumps
        let (outcome, meta) = s.put(None, "demo", b"v2", "text/html", Some(1)).unwrap();
        assert_eq!(outcome, PutOutcome::Updated);
        assert_eq!(meta.rev, 2);

        // stale rev is a conflict carrying the current one; nothing written
        match s.put(None, "demo", b"v3", "text/html", Some(1)) {
            Err(PutError::RevConflict { current }) => assert_eq!(current, 2),
            other => panic!("expected a rev conflict, got {other:?}"),
        }
        let (content, meta) = s.get_current(None, "demo").unwrap().unwrap();
        assert_eq!(content, b"v2");
        assert_eq!(meta.rev, 2);
        let (_, versions) = s.get_meta(None, "demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);

        // matching rev with identical content is Unchanged, rev stays
        let (outcome, meta) = s.put(None, "demo", b"v2", "text/html", Some(2)).unwrap();
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
        let (meta, _) = s.get_meta(None, "legacy").unwrap().unwrap();
        assert_eq!(meta.rev, 0);
        let (outcome, meta) = s.put(None, "legacy", b"new", "text/html", Some(0)).unwrap();
        assert_eq!(outcome, PutOutcome::Updated);
        assert_eq!(meta.rev, 1);

        // and the next writer sees the real rev
        match s.put(None, "legacy", b"x", "text/html", Some(0)) {
            Err(PutError::RevConflict { current }) => assert_eq!(current, 1),
            other => panic!("expected a rev conflict, got {other:?}"),
        }
    }

    #[test]
    fn rollback_restores_content_and_bumps_rev() {
        let (_dir, s) = store();
        s.put(None, "demo", b"v1", "text/html", None).unwrap();
        s.put(None, "demo", b"v2", "text/html", None).unwrap();
        let (_, versions) = s.get_meta(None, "demo").unwrap().unwrap();
        let first = versions[0].version.clone();

        let meta = s.rollback(None, "demo", &first).unwrap().unwrap();
        let (content, _) = s.get_current(None, "demo").unwrap().unwrap();
        assert_eq!(content, b"v1");
        assert_eq!(
            meta.created_at,
            s.read_meta(None, "demo").unwrap().created_at
        );
        assert_eq!(meta.rev, 3);

        // rollback archived the outgoing current
        let (_, versions) = s.get_meta(None, "demo").unwrap().unwrap();
        assert_eq!(versions.len(), 2);
    }

    #[test]
    fn rename_keeps_content_history_and_timestamps() {
        let (_dir, s) = store();
        s.put(None, "demo", b"v1", "text/html", None).unwrap();
        s.put(None, "demo", b"v2", "text/html", None).unwrap();
        s.put(None, "taken", b"x", "text/plain", None).unwrap();
        let (_, created_at) = s.get_current(None, "demo").unwrap().unwrap();
        let created_at = created_at.created_at;

        assert_eq!(
            s.rename(None, "demo", "taken").unwrap(),
            RenameOutcome::TargetExists
        );
        assert!(s.exists(None, "demo"));
        assert_eq!(
            s.rename(None, "nope", "fresh").unwrap(),
            RenameOutcome::SourceMissing
        );
        // renaming to the same name is a no-op
        assert!(matches!(
            s.rename(None, "demo", "demo").unwrap(),
            RenameOutcome::Renamed(_)
        ));

        assert!(matches!(
            s.rename(None, "demo", "renamed").unwrap(),
            RenameOutcome::Renamed(_)
        ));
        assert!(!s.exists(None, "demo"));
        let (content, meta) = s.get_current(None, "renamed").unwrap().unwrap();
        assert_eq!(content, b"v2");
        assert_eq!(meta.created_at, created_at);
        let (_, versions) = s.get_meta(None, "renamed").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].sha256, sha256_hex(b"v1"));
    }

    #[test]
    fn rename_leaves_a_followable_redirect() {
        let (_dir, s) = store();
        s.put(None, "a", b"v1", "text/html", None).unwrap();
        s.rename(None, "a", "b").unwrap();
        assert_eq!(s.resolve_alias(None, "a").unwrap().as_deref(), Some("b"));
        assert_eq!(s.resolve_alias(None, "b").unwrap(), None);
        assert_eq!(s.resolve_alias(None, "nope").unwrap(), None);

        // aliases pointing at the old name follow the artifact along
        s.rename(None, "b", "c").unwrap();
        assert_eq!(s.resolve_alias(None, "a").unwrap().as_deref(), Some("c"));
        assert_eq!(s.resolve_alias(None, "b").unwrap().as_deref(), Some("c"));

        // renaming back consumes the alias sitting on the target name
        s.rename(None, "c", "a").unwrap();
        assert_eq!(s.resolve_alias(None, "a").unwrap(), None);
        assert_eq!(s.resolve_alias(None, "c").unwrap().as_deref(), Some("a"));

        // recreating a name replaces its redirect with the artifact
        s.put(None, "c", b"reincarnated", "text/html", None)
            .unwrap();
        assert_eq!(s.resolve_alias(None, "c").unwrap(), None);
    }

    #[test]
    fn list_with_prefix_and_delete() {
        let (_dir, s) = store();
        s.put(None, "report-a", b"1", "text/plain", None).unwrap();
        s.put(None, "report-b", b"22", "text/plain", None).unwrap();
        s.put(None, "other", b"3", "text/plain", None).unwrap();

        let all = s.list("", None).unwrap();
        assert_eq!(all.len(), 3);
        let reports = s.list("report-", None).unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].size, 1);

        assert!(s.delete(None, "other").unwrap());
        assert!(!s.exists(None, "other"));
        assert!(!s.delete(None, "other").unwrap());
        assert_eq!(s.list("", None).unwrap().len(), 2);
    }

    #[test]
    fn missing_artifact_is_none() {
        let (_dir, s) = store();
        assert!(s.get_current(None, "nope").unwrap().is_none());
        assert!(s.get_meta(None, "nope").unwrap().is_none());
        assert!(s.rollback(None, "nope", "x").unwrap().is_none());
        assert_eq!(
            s.rename(None, "nope", "x").unwrap(),
            RenameOutcome::SourceMissing
        );
    }

    #[test]
    fn image_store_is_a_separate_namespace() {
        let (dir, s) = store();
        let images = FileStore::image_store(dir.path()).unwrap();
        s.put(None, "x-dup", b"page", "text/html", None).unwrap();
        images
            .put(None, "x-dup", b"png-bytes", "image/png", None)
            .unwrap();

        let (content, meta) = images.get_current(None, "x-dup").unwrap().unwrap();
        assert_eq!(content, b"png-bytes");
        assert_eq!(meta.content_type, "image/png");
        let (content, meta) = s.get_current(None, "x-dup").unwrap().unwrap();
        assert_eq!(content, b"page");
        assert_eq!(meta.content_type, "text/html");

        // each namespace lists only its own entries
        assert_eq!(s.list("", None).unwrap().len(), 1);
        assert_eq!(images.list("", None).unwrap().len(), 1);

        // deleting an image never touches the artifact under the same name
        assert!(images.delete(None, "x-dup").unwrap());
        assert!(!images.exists(None, "x-dup"));
        assert!(s.exists(None, "x-dup"));
    }

    #[test]
    fn project_artifacts_are_independent_namespaces() {
        let (dir, s) = store();
        s.put(Some("minisklad"), "demo", b"mine", "text/html", None)
            .unwrap();
        s.put(Some("kladovkin"), "demo", b"theirs", "text/html", None)
            .unwrap();
        s.put(None, "demo", b"legacy", "text/html", None).unwrap();

        // the same name is three independent artifacts, one per namespace
        assert_eq!(
            s.get_current(Some("minisklad"), "demo").unwrap().unwrap().0,
            b"mine"
        );
        assert_eq!(
            s.get_current(Some("kladovkin"), "demo").unwrap().unwrap().0,
            b"theirs"
        );
        assert_eq!(s.get_current(None, "demo").unwrap().unwrap().0, b"legacy");

        // project artifacts land in their own directories
        assert!(dir
            .path()
            .join("artifacts/minisklad/demo/current")
            .is_file());
        assert!(dir
            .path()
            .join("artifacts/kladovkin/demo/current")
            .is_file());

        // version history is per namespace too
        s.put(Some("minisklad"), "demo", b"mine-2", "text/html", None)
            .unwrap();
        let (_, versions) = s.get_meta(Some("minisklad"), "demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].sha256, sha256_hex(b"mine"));

        // updating one namespace never touches the others
        assert_eq!(
            s.get_current(Some("kladovkin"), "demo").unwrap().unwrap().0,
            b"theirs"
        );
        assert_eq!(s.get_current(None, "demo").unwrap().unwrap().0, b"legacy");
    }

    #[test]
    fn list_filters_by_project_and_prefix() {
        let (_dir, s) = store();
        s.put(None, "shared", b"legacy", "text/plain", None)
            .unwrap();
        s.put(Some("alpha"), "report-a", b"1", "text/plain", None)
            .unwrap();
        s.put(Some("alpha"), "report-b", b"22", "text/plain", None)
            .unwrap();
        s.put(Some("beta"), "report-a", b"3", "text/plain", None)
            .unwrap();

        // everything: legacy first, then per project, names sorted within
        let all = s.list("", None).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].project, None);
        assert_eq!(all[0].name, "shared");
        assert_eq!(all[1].project.as_deref(), Some("alpha"));
        assert_eq!(all[3].project.as_deref(), Some("beta"));

        // one project only
        let alpha = s.list("", Some("alpha")).unwrap();
        assert_eq!(
            alpha.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["report-a", "report-b"]
        );

        // legacy only
        let legacy = s.list("", Some("")).unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].project, None);

        // prefix filter composes with the project filter
        let reports = s.list("report-", Some("alpha")).unwrap();
        assert_eq!(reports.len(), 2);
        assert!(s.list("shared", Some("alpha")).unwrap().is_empty());
    }

    #[test]
    fn rename_in_project_scopes_the_alias() {
        let (_dir, s) = store();
        s.put(None, "demo", b"legacy", "text/html", None).unwrap();
        s.put(Some("alpha"), "demo", b"mine", "text/html", None)
            .unwrap();
        s.put(Some("beta"), "demo", b"theirs", "text/html", None)
            .unwrap();

        assert!(matches!(
            s.rename(Some("alpha"), "demo", "moved").unwrap(),
            RenameOutcome::Renamed(_)
        ));
        // the alias lives inside the project: alpha redirects, beta and the
        // flat namespace never do
        assert_eq!(
            s.resolve_alias(Some("alpha"), "demo").unwrap().as_deref(),
            Some("moved")
        );
        assert_eq!(s.resolve_alias(Some("beta"), "demo").unwrap(), None);
        assert_eq!(s.resolve_alias(None, "demo").unwrap(), None);
        assert!(s.exists(Some("beta"), "demo"));
        assert!(s.exists(None, "demo"));

        // the moved artifact keeps its content, under the project dir
        assert_eq!(
            s.get_current(Some("alpha"), "moved").unwrap().unwrap().0,
            b"mine"
        );
        assert!(!s.exists(Some("alpha"), "demo"));
    }

    #[test]
    fn delete_clears_an_emptied_project_dir() {
        let (dir, s) = store();
        s.put(Some("alpha"), "a", b"1", "text/plain", None).unwrap();
        s.put(Some("alpha"), "b", b"2", "text/plain", None).unwrap();

        assert!(s.delete(Some("alpha"), "a").unwrap());
        assert!(dir.path().join("artifacts/alpha").is_dir());
        assert!(s.delete(Some("alpha"), "b").unwrap());
        assert!(!dir.path().join("artifacts/alpha").exists());
    }
}
