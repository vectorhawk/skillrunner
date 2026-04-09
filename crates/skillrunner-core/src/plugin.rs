use crate::install::{install_unpacked_skill, uninstall_skill};
use crate::state::AppState;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use skillrunner_manifest::{PluginManifest, PluginPackage, SkillPackage};
use std::fs;
use tracing::info;

/// Summary of an installed plugin.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InstalledPlugin {
    pub id: String,
    pub version: String,
    pub manifest: PluginManifest,
    pub components: PluginComponents,
    pub status: String,
    pub installed_at: String,
}

/// Tracks which components were installed by a plugin.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginComponents {
    pub skill_ids: Vec<String>,
    pub mcp_server_names: Vec<String>,
    pub command_names: Vec<String>,
}

/// Install a plugin from a local directory.
///
/// 1. Loads and validates the plugin bundle
/// 2. Installs embedded skills via the existing installer
/// 3. Records MCP server names (actual server request happens at the MCP tool layer)
/// 4. Writes slash command files to ~/.claude/skills/
/// 5. Records plugin state in SQLite
pub fn install_plugin_from_dir(
    state: &AppState,
    plugin_dir: &camino::Utf8Path,
) -> Result<InstalledPlugin> {
    let pkg = PluginPackage::load_from_dir(plugin_dir)
        .with_context(|| format!("failed to load plugin from {plugin_dir}"))?;

    let manifest = &pkg.manifest;
    info!(plugin_id = %manifest.id, version = %manifest.version, "installing plugin");

    let mut components = PluginComponents {
        skill_ids: Vec::new(),
        mcp_server_names: Vec::new(),
        command_names: Vec::new(),
    };

    // 1. Install embedded skills
    for skill_ref in &manifest.skills {
        if let Some(path) = &skill_ref.path {
            let skill_dir = pkg.root.join(path);
            let skill_pkg = SkillPackage::load_from_dir(&skill_dir)
                .with_context(|| format!("failed to load embedded skill at {path}"))?;
            let skill_id = skill_pkg.manifest.id.clone();
            install_unpacked_skill(state, &skill_pkg)
                .with_context(|| format!("failed to install embedded skill '{skill_id}'"))?;
            info!(skill_id, "installed embedded skill");
            components.skill_ids.push(skill_id);
        }
        // registry_id skills: record the ID for later resolution at the MCP tool layer
        if let Some(registry_id) = &skill_ref.registry_id {
            components.skill_ids.push(registry_id.clone());
        }
    }

    // 2. Record MCP server names (actual request/install happens via MCP tools)
    for server in &manifest.mcp_servers {
        components.mcp_server_names.push(server.name.clone());
    }

    // 3. Write slash command files to ~/.claude/skills/
    if let Some(home) = std::env::var("HOME").ok().map(std::path::PathBuf::from) {
        let skills_dir = home.join(".claude").join("skills");
        for cmd in &manifest.commands {
            let cmd_path = pkg.root.join(&cmd.path);
            if let Ok(content) = fs::read_to_string(&cmd_path) {
                // Derive command name from filename (e.g. "my-command.md" -> "my-command")
                let cmd_name = std::path::Path::new(&cmd.path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                let target_dir = skills_dir.join(cmd_name);
                fs::create_dir_all(&target_dir)?;
                fs::write(target_dir.join("SKILL.md"), &content)?;
                info!(command = cmd_name, "installed slash command");
                components.command_names.push(cmd_name.to_string());
            }
        }
    }

    // 4. Determine status
    let status = if manifest.mcp_servers.is_empty() {
        "installed"
    } else {
        "partially_installed" // MCP servers need approval
    };

    // 5. Record in SQLite
    let manifest_json = serde_json::to_string(&manifest)?;
    let components_json = serde_json::to_string(&components)?;

    let conn = Connection::open(&state.db_path)?;
    conn.execute(
        "INSERT OR REPLACE INTO installed_plugins (id, version, manifest, components, status) VALUES (?, ?, ?, ?, ?)",
        params![
            manifest.id,
            manifest.version.to_string(),
            manifest_json,
            components_json,
            status,
        ],
    )?;

    info!(plugin_id = %manifest.id, status, "plugin recorded");

    Ok(InstalledPlugin {
        id: manifest.id.clone(),
        version: manifest.version.to_string(),
        manifest: manifest.clone(),
        components,
        status: status.to_string(),
        installed_at: chrono_now(),
    })
}

/// List all installed plugins.
pub fn list_installed_plugins(state: &AppState) -> Result<Vec<InstalledPlugin>> {
    let conn = Connection::open(&state.db_path)?;
    let mut stmt = conn.prepare(
        "SELECT id, version, manifest, components, status, installed_at FROM installed_plugins ORDER BY id",
    )?;

    let plugins = stmt
        .query_map([], |row| {
            let manifest_str: String = row.get(2)?;
            let components_str: String = row.get(3)?;
            Ok(InstalledPlugin {
                id: row.get(0)?,
                version: row.get(1)?,
                manifest: serde_json::from_str(&manifest_str).unwrap_or_else(|_| {
                    // Fallback for corrupt manifest — shouldn't happen
                    serde_json::from_str("{}").unwrap()
                }),
                components: serde_json::from_str(&components_str).unwrap_or_else(|_| {
                    PluginComponents {
                        skill_ids: vec![],
                        mcp_server_names: vec![],
                        command_names: vec![],
                    }
                }),
                status: row.get(4)?,
                installed_at: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(plugins)
}

/// Get a single installed plugin by ID.
pub fn get_installed_plugin(state: &AppState, plugin_id: &str) -> Result<Option<InstalledPlugin>> {
    let conn = Connection::open(&state.db_path)?;
    let result = conn.query_row(
        "SELECT id, version, manifest, components, status, installed_at FROM installed_plugins WHERE id = ?",
        params![plugin_id],
        |row| {
            let manifest_str: String = row.get(2)?;
            let components_str: String = row.get(3)?;
            Ok(InstalledPlugin {
                id: row.get(0)?,
                version: row.get(1)?,
                manifest: serde_json::from_str(&manifest_str).unwrap_or_else(|_| {
                    serde_json::from_str("{}").unwrap()
                }),
                components: serde_json::from_str(&components_str).unwrap_or_else(|_| {
                    PluginComponents {
                        skill_ids: vec![],
                        mcp_server_names: vec![],
                        command_names: vec![],
                    }
                }),
                status: row.get(4)?,
                installed_at: row.get(5)?,
            })
        },
    );

    match result {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Uninstall a plugin: remove skills, clean up commands, delete record.
pub fn uninstall_plugin(state: &AppState, plugin_id: &str) -> Result<Option<String>> {
    let plugin = match get_installed_plugin(state, plugin_id)? {
        Some(p) => p,
        None => return Ok(None),
    };

    // 1. Uninstall skills
    for skill_id in &plugin.components.skill_ids {
        if let Err(e) = uninstall_skill(state, skill_id) {
            info!(skill_id, error = %e, "warning: failed to uninstall plugin skill");
        }
    }

    // 2. Remove slash commands
    if let Some(home) = std::env::var("HOME").ok().map(std::path::PathBuf::from) {
        let skills_dir = home.join(".claude").join("skills");
        for cmd_name in &plugin.components.command_names {
            let cmd_dir = skills_dir.join(cmd_name);
            if cmd_dir.exists() {
                let _ = fs::remove_dir_all(&cmd_dir);
                info!(command = cmd_name.as_str(), "removed slash command");
            }
        }
    }

    // 3. Delete plugin record
    let conn = Connection::open(&state.db_path)?;
    conn.execute(
        "DELETE FROM installed_plugins WHERE id = ?",
        params![plugin_id],
    )?;

    info!(plugin_id, version = %plugin.version, "plugin uninstalled");
    Ok(Some(plugin.version))
}

/// Update plugin status (e.g. from partially_installed to installed).
pub fn update_plugin_status(state: &AppState, plugin_id: &str, status: &str) -> Result<()> {
    let conn = Connection::open(&state.db_path)?;
    conn.execute(
        "UPDATE installed_plugins SET status = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        params![status, plugin_id],
    )?;
    Ok(())
}

fn chrono_now() -> String {
    // Simple UTC timestamp without pulling in chrono crate
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("plugin-test-{label}-{nanos}")),
        )
        .unwrap()
    }

    fn write_test_plugin(root: &camino::Utf8Path) {
        // Embedded skill
        let skill_dir = root.join("skills").join("test-skill");
        fs::create_dir_all(skill_dir.join("schemas")).unwrap();
        fs::create_dir_all(skill_dir.join("prompts")).unwrap();
        fs::write(
            skill_dir.join("manifest.json"),
            r#"{
            "schema_version": "1.0", "id": "test-skill", "name": "Test Skill",
            "version": "0.1.0", "publisher": "test",
            "entrypoint": "workflow.yaml",
            "inputs_schema": "schemas/input.schema.json",
            "outputs_schema": "schemas/output.schema.json",
            "permissions": {"filesystem": "none", "network": "none", "clipboard": false},
            "execution": {"sandbox_profile": "strict", "timeout_seconds": 30, "memory_mb": 512}
        }"#,
        )
        .unwrap();
        fs::write(skill_dir.join("workflow.yaml"), "name: test\nsteps: []").unwrap();
        fs::write(skill_dir.join("schemas/input.schema.json"), "{}").unwrap();
        fs::write(skill_dir.join("schemas/output.schema.json"), "{}").unwrap();
        fs::write(skill_dir.join("prompts/system.txt"), "test").unwrap();

        // Command
        let cmd_dir = root.join("commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(
            cmd_dir.join("test-cmd.md"),
            "---\nname: test-cmd\ndescription: test\n---\nDo it.",
        )
        .unwrap();

        // plugin.json
        fs::write(
            root.join("plugin.json"),
            r#"{
            "schema_version": "1.0",
            "id": "test-plugin",
            "name": "Test Plugin",
            "version": "0.1.0",
            "publisher": "test",
            "skills": [{ "path": "./skills/test-skill" }],
            "mcp_servers": [{ "name": "Test Server", "package_source": "npx test" }],
            "commands": [{ "path": "./commands/test-cmd.md" }]
        }"#,
        )
        .unwrap();
    }

    #[test]
    fn install_and_list_plugin() {
        let state_root = temp_root("install-list");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let plugin_dir = temp_root("plugin-src");
        fs::create_dir_all(&plugin_dir).unwrap();
        write_test_plugin(&plugin_dir);

        // Install
        let result = install_plugin_from_dir(&state, &plugin_dir).unwrap();
        assert_eq!(result.id, "test-plugin");
        assert_eq!(result.version, "0.1.0");
        assert_eq!(result.components.skill_ids, vec!["test-skill"]);
        assert_eq!(result.components.mcp_server_names, vec!["Test Server"]);
        assert_eq!(result.status, "partially_installed"); // has MCP servers

        // List
        let plugins = list_installed_plugins(&state).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "test-plugin");

        // Get
        let fetched = get_installed_plugin(&state, "test-plugin").unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().version, "0.1.0");

        // Not found
        let missing = get_installed_plugin(&state, "nonexistent").unwrap();
        assert!(missing.is_none());

        let _ = fs::remove_dir_all(state_root.as_str());
        let _ = fs::remove_dir_all(plugin_dir.as_str());
    }

    #[test]
    fn uninstall_plugin_removes_record_and_skills() {
        let state_root = temp_root("uninstall");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let plugin_dir = temp_root("plugin-uninstall-src");
        fs::create_dir_all(&plugin_dir).unwrap();
        write_test_plugin(&plugin_dir);

        install_plugin_from_dir(&state, &plugin_dir).unwrap();

        // Verify skill was installed
        let skill_active = state.root_dir.join("skills/test-skill/active");
        assert!(skill_active.exists(), "skill should be installed");

        // Uninstall
        let version = uninstall_plugin(&state, "test-plugin").unwrap();
        assert_eq!(version, Some("0.1.0".to_string()));

        // Plugin record gone
        let plugins = list_installed_plugins(&state).unwrap();
        assert!(plugins.is_empty());

        // Skill removed
        assert!(!skill_active.exists(), "skill should be removed");

        // Uninstall again returns None
        let again = uninstall_plugin(&state, "test-plugin").unwrap();
        assert!(again.is_none());

        let _ = fs::remove_dir_all(state_root.as_str());
        let _ = fs::remove_dir_all(plugin_dir.as_str());
    }

    #[test]
    fn update_plugin_status_changes_status() {
        let state_root = temp_root("status-update");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let plugin_dir = temp_root("plugin-status-src");
        fs::create_dir_all(&plugin_dir).unwrap();
        write_test_plugin(&plugin_dir);

        install_plugin_from_dir(&state, &plugin_dir).unwrap();

        // Update status
        update_plugin_status(&state, "test-plugin", "installed").unwrap();

        let plugin = get_installed_plugin(&state, "test-plugin")
            .unwrap()
            .unwrap();
        assert_eq!(plugin.status, "installed");

        let _ = fs::remove_dir_all(state_root.as_str());
        let _ = fs::remove_dir_all(plugin_dir.as_str());
    }
}
