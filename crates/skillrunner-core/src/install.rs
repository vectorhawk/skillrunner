use crate::lockfile::{compute_integrity, LockedSkill, Lockfile};
use crate::state::AppState;
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{params, Connection, OptionalExtension};
use skillrunner_manifest::SkillPackage;
use std::fs;

/// Controls how a skill source directory is placed into the versioned install layout.
#[derive(Clone, Copy, Debug)]
pub enum InstallMode {
    /// Copy the source directory into `versions/{ver}/` (default, used by registry installs).
    Copy,
    /// Make `versions/{ver}/` itself a symlink pointing at the source directory.
    /// Changes to the source directory are immediately visible through `active/`.
    /// Only supported on Unix; returns an error on other platforms.
    Symlink,
}

pub fn install_unpacked_skill(
    state: &AppState,
    skill: &SkillPackage,
    mode: InstallMode,
) -> Result<()> {
    let install_root = state.root_dir.join("skills").join(&skill.manifest.id);
    let versions_dir = install_root.join("versions");
    fs::create_dir_all(&versions_dir)?;

    let version_dir = versions_dir.join(skill.manifest.version.to_string());

    // Place the skill content according to the requested mode.
    let source_type = install_with_mode(&skill.root, &version_dir, mode)?;

    let active_dir = install_root.join("active");
    if active_dir.exists() {
        fs::remove_file(&active_dir)
            .or_else(|_| fs::remove_dir_all(&active_dir))
            .ok();
    }
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(&version_dir, &active_dir)?;

    let conn = Connection::open(&state.db_path)?;
    conn.execute(
        "INSERT OR REPLACE INTO installed_skills(skill_id, active_version, install_root, channel, current_status) VALUES (?, ?, ?, ?, 'active')",
        params![
            skill.manifest.id,
            skill.manifest.version.to_string(),
            install_root.as_str(),
            skill.manifest.update.as_ref().and_then(|u| u.channel.clone()).unwrap_or_else(|| "stable".to_string())
        ],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO skill_versions(skill_id, version, install_path, source_type) VALUES (?, ?, ?, ?)",
        params![
            skill.manifest.id,
            skill.manifest.version.to_string(),
            version_dir.as_str(),
            source_type,
        ],
    )?;

    Ok(())
}

/// Perform the file-system placement for one version slot, returning the
/// `source_type` string to record in `skill_versions`.
fn install_with_mode(
    source: &camino::Utf8Path,
    version_dir: &camino::Utf8Path,
    mode: InstallMode,
) -> Result<&'static str> {
    match mode {
        InstallMode::Copy => {
            if version_dir.exists() {
                fs::remove_dir_all(version_dir)
                    .with_context(|| format!("failed to remove existing {version_dir}"))?;
            }
            copy_dir_all::copy_dir_all(source, version_dir)
                .with_context(|| format!("failed to copy skill into {version_dir}"))?;
            Ok("local_dir")
        }
        InstallMode::Symlink => {
            symlink_version_dir(source, version_dir)?;
            Ok("local_symlink")
        }
    }
}

/// Create `version_dir` as a symlink pointing at the canonical (absolute) path
/// of `source`.
///
/// Using an absolute target ensures the symlink remains valid regardless of
/// the working directory at resolution time.
///
/// Only available on Unix. On other platforms this always returns an error.
fn symlink_version_dir(
    source: &camino::Utf8Path,
    version_dir: &camino::Utf8Path,
) -> Result<()> {
    #[cfg(target_family = "unix")]
    {
        // Resolve to an absolute path so the symlink target is stable.
        let abs_source = std::fs::canonicalize(source)
            .with_context(|| format!("failed to canonicalize source path {source}"))?;

        // Remove a pre-existing entry so the symlink placement is idempotent.
        if version_dir.exists() || version_dir.is_symlink() {
            fs::remove_file(version_dir)
                .or_else(|_| fs::remove_dir_all(version_dir))
                .with_context(|| format!("failed to remove existing {version_dir}"))?;
        }
        std::os::unix::fs::symlink(&abs_source, version_dir)
            .with_context(|| {
                format!(
                    "failed to create symlink {} -> {}",
                    version_dir,
                    abs_source.display()
                )
            })?;
        Ok(())
    }
    #[cfg(not(target_family = "unix"))]
    {
        let _ = (source, version_dir);
        Err(anyhow::anyhow!(
            "--link (Symlink install mode) is only supported on Unix; \
             use the default copy mode on this platform"
        ))
    }
}

/// Uninstall a skill completely.
///
/// Removes the `active` symlink, the entire `skills/{skill_id}/` directory tree,
/// and deletes all rows from `installed_skills` and `skill_versions`.
///
/// Returns `Ok(Some(version))` with the previously active version string, or
/// `Ok(None)` if the skill was not installed.
pub fn uninstall_skill(state: &AppState, skill_id: &str) -> Result<Option<String>> {
    let conn = Connection::open(&state.db_path)?;

    // Look up the install_root and active_version
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT install_root, active_version FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (install_root, active_version) = match row {
        Some(r) => r,
        None => return Ok(None),
    };

    // Remove the active symlink first (best-effort)
    let active_path = std::path::Path::new(&install_root).join("active");
    if active_path.exists() || active_path.is_symlink() {
        fs::remove_file(&active_path)
            .or_else(|_| fs::remove_dir_all(&active_path))
            .ok();
    }

    // Remove the entire skill directory tree
    let skill_dir = std::path::Path::new(&install_root);
    if skill_dir.exists() {
        fs::remove_dir_all(skill_dir)?;
    }

    // Remove DB records
    conn.execute("DELETE FROM skill_versions WHERE skill_id = ?1", [skill_id])?;
    conn.execute(
        "DELETE FROM installed_skills WHERE skill_id = ?1",
        [skill_id],
    )?;

    Ok(Some(active_version))
}

/// Deactivate an installed skill.
///
/// Sets `current_status = 'deactivated'` in `installed_skills` and removes
/// the `active` symlink (versioned files are kept intact).
///
/// Returns `true` if the skill was active and has been deactivated,
/// `false` if the skill was not found or was already deactivated.
pub fn deactivate_skill(state: &AppState, skill_id: &str) -> Result<bool> {
    let conn = Connection::open(&state.db_path)?;

    // Only deactivate if currently active
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT install_root, current_status FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (install_root, current_status) = match row {
        Some(r) => r,
        None => return Ok(false),
    };

    if current_status != "active" {
        return Ok(false);
    }

    // Remove the active symlink (keep versioned files)
    let active_path = std::path::Path::new(&install_root).join("active");
    if active_path.exists() || active_path.is_symlink() {
        fs::remove_file(&active_path)
            .or_else(|_| fs::remove_dir_all(&active_path))
            .ok();
    }

    // Update status in DB
    conn.execute(
        "UPDATE installed_skills SET current_status = 'deactivated' WHERE skill_id = ?1",
        [skill_id],
    )?;

    Ok(true)
}

/// Reactivate a deactivated skill.
///
/// Finds the latest version directory under `skills/{skill_id}/versions/`,
/// restores the `active` symlink pointing to it, and sets
/// `current_status = 'active'` in `installed_skills`.
///
/// Returns `true` if the skill was deactivated and has been reactivated,
/// `false` if the skill was not found or was not deactivated.
pub fn reactivate_skill(state: &AppState, skill_id: &str) -> Result<bool> {
    let conn = Connection::open(&state.db_path)?;

    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT install_root, active_version, current_status FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    let (install_root, active_version, current_status) = match row {
        Some(r) => r,
        None => return Ok(false),
    };

    if current_status != "deactivated" {
        return Ok(false);
    }

    // Find the version directory to point the symlink at.
    // Prefer the recorded active_version; fall back to latest on disk.
    let versions_dir = std::path::Path::new(&install_root).join("versions");
    let target_version_dir = versions_dir.join(&active_version);

    let version_dir = if target_version_dir.exists() {
        target_version_dir
    } else {
        // Fall back: pick the lexicographically largest version directory
        let mut entries: Vec<std::path::PathBuf> = fs::read_dir(&versions_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        entries.sort();
        match entries.into_iter().last() {
            Some(p) => p,
            None => anyhow::bail!("no version directories found for skill '{skill_id}'"),
        }
    };

    // Restore active symlink
    let active_path = std::path::Path::new(&install_root).join("active");
    if active_path.exists() || active_path.is_symlink() {
        fs::remove_file(&active_path)
            .or_else(|_| fs::remove_dir_all(&active_path))
            .ok();
    }
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(&version_dir, &active_path)?;

    // Update status in DB
    conn.execute(
        "UPDATE installed_skills SET current_status = 'active' WHERE skill_id = ?1",
        [skill_id],
    )?;

    Ok(true)
}

// ── Project-scope install ─────────────────────────────────────────────────────

/// Where an installed skill lives: in the global user store, or local to a project.
///
/// `Project` carries the path to the project root — the directory that contains
/// (or will contain) `.vectorhawk/`.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallScope {
    /// Global user install: `~/Library/Application Support/SkillClub/SkillRunner/skills/…`
    User,
    /// Project-local install: `{project_root}/.vectorhawk/skills/{id}/`
    Project(Utf8PathBuf),
}

/// The hidden directory name inside a project root.
const VH_DIR: &str = ".vectorhawk";

/// Install a skill bundle into the **project** scope.
///
/// Creates `.vectorhawk/skills/{id}/` under `project_root`, copies the bundle,
/// upserts the lockfile, and auto-generates `.vectorhawk/.gitignore` on first use.
///
/// # Arguments
///
/// - `project_root` — directory that owns (or will own) `.vectorhawk/`
/// - `skill` — the loaded, validated skill package to install
/// - `registry_url` — if `Some`, records a `Registry` lockfile entry; if `None`,
///   records a `Local` entry with a path relative to `project_root`
/// - `integrity` — SHA-256 integrity string (e.g. `"sha256-abc123"`); only used
///   when `registry_url` is `Some`
///
/// Returns the path to the installed skill directory.
pub fn install_project_skill(
    project_root: &Utf8Path,
    skill: &SkillPackage,
    registry_url: Option<&str>,
    // The caller-supplied integrity is superseded: we always compute a fresh
    // hash from the files written to disk and record that in the lockfile.
    // This parameter is retained for API stability.
    _integrity: Option<&str>,
) -> Result<Utf8PathBuf> {
    let vh_dir = project_root.join(VH_DIR);
    let skills_cache = vh_dir.join("skills");
    let skill_dir = skills_cache.join(&skill.manifest.id);

    // 1. Create the skill directory under the project cache.
    fs::create_dir_all(skill_dir.as_std_path())
        .with_context(|| format!("failed to create project skill dir at {skill_dir}"))?;

    // 2. Copy the bundle into the skill directory (always Copy, never Symlink).
    //    Remove any pre-existing content so re-installs are idempotent.
    if skill_dir.exists() {
        fs::remove_dir_all(skill_dir.as_std_path())
            .with_context(|| format!("failed to clear existing project skill dir at {skill_dir}"))?;
    }
    copy_dir_all::copy_dir_all(skill.root.as_std_path(), skill_dir.as_std_path())
        .with_context(|| {
            format!(
                "failed to copy skill '{}' into project cache at {skill_dir}",
                skill.manifest.id
            )
        })?;

    // 3. Compute the integrity hash from the files that were just written to disk.
    //    This overrides any caller-supplied `integrity` so the lockfile always
    //    records the hash of what is actually present in the cache.
    let computed_integrity = compute_integrity(&skill_dir)
        .with_context(|| {
            format!(
                "failed to compute integrity for skill '{}' at {skill_dir}",
                skill.manifest.id
            )
        })?;

    // 4. Load or create the lockfile.
    let lockfile_path = vh_dir.join("skills.lock.json");
    let mut lockfile = if lockfile_path.exists() {
        Lockfile::load(&lockfile_path)
            .with_context(|| format!("failed to load lockfile at {lockfile_path}"))?
    } else {
        Lockfile::new()
    };

    // 5. Build the lockfile entry using the freshly computed integrity.
    let locked_entry = build_locked_skill(project_root, skill, registry_url, Some(&computed_integrity))?;

    // 6. Upsert and save atomically.
    lockfile.upsert(skill.manifest.id.clone(), locked_entry);
    lockfile
        .save(&lockfile_path)
        .with_context(|| format!("failed to save lockfile at {lockfile_path}"))?;

    // 7. Auto-generate `.vectorhawk/.gitignore` on first project install.
    ensure_gitignore(&vh_dir)?;

    Ok(skill_dir)
}

/// Compute a relative path from `base` to `target`, returning a forward-slash
/// string suitable for cross-platform lockfile storage.
///
/// Returns `None` if the paths share no common prefix (e.g. different Windows
/// drive letters), in which case the caller should fall back to the absolute path.
fn relative_utf8_path(base: &Utf8Path, target: &Utf8Path) -> Option<String> {
    // Walk both paths to find the common prefix length.
    let base_comps: Vec<_> = base.components().collect();
    let target_comps: Vec<_> = target.components().collect();

    // If no common prefix at all, relativisation is not possible.
    let common_len = base_comps
        .iter()
        .zip(target_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();

    if common_len == 0 {
        return None;
    }

    // Number of `..` steps needed to back up from `base` to the common ancestor.
    let up_steps = base_comps.len() - common_len;
    // Remaining components of `target` after the common prefix.
    let down_comps = &target_comps[common_len..];

    let mut parts: Vec<String> = Vec::new();
    for _ in 0..up_steps {
        parts.push("..".to_string());
    }
    for comp in down_comps {
        parts.push(comp.as_str().replace('\\', "/"));
    }

    Some(parts.join("/"))
}

/// Build the `LockedSkill` entry for a skill install.
fn build_locked_skill(
    project_root: &Utf8Path,
    skill: &SkillPackage,
    registry_url: Option<&str>,
    integrity: Option<&str>,
) -> Result<LockedSkill> {
    match registry_url {
        Some(url) => Ok(LockedSkill::Registry {
            version: skill.manifest.version.to_string(),
            registry_url: url.to_string(),
            integrity: integrity.unwrap_or("").to_string(),
        }),
        None => {
            // Compute a path relative from the project root to the skill source.
            // Fall back to the absolute source path string if relativisation
            // is not possible (e.g. different Windows drive letters).
            let local_path = relative_utf8_path(project_root, &skill.root)
                .unwrap_or_else(|| skill.root.as_str().replace('\\', "/"));
            Ok(LockedSkill::Local { local_path })
        }
    }
}

/// Write `.vectorhawk/.gitignore` with a `skills/` entry if it doesn't exist.
fn ensure_gitignore(vh_dir: &Utf8Path) -> Result<()> {
    let gitignore_path = vh_dir.join(".gitignore");
    if !gitignore_path.exists() {
        fs::write(gitignore_path.as_std_path(), "skills/\n")
            .with_context(|| format!("failed to write .gitignore at {gitignore_path}"))?;
    }
    Ok(())
}

mod copy_dir_all {
    use std::fs;
    use std::io;
    use std::path::Path;

    pub fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
        fs::create_dir_all(&dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
            } else {
                fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use camino::Utf8PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(test_name: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("forge-tests-{test_name}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temporary test path should be utf-8")
    }

    #[test]
    fn install_unpacked_skill_copies_files_and_records_metadata() {
        let root = temp_root("install");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        let skill = SkillPackage::load_from_dir(
            Utf8PathBuf::from("../..").join("examples/skills/contract-compare"),
        )
        .expect("example skill should load");

        let version = skill.manifest.version.to_string();
        install_unpacked_skill(&state, &skill, InstallMode::Copy)
            .expect("install should succeed");

        let install_root = state.root_dir.join("skills").join("contract-compare");
        let version_dir = install_root.join("versions").join(&version);
        assert!(version_dir.join("SKILL.md").exists());
        assert!(version_dir.join("workflow.yaml").exists());

        #[cfg(target_family = "unix")]
        {
            let active_dir = install_root.join("active");
            assert!(active_dir.exists());
            let symlink_target = fs::read_link(&active_dir).expect("active symlink should exist");
            assert_eq!(symlink_target, version_dir.as_std_path());
        }

        let conn = Connection::open(&state.db_path).expect("state db should open");
        let installed_row: (String, String, String) = conn
            .query_row(
                "SELECT skill_id, active_version, current_status FROM installed_skills WHERE skill_id = ?1",
                ["contract-compare"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("installed skill row should exist");
        let version_row: (String, String, String) = conn
            .query_row(
                "SELECT skill_id, version, source_type FROM skill_versions WHERE skill_id = ?1 AND version = ?2",
                rusqlite::params!["contract-compare", &version],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("skill version row should exist");

        assert_eq!(
            installed_row,
            (
                "contract-compare".to_string(),
                version.clone(),
                "active".to_string()
            )
        );
        assert_eq!(
            version_row,
            (
                "contract-compare".to_string(),
                version.clone(),
                "local_dir".to_string()
            )
        );

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    /// Helper: load and install the contract-compare example skill into `state` using Copy mode.
    fn install_example_skill(state: &AppState) -> String {
        let skill = SkillPackage::load_from_dir(
            Utf8PathBuf::from("../..").join("examples/skills/contract-compare"),
        )
        .expect("example skill should load");
        let version = skill.manifest.version.to_string();
        install_unpacked_skill(state, &skill, InstallMode::Copy).expect("install should succeed");
        version
    }

    /// Verify that Symlink mode makes `versions/{ver}/` itself a symlink to the
    /// source dir, so edits to the source are visible through `active/`.
    #[cfg(target_family = "unix")]
    #[test]
    fn install_symlink_mode_links_source_dir() {
        use std::io::Write;

        let root = temp_root("symlink-install");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");

        // Load the example skill so we have a valid SkillPackage.
        let skill = SkillPackage::load_from_dir(
            Utf8PathBuf::from("../..").join("examples/skills/contract-compare"),
        )
        .expect("example skill should load");
        let version = skill.manifest.version.to_string();
        let source_dir = skill.root.clone();

        install_unpacked_skill(&state, &skill, InstallMode::Symlink)
            .expect("symlink install should succeed");

        let install_root = state.root_dir.join("skills").join("contract-compare");

        // `versions/{ver}/` must be a symlink pointing at the source directory.
        let version_dir = install_root.join("versions").join(&version);
        assert!(
            version_dir.is_symlink(),
            "versions/{version} should be a symlink in Symlink mode"
        );
        let symlink_target =
            fs::read_link(version_dir.as_std_path()).expect("symlink target should be readable");
        let canonical_source = fs::canonicalize(source_dir.as_std_path())
            .expect("source dir should canonicalize");
        assert_eq!(
            symlink_target,
            canonical_source,
            "symlink should point at the canonical skill source directory"
        );

        // `active/` points at `versions/{ver}/` as normal.
        let active_dir = install_root.join("active");
        assert!(active_dir.exists(), "active symlink should exist");

        // Writes to a file in the source dir are visible through `active/`.
        let probe_path = source_dir.join("__symlink_probe__.txt");
        {
            let mut f = std::fs::File::create(probe_path.as_std_path())
                .expect("probe file creation should succeed");
            f.write_all(b"hello symlink").expect("probe write should succeed");
        }
        let through_active = active_dir.join("__symlink_probe__.txt");
        assert!(
            through_active.exists(),
            "file written into source dir should be visible through active/"
        );
        let content = fs::read_to_string(through_active.as_std_path())
            .expect("reading through active/ should succeed");
        assert_eq!(content, "hello symlink");

        // Clean up probe file so we don't dirty the example skill.
        let _ = fs::remove_file(probe_path.as_std_path());

        // DB row must record source_type = 'local_symlink'.
        let conn = Connection::open(&state.db_path).expect("db should open");
        let source_type: String = conn
            .query_row(
                "SELECT source_type FROM skill_versions WHERE skill_id = ?1 AND version = ?2",
                rusqlite::params!["contract-compare", &version],
                |row| row.get(0),
            )
            .expect("skill_versions row should exist");
        assert_eq!(
            source_type, "local_symlink",
            "skill_versions.source_type should be 'local_symlink'"
        );

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_uninstall_removes_files_and_db_records() {
        let root = temp_root("uninstall");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        let version = install_example_skill(&state);

        let install_root = state.root_dir.join("skills").join("contract-compare");
        assert!(
            install_root.exists(),
            "install dir should exist before uninstall"
        );

        let result = uninstall_skill(&state, "contract-compare").expect("uninstall should succeed");
        assert_eq!(result, Some(version), "should return the active version");

        // Files should be gone
        assert!(!install_root.exists(), "install dir should be removed");

        // DB records should be gone
        let conn = Connection::open(&state.db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM installed_skills WHERE skill_id = 'contract-compare'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "installed_skills row should be deleted");

        let ver_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM skill_versions WHERE skill_id = 'contract-compare'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ver_count, 0, "skill_versions rows should be deleted");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_uninstall_nonexistent_skill_returns_none() {
        let root = temp_root("uninstall-none");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");

        let result = uninstall_skill(&state, "ghost-skill").expect("uninstall should not error");
        assert_eq!(result, None, "should return None for uninstalled skill");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_deactivate_skill_updates_status_and_removes_symlink() {
        let root = temp_root("deactivate");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        install_example_skill(&state);

        let install_root = state.root_dir.join("skills").join("contract-compare");
        let active_path = install_root.join("active");

        #[cfg(target_family = "unix")]
        assert!(
            active_path.exists(),
            "active symlink should exist before deactivate"
        );

        let changed =
            deactivate_skill(&state, "contract-compare").expect("deactivate should succeed");
        assert!(
            changed,
            "should return true when deactivating an active skill"
        );

        // Symlink should be removed
        #[cfg(target_family = "unix")]
        assert!(
            !active_path.exists() && !active_path.is_symlink(),
            "active symlink should be removed"
        );

        // Versioned files should still exist
        let conn = Connection::open(&state.db_path).unwrap();
        let status: String = conn
            .query_row(
                "SELECT current_status FROM installed_skills WHERE skill_id = 'contract-compare'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "deactivated");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_reactivate_skill_restores_symlink_and_status() {
        let root = temp_root("reactivate");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        install_example_skill(&state);

        // Deactivate first
        deactivate_skill(&state, "contract-compare").expect("deactivate should succeed");

        // Now reactivate
        let changed =
            reactivate_skill(&state, "contract-compare").expect("reactivate should succeed");
        assert!(
            changed,
            "should return true when reactivating a deactivated skill"
        );

        let install_root = state.root_dir.join("skills").join("contract-compare");
        let active_path = install_root.join("active");

        #[cfg(target_family = "unix")]
        assert!(active_path.exists(), "active symlink should be restored");

        let conn = Connection::open(&state.db_path).unwrap();
        let status: String = conn
            .query_row(
                "SELECT current_status FROM installed_skills WHERE skill_id = 'contract-compare'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_deactivate_already_deactivated_returns_false() {
        let root = temp_root("deactivate-idempotent");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");
        install_example_skill(&state);

        deactivate_skill(&state, "contract-compare").expect("first deactivate should succeed");
        let changed = deactivate_skill(&state, "contract-compare")
            .expect("second deactivate should not error");
        assert!(!changed, "should return false when already deactivated");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    #[test]
    fn test_reactivate_nonexistent_skill_returns_false() {
        let root = temp_root("reactivate-none");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");

        let changed = reactivate_skill(&state, "ghost-skill").expect("reactivate should not error");
        assert!(!changed, "should return false for non-existent skill");

        let _ = fs::remove_dir_all(&state.root_dir);
    }

    // ── install_project_skill tests ───────────────────────────────────────────

    fn load_example_package() -> SkillPackage {
        SkillPackage::load_from_dir(
            Utf8PathBuf::from("../..").join("examples/skills/contract-compare"),
        )
        .expect("example skill should load")
    }

    #[test]
    fn install_project_skill_creates_bundle_and_lockfile() {
        let project_root = temp_root("proj-install-basic");
        let skill = load_example_package();
        let skill_id = skill.manifest.id.clone();

        install_project_skill(&project_root, &skill, None, None)
            .expect("project install should succeed");

        // Bundle must be present under .vectorhawk/skills/{id}/
        // The example skill uses SKILL.md as its root file.
        let skill_dir = project_root.join(".vectorhawk/skills").join(&skill_id);
        assert!(
            skill_dir.join("SKILL.md").exists() || skill_dir.join("manifest.json").exists(),
            "skill bundle root file should be in the project cache (SKILL.md or manifest.json)"
        );

        // Lockfile must exist.
        let lockfile_path = project_root.join(".vectorhawk/skills.lock.json");
        assert!(lockfile_path.exists(), "lockfile must be created");
        let lf = Lockfile::load(&lockfile_path).expect("lockfile must be valid");
        assert!(
            lf.skills.contains_key(&skill_id),
            "lockfile must contain the installed skill"
        );

        // .gitignore must exist with skills/ entry.
        let gitignore_path = project_root.join(".vectorhawk/.gitignore");
        assert!(gitignore_path.exists(), ".gitignore must be created");
        let contents =
            fs::read_to_string(gitignore_path.as_std_path()).expect("read .gitignore");
        assert!(
            contents.contains("skills/"),
            ".gitignore must contain skills/ entry"
        );

        let _ = fs::remove_dir_all(project_root.as_std_path());
    }

    #[test]
    fn install_project_skill_upserts_lockfile() {
        let project_root = temp_root("proj-install-upsert");

        // Install first skill (contract-compare).
        let skill_a = load_example_package();
        let id_a = skill_a.manifest.id.clone();
        install_project_skill(&project_root, &skill_a, None, None)
            .expect("first project install should succeed");

        // Build a second minimal SkillPackage by pointing at the same bundle
        // but under a different id — we do this by cloning and patching.
        // For this test we load the same package a second time; the lockfile
        // upsert test just needs two keys so we install the same skill twice
        // with different registry_url values to distinguish them.
        //
        // Actually: install skill_a again with registry_url to change the entry,
        // then verify the lockfile has exactly one key (upserted, not duplicated).
        install_project_skill(
            &project_root,
            &skill_a,
            Some("https://app.vectorhawk.ai"),
            Some("sha256-abc"),
        )
        .expect("second project install should succeed");

        let lockfile_path = project_root.join(".vectorhawk/skills.lock.json");
        let lf = Lockfile::load(&lockfile_path).expect("lockfile must load");
        assert_eq!(
            lf.skills.len(),
            1,
            "upsert should leave exactly one entry, not duplicate"
        );
        // The entry must now be a Registry entry (the second install).
        match lf.skills.get(&id_a) {
            Some(LockedSkill::Registry { registry_url, .. }) => {
                assert_eq!(registry_url, "https://app.vectorhawk.ai");
            }
            other => panic!("expected Registry entry, got {other:?}"),
        }

        let _ = fs::remove_dir_all(project_root.as_std_path());
    }

    #[test]
    fn install_project_skill_registry_entry() {
        let project_root = temp_root("proj-install-registry");
        let skill = load_example_package();
        let skill_id = skill.manifest.id.clone();

        install_project_skill(
            &project_root,
            &skill,
            Some("https://app.vectorhawk.ai"),
            // Caller-supplied integrity is ignored; the fresh hash is computed
            // from what was actually written to disk.
            Some("sha256-deadbeef"),
        )
        .expect("registry project install should succeed");

        let lockfile_path = project_root.join(".vectorhawk/skills.lock.json");
        let lf = Lockfile::load(&lockfile_path).expect("lockfile must load");
        match lf.skills.get(&skill_id) {
            Some(LockedSkill::Registry {
                version,
                registry_url,
                integrity,
            }) => {
                assert_eq!(version, &skill.manifest.version.to_string());
                assert_eq!(registry_url, "https://app.vectorhawk.ai");
                // Integrity is computed from the files on disk, not the
                // caller-supplied value. Verify it is a valid sha256 string
                // and matches the actual installed directory.
                assert!(
                    integrity.starts_with("sha256-"),
                    "integrity should start with 'sha256-': {integrity}"
                );
                let cache_dir = project_root
                    .join(".vectorhawk")
                    .join("skills")
                    .join(&skill_id);
                let matches = crate::lockfile::verify_integrity(&cache_dir, integrity)
                    .expect("verify_integrity should not error");
                assert!(
                    matches,
                    "recorded integrity should verify against the installed cache"
                );
            }
            other => panic!("expected Registry variant, got {other:?}"),
        }

        let _ = fs::remove_dir_all(project_root.as_std_path());
    }

    #[test]
    fn install_project_skill_local_entry() {
        let project_root = temp_root("proj-install-local");
        let skill = load_example_package();
        let skill_id = skill.manifest.id.clone();

        install_project_skill(&project_root, &skill, None, None)
            .expect("local project install should succeed");

        let lockfile_path = project_root.join(".vectorhawk/skills.lock.json");
        let lf = Lockfile::load(&lockfile_path).expect("lockfile must load");
        match lf.skills.get(&skill_id) {
            Some(LockedSkill::Local { local_path }) => {
                // Must be a relative or absolute path pointing at the skill source.
                // At minimum it should not be empty.
                assert!(
                    !local_path.is_empty(),
                    "local_path must not be empty"
                );
            }
            other => panic!("expected Local variant, got {other:?}"),
        }

        let _ = fs::remove_dir_all(project_root.as_std_path());
    }
}
