//! File-backed artifact store.
//!
//! Layout under the data dir:
//!
//! ```text
//! artifacts/<name>/
//!   current          # current version content
//!   meta.json        # content_type, timestamps, size, sha256
//!   versions/
//!     2026-10-09T18-22-33-123Z       # previous content
//!     2026-10-09T18-22-33-123Z.json  # its meta at the time of saving
//! ```
//!
//! All writes are atomic (tmp file + rename). Artifact names are strictly
//! validated, so they are always a single safe path component.

use std::fs;
use std::io;
use std::path::PathBuf;

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
}

impl FileStore {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let store = FileStore { root: root.into() };
        fs::create_dir_all(store.artifacts_root())?;
        Ok(store)
    }

    fn artifacts_root(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    fn artifact_dir(&self, name: &str) -> PathBuf {
        self.artifacts_root().join(name)
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

    /// Create or update an artifact. Updating moves the previous `current`
    /// into `versions/`; identical content+type is a no-op.
    pub fn put(&self, name: &str, content: &[u8], content_type: &str) -> io::Result<PutOutcome> {
        let dir = self.artifact_dir(name);
        if !self.exists(name) {
            fs::create_dir_all(&dir)?;
            let meta = Meta {
                content_type: content_type.to_string(),
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                size: content.len() as u64,
                sha256: sha256_hex(content),
            };
            atomic_write(&dir.join("current"), content)?;
            atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta)?)?;
            return Ok(PutOutcome::Created);
        }

        let old_meta = self.read_meta(name)?;
        if old_meta.content_type == content_type && old_meta.sha256 == sha256_hex(content) {
            return Ok(PutOutcome::Unchanged);
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
            &serde_json::to_vec_pretty(&version_info)?,
        )?;

        let meta = Meta {
            content_type: content_type.to_string(),
            created_at: old_meta.created_at,
            updated_at: now_rfc3339(),
            size: content.len() as u64,
            sha256: sha256_hex(content),
        };
        atomic_write(&dir.join("current"), content)?;
        atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta)?)?;
        Ok(PutOutcome::Updated)
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
    /// as a new version entry.
    pub fn rollback(&self, name: &str, version: &str) -> io::Result<Option<Meta>> {
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

    pub fn delete(&self, name: &str) -> io::Result<bool> {
        if !self.exists(name) {
            return Ok(false);
        }
        fs::remove_dir_all(self.artifact_dir(name))?;
        Ok(true)
    }

    pub fn list(&self, prefix: &str) -> io::Result<Vec<ListItem>> {
        let mut items = Vec::new();
        for entry in fs::read_dir(self.artifacts_root())? {
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
        assert_eq!(s.put("demo", b"v1", "text/html").unwrap(), PutOutcome::Created);
        let (content, meta) = s.get_current("demo").unwrap().unwrap();
        assert_eq!(content, b"v1");
        assert_eq!(meta.content_type, "text/html");

        assert_eq!(s.put("demo", b"v2", "text/html").unwrap(), PutOutcome::Updated);
        let (content, _) = s.get_current("demo").unwrap().unwrap();
        assert_eq!(content, b"v2");

        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].sha256, sha256_hex(b"v1"));

        // identical content is a no-op, no version churn
        assert_eq!(s.put("demo", b"v2", "text/html").unwrap(), PutOutcome::Unchanged);
        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        assert_eq!(versions.len(), 1);
    }

    #[test]
    fn rollback_restores_content() {
        let (_dir, s) = store();
        s.put("demo", b"v1", "text/html").unwrap();
        s.put("demo", b"v2", "text/html").unwrap();
        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        let first = versions[0].version.clone();

        let meta = s.rollback("demo", &first).unwrap().unwrap();
        let (content, _) = s.get_current("demo").unwrap().unwrap();
        assert_eq!(content, b"v1");
        assert_eq!(meta.created_at, s.read_meta("demo").unwrap().created_at);

        // rollback archived the outgoing current
        let (_, versions) = s.get_meta("demo").unwrap().unwrap();
        assert_eq!(versions.len(), 2);
    }

    #[test]
    fn list_with_prefix_and_delete() {
        let (_dir, s) = store();
        s.put("report-a", b"1", "text/plain").unwrap();
        s.put("report-b", b"22", "text/plain").unwrap();
        s.put("other", b"3", "text/plain").unwrap();

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
    }
}
