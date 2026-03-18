use anyhow::Result;
use camino::Utf8PathBuf;
use directories::ProjectDirs;
use rusqlite::Connection;
use std::fs;

pub struct AppState {
    pub root_dir: Utf8PathBuf,
    pub db_path: Utf8PathBuf,
}

impl AppState {
    pub fn bootstrap() -> Result<Self> {
        let dirs = ProjectDirs::from("ai", "SkillClub", "SkillRunner")
            .ok_or_else(|| anyhow::anyhow!("failed to resolve application directories"))?;

        let root_dir = Utf8PathBuf::from_path_buf(dirs.data_dir().to_path_buf())
            .map_err(|_| anyhow::anyhow!("non-utf8 app data dir"))?;

        Self::bootstrap_in(root_dir)
    }

    pub fn bootstrap_in(root_dir: Utf8PathBuf) -> Result<Self> {
        fs::create_dir_all(root_dir.join("skills"))?;
        fs::create_dir_all(root_dir.join("cache"))?;
        fs::create_dir_all(root_dir.join("logs"))?;
        fs::create_dir_all(root_dir.join("policy"))?;
        fs::create_dir_all(root_dir.join("tmp"))?;

        let db_path = root_dir.join("state.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS installed_skills (
                skill_id TEXT PRIMARY KEY,
                active_version TEXT NOT NULL,
                install_root TEXT NOT NULL,
                channel TEXT,
                current_status TEXT NOT NULL DEFAULT 'active',
                installed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS skill_versions (
                skill_id TEXT NOT NULL,
                version TEXT NOT NULL,
                install_path TEXT NOT NULL,
                source_type TEXT NOT NULL,
                installed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(skill_id, version)
            );

            CREATE TABLE IF NOT EXISTS policy_cache (
                skill_id TEXT PRIMARY KEY,
                policy_json TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                fetched_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS auth_tokens (
                registry_url TEXT PRIMARY KEY,
                access_token TEXT NOT NULL,
                refresh_token TEXT NOT NULL,
                saved_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS execution_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                skill_id TEXT NOT NULL,
                version TEXT NOT NULL,
                status TEXT NOT NULL,
                prompt_tokens INTEGER,
                completion_tokens INTEGER,
                latency_ms INTEGER,
                executed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            "#,
        )?;

        Ok(Self { root_dir, db_path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(test_name: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("forge-tests-{test_name}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temporary test path should be utf-8")
    }

    fn cleanup(path: &Utf8Path) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn bootstrap_in_creates_expected_directories_and_tables() {
        let root = temp_root("bootstrap");
        let state = AppState::bootstrap_in(root.clone()).expect("state bootstrap should succeed");

        assert_eq!(state.root_dir, root);
        assert!(state.root_dir.join("skills").exists());
        assert!(state.root_dir.join("cache").exists());
        assert!(state.root_dir.join("logs").exists());
        assert!(state.root_dir.join("policy").exists());
        assert!(state.root_dir.join("tmp").exists());
        assert!(state.db_path.exists());

        let conn = Connection::open(&state.db_path).expect("state db should open");
        let installed_skills_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'installed_skills'",
                [],
                |row| row.get(0),
            )
            .expect("installed_skills table should be queryable");
        let skill_versions_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'skill_versions'",
                [],
                |row| row.get(0),
            )
            .expect("skill_versions table should be queryable");

        assert_eq!(installed_skills_exists, 1);
        assert_eq!(skill_versions_exists, 1);

        cleanup(&state.root_dir);
    }
}
