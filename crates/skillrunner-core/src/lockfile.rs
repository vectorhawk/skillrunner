//! Lockfile types and IO for project-scope skill installs.
//!
//! The lockfile lives at `.vectorhawk/skills.lock.json` relative to a project
//! root and is committed to version control. It records the exact source and
//! version of every project-scoped skill so teammates can reproduce the same
//! install state.

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Filename of the lockfile within the `.vectorhawk/` directory.
const LOCKFILE_NAME: &str = "skills.lock.json";
/// The hidden directory that anchors a project's VectorHawk state.
const VH_DIR: &str = ".vectorhawk";

// ── Data types ────────────────────────────────────────────────────────────────

/// The full lockfile: version header + one entry per installed skill.
///
/// `BTreeMap` is used for `skills` so the serialized JSON has stable,
/// alphabetically sorted keys — important for deterministic git diffs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u8,
    pub skills: BTreeMap<String, LockedSkill>,
}

/// One skill entry in the lockfile.
///
/// Serde's `#[serde(tag = "source")]` writes `"source": "registry"` or
/// `"source": "local"` inline, matching the architecture doc format exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum LockedSkill {
    Registry {
        version: String,
        registry_url: String,
        /// SHA-256 integrity string, format: `"sha256-{hex}"`.
        integrity: String,
    },
    Local {
        /// Path to the skill bundle, relative to the lockfile's parent directory.
        local_path: String,
    },
}

// ── impl Lockfile ─────────────────────────────────────────────────────────────

impl Lockfile {
    /// Create an empty lockfile with `version = 1`.
    pub fn new() -> Self {
        Self {
            version: 1,
            skills: BTreeMap::new(),
        }
    }

    /// Deserialize a lockfile from `path`.
    ///
    /// Returns an error if the file does not exist or cannot be parsed.
    pub fn load(path: &Utf8Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read lockfile at {path}"))?;
        let lockfile: Self = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse lockfile at {path}"))?;
        Ok(lockfile)
    }

    /// Serialize and atomically write the lockfile to `path`.
    ///
    /// Writes to a `.tmp` sibling first, then renames into place so a crash
    /// mid-write cannot leave a partial file. The JSON is pretty-printed with
    /// 2-space indentation because this file is committed to git.
    pub fn save(&self, path: &Utf8Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .context("failed to serialize lockfile")?;

        // Re-indent to 2 spaces. serde_json::to_string_pretty uses 2 spaces
        // by default, so this is already correct — but we add a trailing
        // newline to satisfy standard text-file conventions.
        let json_with_newline = format!("{json}\n");

        // Atomic write: temp file in same directory, then rename.
        let tmp_path_std = {
            let std_path = path.as_std_path();
            let parent = std_path
                .parent()
                .with_context(|| format!("lockfile path has no parent: {path}"))?;
            parent.join(format!("{LOCKFILE_NAME}.tmp"))
        };

        std::fs::write(&tmp_path_std, json_with_newline.as_bytes())
            .with_context(|| format!("failed to write tmp lockfile at {}", tmp_path_std.display()))?;

        std::fs::rename(&tmp_path_std, path.as_std_path())
            .with_context(|| format!("failed to rename tmp lockfile into place at {path}"))?;

        Ok(())
    }

    /// Walk ancestors of `start_dir` looking for `.vectorhawk/skills.lock.json`.
    ///
    /// Returns the path to the lockfile if found, `None` if we reach the
    /// filesystem root without finding one. Stops at root, not at `.git`,
    /// to support monorepos with `.vectorhawk/` in a subdirectory.
    pub fn discover(start_dir: &Utf8Path) -> Option<Utf8PathBuf> {
        let mut current: &Utf8Path = start_dir;
        loop {
            let candidate = current.join(VH_DIR).join(LOCKFILE_NAME);
            if candidate.exists() {
                return Some(candidate);
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => return None,
            }
        }
    }

    /// Insert or replace a skill entry. Returns the previous entry if one existed.
    pub fn upsert(&mut self, skill_id: String, entry: LockedSkill) -> Option<LockedSkill> {
        self.skills.insert(skill_id, entry)
    }

    /// Remove a skill entry. Returns the removed entry, or `None` if absent.
    pub fn remove(&mut self, skill_id: &str) -> Option<LockedSkill> {
        self.skills.remove(skill_id)
    }
}

impl Default for Lockfile {
    fn default() -> Self {
        Self::new()
    }
}

// ── Integrity verification ─────────────────────────────────────────────────────

/// Compute a deterministic SHA-256 hash of a skill directory and compare it
/// against `expected`.
///
/// # Hashing strategy
///
/// All files under `dir_path` are walked, sorted by their path relative to
/// `dir_path`, then fed into the hasher in the form:
///
/// ```text
/// "{relative_path}\n{file_bytes}"
/// ```
///
/// for each file. Only regular files are hashed; directories and symlinks are
/// skipped. OS artifacts (`.DS_Store`) are excluded so macOS and Linux produce
/// identical hashes for the same logical content.
///
/// # Expected format
///
/// `expected` must be either:
/// - `""` — empty string; treated as "no integrity recorded" and returns `true`
///   unconditionally (backwards compatibility for lockfile entries written
///   before integrity tracking was added).
/// - `"sha256-{hex}"` — compared against the computed digest.
///
/// Any other prefix returns an error.
///
/// # Returns
///
/// `Ok(true)` if the computed hash matches `expected` (or `expected` is empty).
/// `Ok(false)` if the hash does not match.
/// `Err(_)` if the directory cannot be read or `expected` has an unsupported format.
pub fn verify_integrity(dir_path: &Utf8Path, expected: &str) -> Result<bool> {
    // Backwards-compat: empty string means "not recorded" — always pass.
    if expected.is_empty() {
        return Ok(true);
    }

    // Parse prefix.
    let hex_expected = expected
        .strip_prefix("sha256-")
        .with_context(|| format!("unsupported integrity format: '{expected}' (expected 'sha256-{{hex}}')"))?;

    let computed = hash_skill_dir(dir_path)?;
    Ok(computed == hex_expected)
}

/// Compute and return the `sha256-{hex}` integrity string for `dir_path`.
///
/// Same hashing strategy as [`verify_integrity`]. Call this after writing a
/// skill bundle to produce the string to record in the lockfile.
pub fn compute_integrity(dir_path: &Utf8Path) -> Result<String> {
    let hex = hash_skill_dir(dir_path)?;
    Ok(format!("sha256-{hex}"))
}

/// Walk `dir_path`, collect all regular files sorted by relative path, and
/// return the lowercase hex SHA-256 digest.
///
/// Files whose name is exactly `.DS_Store` are skipped so macOS hosts produce
/// the same hash as Linux hosts for the same logical skill content.
fn hash_skill_dir(dir_path: &Utf8Path) -> Result<String> {
    // Collect (relative_path_string, absolute_path) for every regular file.
    let mut entries: Vec<(String, Utf8PathBuf)> = Vec::new();
    collect_files(dir_path, dir_path, &mut entries)?;

    // Sort by the relative path so the hash is stable regardless of readdir order.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel_path, abs_path) in &entries {
        let bytes = std::fs::read(abs_path.as_std_path())
            .with_context(|| format!("failed to read file for hashing: {abs_path}"))?;
        // Feed the relative path and then the file bytes.
        hasher.update(rel_path.as_bytes());
        hasher.update(b"\n");
        hasher.update(&bytes);
    }

    let digest = hasher.finalize();
    Ok(format!("{digest:x}"))
}

/// Recursively collect regular files under `dir`, storing each as
/// `(relative_path_from_root, absolute_path)`.
///
/// The relative path uses forward slashes on all platforms.
fn collect_files(
    root: &Utf8Path,
    dir: &Utf8Path,
    out: &mut Vec<(String, Utf8PathBuf)>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir.as_std_path())
        .with_context(|| format!("failed to read directory for hashing: {dir}"))?
    {
        let entry = entry.with_context(|| format!("failed to read entry in {dir}"))?;
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        // Skip macOS metadata files.
        if file_name_str == ".DS_Store" {
            continue;
        }

        let abs_path =
            Utf8PathBuf::from_path_buf(entry.path()).map_err(|p| {
                anyhow::anyhow!("non-UTF-8 path encountered during hashing: {}", p.display())
            })?;

        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to get file type for {}", abs_path))?;

        if file_type.is_dir() {
            collect_files(root, &abs_path, out)?;
        } else if file_type.is_file() {
            // Build a forward-slash relative path from root.
            let rel = abs_path
                .strip_prefix(root)
                .with_context(|| format!("path {abs_path} is not under root {root}"))?;
            // Replace OS path separators with forward slash for cross-platform stability.
            let rel_str = rel.as_str().replace('\\', "/");
            out.push((rel_str, abs_path));
        }
        // Symlinks: skip — symlinks in skill bundles are not expected.
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    // Helper: build a tempdir with a UTF-8 path we can work with.
    fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let utf8 = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .expect("tempdir path is valid UTF-8");
        (dir, utf8)
    }

    // Helper: create a skill directory with known contents and return its path.
    fn make_skill_dir(root: &Utf8Path) -> Utf8PathBuf {
        let skill_dir = root.join("skill");
        fs::create_dir_all(skill_dir.as_std_path()).unwrap();
        fs::write(skill_dir.join("manifest.json").as_std_path(), b"{}").unwrap();
        fs::write(skill_dir.join("workflow.yaml").as_std_path(), b"steps: []").unwrap();
        let prompts = skill_dir.join("prompts");
        fs::create_dir_all(prompts.as_std_path()).unwrap();
        fs::write(prompts.join("system.txt").as_std_path(), b"You are helpful.").unwrap();
        skill_dir
    }

    // ── verify_integrity tests ────────────────────────────────────────────────

    #[test]
    fn verify_integrity_matches() {
        let (_dir, root) = tempdir();
        let skill_dir = make_skill_dir(&root);

        // Compute the expected hash, then assert verification passes.
        let integrity = compute_integrity(&skill_dir).unwrap();
        assert!(integrity.starts_with("sha256-"), "integrity should start with sha256-");

        let result = verify_integrity(&skill_dir, &integrity).unwrap();
        assert!(result, "integrity should match when computed from same files");
    }

    #[test]
    fn verify_integrity_mismatch() {
        let (_dir, root) = tempdir();
        let skill_dir = make_skill_dir(&root);

        // A hash full of zeros will never match a real directory hash.
        let wrong = "sha256-0000000000000000000000000000000000000000000000000000000000000000";
        let result = verify_integrity(&skill_dir, wrong).unwrap();
        assert!(!result, "integrity should not match with wrong hash");
    }

    #[test]
    fn verify_integrity_empty_string_passes() {
        let (_dir, root) = tempdir();
        let skill_dir = make_skill_dir(&root);

        // Empty string = no integrity recorded; must return true for backwards compat.
        let result = verify_integrity(&skill_dir, "").unwrap();
        assert!(result, "empty integrity string should always pass");
    }

    #[test]
    fn verify_integrity_unsupported_prefix_errors() {
        let (_dir, root) = tempdir();
        let skill_dir = make_skill_dir(&root);

        let err = verify_integrity(&skill_dir, "md5-deadbeef").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported integrity format"),
            "error message should mention unsupported format: {msg}"
        );
    }

    #[test]
    fn hash_is_stable_across_calls() {
        let (_dir, root) = tempdir();
        let skill_dir = make_skill_dir(&root);

        let h1 = compute_integrity(&skill_dir).unwrap();
        let h2 = compute_integrity(&skill_dir).unwrap();
        assert_eq!(h1, h2, "repeated hashes of the same dir should be identical");
    }

    #[test]
    fn hash_changes_when_file_modified() {
        let (_dir, root) = tempdir();
        let skill_dir = make_skill_dir(&root);

        let before = compute_integrity(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("manifest.json").as_std_path(),
            b"{\"version\":\"2.0.0\"}",
        )
        .unwrap();
        let after = compute_integrity(&skill_dir).unwrap();
        assert_ne!(before, after, "hash should change when file content changes");
    }

    #[test]
    fn roundtrip_registry_and_local() {
        let (_dir, base) = tempdir();
        let lock_path = base.join(LOCKFILE_NAME);

        let mut lf = Lockfile::new();
        lf.upsert(
            "contract-compare".to_string(),
            LockedSkill::Registry {
                version: "1.2.0".to_string(),
                registry_url: "https://app.vectorhawk.ai".to_string(),
                integrity: "sha256-abc123".to_string(),
            },
        );
        lf.upsert(
            "my-custom-skill".to_string(),
            LockedSkill::Local {
                local_path: "./skills/my-custom-skill.md".to_string(),
            },
        );

        lf.save(&lock_path).expect("save");

        let loaded = Lockfile::load(&lock_path).expect("load");
        assert_eq!(lf, loaded);
    }

    #[test]
    fn discover_walks_up() {
        let (_dir, base) = tempdir();
        // Create a/b/c/
        let a = base.join("a");
        let b = a.join("b");
        let c = b.join("c");
        fs::create_dir_all(c.as_std_path()).expect("create dirs");

        // Place the lockfile in a/.vectorhawk/
        let vh_dir = a.join(VH_DIR);
        fs::create_dir_all(vh_dir.as_std_path()).expect("create .vectorhawk");
        let lock_in_a = vh_dir.join(LOCKFILE_NAME);
        Lockfile::new().save(&lock_in_a).expect("save lockfile in a");

        let found = Lockfile::discover(&c);
        assert_eq!(found, Some(lock_in_a));
    }

    #[test]
    fn discover_returns_none_at_root() {
        // Use a known directory that definitely has no .vectorhawk ancestor.
        // `/tmp` (or equivalent) should be safe for this assertion.
        let tmp = Utf8Path::new("/tmp");
        // Walk up from /tmp; there should be no .vectorhawk/skills.lock.json
        // between /tmp and / on any CI machine.
        let found = Lockfile::discover(tmp);
        // We can't guarantee /tmp has no ancestor with .vectorhawk, but we
        // can guarantee that / has no parent — so discover must terminate.
        // The real assertion here is "does not hang / panic"; we also assert
        // the type is Option<_>.
        let _: Option<Utf8PathBuf> = found;

        // Stronger test: start from a freshly created isolated tempdir.
        // Its only ancestors are OS-controlled dirs; no .vectorhawk will exist.
        let (_dir, isolated) = tempdir();
        // Go one level deeper to have at least one parent to walk.
        let child = isolated.join("sub");
        fs::create_dir_all(child.as_std_path()).expect("create sub");
        let result = Lockfile::discover(&child);
        assert!(
            result.is_none(),
            "expected None from isolated tempdir, got {result:?}"
        );
    }

    #[test]
    fn upsert_and_remove() {
        let mut lf = Lockfile::new();
        let entry_v1 = LockedSkill::Registry {
            version: "1.0.0".to_string(),
            registry_url: "https://app.vectorhawk.ai".to_string(),
            integrity: "sha256-aaa".to_string(),
        };
        let entry_v2 = LockedSkill::Registry {
            version: "2.0.0".to_string(),
            registry_url: "https://app.vectorhawk.ai".to_string(),
            integrity: "sha256-bbb".to_string(),
        };

        // Insert
        let prev = lf.upsert("my-skill".to_string(), entry_v1.clone());
        assert!(prev.is_none());
        assert_eq!(lf.skills.get("my-skill"), Some(&entry_v1));

        // Upsert (replace)
        let prev = lf.upsert("my-skill".to_string(), entry_v2.clone());
        assert_eq!(prev, Some(entry_v1));
        assert_eq!(lf.skills.get("my-skill"), Some(&entry_v2));

        // Remove
        let removed = lf.remove("my-skill");
        assert_eq!(removed, Some(entry_v2));
        assert!(lf.skills.get("my-skill").is_none());

        // Remove again → None
        assert!(lf.remove("my-skill").is_none());
    }

    #[test]
    fn json_format_matches_spec() {
        let mut lf = Lockfile::new();
        lf.upsert(
            "contract-compare".to_string(),
            LockedSkill::Registry {
                version: "1.2.0".to_string(),
                registry_url: "https://app.vectorhawk.ai".to_string(),
                integrity: "sha256-abc123...".to_string(),
            },
        );
        lf.upsert(
            "my-custom-skill".to_string(),
            LockedSkill::Local {
                local_path: "./skills/my-custom-skill.md".to_string(),
            },
        );

        let json_str = serde_json::to_string_pretty(&lf).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json_str).expect("parse back");

        assert_eq!(v["version"], serde_json::json!(1));

        let cc = &v["skills"]["contract-compare"];
        assert_eq!(cc["source"], "registry");
        assert_eq!(cc["version"], "1.2.0");
        assert_eq!(cc["registry_url"], "https://app.vectorhawk.ai");
        assert_eq!(cc["integrity"], "sha256-abc123...");

        let mc = &v["skills"]["my-custom-skill"];
        assert_eq!(mc["source"], "local");
        assert_eq!(mc["local_path"], "./skills/my-custom-skill.md");

        // Ensure no extra top-level keys beyond "version" and "skills"
        let top_keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        assert_eq!(top_keys.len(), 2, "unexpected top-level keys: {top_keys:?}");
    }
}
