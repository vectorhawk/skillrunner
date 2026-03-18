use anyhow::Result;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

/// A detected AI client that supports MCP.
#[derive(Debug)]
pub struct DetectedClient {
    pub name: String,
    pub config_path: PathBuf,
    pub already_configured: bool,
}

/// Detect installed AI clients that support MCP server configuration.
pub fn detect_ai_clients(_skillrunner_path: &str) -> Vec<DetectedClient> {
    let mut clients = Vec::new();

    let home = match dirs_home() {
        Some(h) => h,
        None => return clients,
    };

    // Claude Code: ~/.claude.json
    let claude_config = home.join(".claude.json");
    let claude_configured = is_skillclub_configured(&claude_config);
    if claude_config.parent().map(|p| p.exists()).unwrap_or(false) || claude_config.exists() {
        // Claude Code directory or config exists
        let claude_dir = home.join(".claude");
        if claude_dir.exists() || claude_config.exists() {
            clients.push(DetectedClient {
                name: "Claude Code".to_string(),
                config_path: claude_config,
                already_configured: claude_configured,
            });
        }
    }

    // Cursor: ~/.cursor/mcp.json
    let cursor_dir = home.join(".cursor");
    if cursor_dir.exists() {
        let cursor_config = cursor_dir.join("mcp.json");
        let cursor_configured = is_skillclub_configured(&cursor_config);
        clients.push(DetectedClient {
            name: "Cursor".to_string(),
            config_path: cursor_config,
            already_configured: cursor_configured,
        });
    }

    clients
}

/// Configure SkillRunner as an MCP server for a detected client.
pub fn configure_client(
    client: &DetectedClient,
    skillrunner_path: &str,
    registry_url: &Option<String>,
) -> Result<()> {
    let mut config: serde_json::Value = if client.config_path.exists() {
        let text = fs::read_to_string(&client.config_path)?;
        serde_json::from_str(&text).unwrap_or(json!({}))
    } else {
        json!({})
    };

    let mcp_servers = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config is not a JSON object"))?
        .entry("mcpServers")
        .or_insert(json!({}));

    let mut env = json!({});
    if let Some(url) = registry_url {
        env["SKILLCLUB_REGISTRY_URL"] = json!(url);
    }

    mcp_servers["skillclub"] = json!({
        "command": skillrunner_path,
        "args": ["mcp", "serve"],
        "env": env,
    });

    // Ensure parent directory exists
    if let Some(parent) = client.config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let formatted = serde_json::to_string_pretty(&config)?;
    fs::write(&client.config_path, formatted)?;

    Ok(())
}

/// Check if the first-run setup has already been offered.
pub fn first_run_offered(state: &skillrunner_core::state::AppState) -> bool {
    let conn = match rusqlite::Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Create the metadata table if it doesn't exist
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    );

    conn.query_row(
        "SELECT value FROM metadata WHERE key = 'mcp_setup_offered'",
        [],
        |row| row.get::<_, String>(0),
    )
    .is_ok()
}

/// Mark the first-run setup as offered.
pub fn mark_first_run_offered(state: &skillrunner_core::state::AppState) -> Result<()> {
    let conn = rusqlite::Connection::open(&state.db_path)?;
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    );
    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES ('mcp_setup_offered', 'true')",
        [],
    )?;
    Ok(())
}

fn dirs_home() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

fn is_skillclub_configured(config_path: &PathBuf) -> bool {
    if !config_path.exists() {
        return false;
    }
    let text = match fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let config: serde_json::Value = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(_) => return false,
    };
    config
        .get("mcpServers")
        .and_then(|s| s.get("skillclub"))
        .is_some()
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
            std::env::temp_dir().join(format!("mcp-setup-test-{label}-{nanos}")),
        )
        .unwrap()
    }

    #[test]
    fn configure_client_creates_config_file() {
        let tmp = temp_root("configure");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("test-mcp.json").into_std_path_buf();

        let client = DetectedClient {
            name: "Test Client".to_string(),
            config_path: config_path.clone(),
            already_configured: false,
        };

        configure_client(
            &client,
            "/usr/local/bin/skillrunner",
            &Some("https://registry.skillclub.ai".to_string()),
        )
        .unwrap();

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        assert_eq!(
            content["mcpServers"]["skillclub"]["command"],
            "/usr/local/bin/skillrunner"
        );
        assert_eq!(
            content["mcpServers"]["skillclub"]["args"][0],
            "mcp"
        );
        assert_eq!(
            content["mcpServers"]["skillclub"]["args"][1],
            "serve"
        );
        assert_eq!(
            content["mcpServers"]["skillclub"]["env"]["SKILLCLUB_REGISTRY_URL"],
            "https://registry.skillclub.ai"
        );

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn configure_client_preserves_existing_config() {
        let tmp = temp_root("preserve");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("existing.json").into_std_path_buf();

        // Write existing config
        fs::write(
            &config_path,
            r#"{"mcpServers":{"other-server":{"command":"other"}},"customKey":"value"}"#,
        )
        .unwrap();

        let client = DetectedClient {
            name: "Test".to_string(),
            config_path: config_path.clone(),
            already_configured: false,
        };

        configure_client(&client, "/usr/local/bin/skillrunner", &None).unwrap();

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        // Existing keys preserved
        assert_eq!(content["customKey"], "value");
        assert_eq!(content["mcpServers"]["other-server"]["command"], "other");
        // SkillClub added
        assert_eq!(
            content["mcpServers"]["skillclub"]["command"],
            "/usr/local/bin/skillrunner"
        );

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn first_run_tracking() {
        let state_root = temp_root("first-run");
        let state =
            skillrunner_core::state::AppState::bootstrap_in(state_root.clone()).unwrap();

        assert!(!first_run_offered(&state));
        mark_first_run_offered(&state).unwrap();
        assert!(first_run_offered(&state));

        let _ = fs::remove_dir_all(state_root.as_str());
    }

    #[test]
    fn is_skillclub_configured_false_when_missing() {
        assert!(!is_skillclub_configured(&PathBuf::from("/nonexistent/path.json")));
    }

    #[test]
    fn is_skillclub_configured_true_when_present() {
        let tmp = temp_root("configured-check");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("check.json").into_std_path_buf();
        fs::write(
            &config_path,
            r#"{"mcpServers":{"skillclub":{"command":"skillrunner"}}}"#,
        )
        .unwrap();

        assert!(is_skillclub_configured(&config_path));

        let _ = fs::remove_dir_all(tmp.as_str());
    }
}
