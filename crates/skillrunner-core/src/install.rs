use crate::state::AppState;
use anyhow::Result;
use skillrunner_manifest::SkillPackage;
use rusqlite::{params, Connection};
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

        install_unpacked_skill(&state, &skill).expect("install should succeed");

        let install_root = state.root_dir.join("skills").join("contract-compare");
        let version_dir = install_root.join("versions").join("0.2.0");
        assert!(version_dir.join("manifest.json").exists());
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
                ["contract-compare", "0.2.0"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("skill version row should exist");

        assert_eq!(
            installed_row,
            (
                "contract-compare".to_string(),
                "0.2.0".to_string(),
                "active".to_string()
            )
        );
        assert_eq!(
            version_row,
            (
                "contract-compare".to_string(),
                "0.2.0".to_string(),
                "local_dir".to_string()
            )
        );

        let _ = fs::remove_dir_all(&state.root_dir);
    }
}
