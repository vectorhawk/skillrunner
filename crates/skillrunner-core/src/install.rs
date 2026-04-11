use crate::state::AppState;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use skillrunner_manifest::SkillPackage;
use std::fs;

pub fn install_unpacked_skill(state: &AppState, skill: &SkillPackage) -> Result<()> {
    let install_root = state.root_dir.join("skills").join(&skill.manifest.id);
    let versions_dir = install_root.join("versions");
    fs::create_dir_all(&versions_dir)?;

    let version_dir = versions_dir.join(skill.manifest.version.to_string());
    if version_dir.exists() {
        fs::remove_dir_all(&version_dir)?;
    }
    copy_dir_all::copy_dir_all(&skill.root, &version_dir)?;

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
        "INSERT OR REPLACE INTO skill_versions(skill_id, version, install_path, source_type) VALUES (?, ?, ?, 'local_dir')",
        params![
            skill.manifest.id,
            skill.manifest.version.to_string(),
            version_dir.as_str(),
        ],
    )?;

    Ok(())
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
        install_unpacked_skill(&state, &skill).expect("install should succeed");

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

    /// Helper: load and install the contract-compare example skill into `state`.
    fn install_example_skill(state: &AppState) -> String {
        let skill = SkillPackage::load_from_dir(
            Utf8PathBuf::from("../..").join("examples/skills/contract-compare"),
        )
        .expect("example skill should load");
        let version = skill.manifest.version.to_string();
        install_unpacked_skill(state, &skill).expect("install should succeed");
        version
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
}
