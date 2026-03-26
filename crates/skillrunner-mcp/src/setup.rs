use anyhow::Result;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

/// Configuration for a detected AI client that supports MCP.
///
/// `mcp_key` is the top-level JSON key in the client's config file that holds
/// the MCP server map (e.g. `"mcpServers"` for Claude Code, Cursor, Windsurf,
/// Gemini CLI; `"servers"` for VS Code's nested `mcp` object).
#[derive(Debug)]
pub struct ClientConfig {
    pub name: String,
    pub config_path: PathBuf,
    pub mcp_key: String,
    pub already_configured: bool,
}

/// Backward-compatible type alias so callers that still use `DetectedClient` compile.
pub type DetectedClient = ClientConfig;

/// Detect installed AI clients that support MCP server configuration.
pub fn detect_ai_clients(_skillrunner_path: &str) -> Vec<ClientConfig> {
    let mut clients = Vec::new();

    let home = match dirs_home() {
        Some(h) => h,
        None => return clients,
    };

    // ── Claude Code ──────────────────────────────────────────────────────────
    // Config: ~/.claude.json  (key: "mcpServers")
    let claude_config = home.join(".claude.json");
    let claude_dir = home.join(".claude");
    if claude_dir.exists() || claude_config.exists() {
        let already = is_skillrunner_configured(&claude_config, "mcpServers");
        clients.push(ClientConfig {
            name: "Claude Code".to_string(),
            config_path: claude_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
        });
    }

    // ── Cursor ───────────────────────────────────────────────────────────────
    // Config: ~/.cursor/mcp.json  (key: "mcpServers")
    let cursor_dir = home.join(".cursor");
    if cursor_dir.exists() {
        let cursor_config = cursor_dir.join("mcp.json");
        let already = is_skillrunner_configured(&cursor_config, "mcpServers");
        clients.push(ClientConfig {
            name: "Cursor".to_string(),
            config_path: cursor_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
        });
    }

    // ── Windsurf ─────────────────────────────────────────────────────────────
    // Config: ~/.codeium/windsurf/mcp_config.json  (key: "mcpServers")
    let windsurf_config = home.join(".codeium").join("windsurf").join("mcp_config.json");
    let windsurf_dir = home.join(".codeium").join("windsurf");
    if windsurf_dir.exists() || windsurf_config.exists() {
        let already = is_skillrunner_configured(&windsurf_config, "mcpServers");
        clients.push(ClientConfig {
            name: "Windsurf".to_string(),
            config_path: windsurf_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
        });
    }

    // ── VS Code (with GitHub Copilot / MCP extension) ────────────────────────
    // Config: ~/Library/Application Support/Code/User/settings.json (macOS)
    //         ~/.config/Code/User/settings.json (Linux)
    // Key: "mcp" object's "servers" subkey — we store under top-level "mcp"
    // and use mcp_key = "mcp" so configure_client() writes into config["mcp"]["servers"].
    // For simplicity we use mcp_key = "mcpServers" and write at top level;
    // VS Code also reads top-level mcpServers in its MCP extension.
    let vscode_config_path = vscode_settings_path(&home);
    if let Some(vscode_config) = vscode_config_path {
        let vscode_dir = vscode_config.parent().map(|p| p.to_path_buf());
        if vscode_dir.as_ref().map(|d| d.exists()).unwrap_or(false) || vscode_config.exists() {
            let already = is_skillrunner_configured(&vscode_config, "mcpServers");
            clients.push(ClientConfig {
                name: "VS Code".to_string(),
                config_path: vscode_config,
                mcp_key: "mcpServers".to_string(),
                already_configured: already,
            });
        }
    }

    // ── Gemini CLI ───────────────────────────────────────────────────────────
    // Config: ~/.gemini/settings.json  (key: "mcpServers")
    let gemini_config = home.join(".gemini").join("settings.json");
    let gemini_dir = home.join(".gemini");
    if gemini_dir.exists() || gemini_config.exists() {
        let already = is_skillrunner_configured(&gemini_config, "mcpServers");
        clients.push(ClientConfig {
            name: "Gemini CLI".to_string(),
            config_path: gemini_config,
            mcp_key: "mcpServers".to_string(),
            already_configured: already,
        });
    }

    clients
}

/// Configure SkillRunner as an MCP server for a detected client.
pub fn configure_client(
    client: &ClientConfig,
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
        .entry(&client.mcp_key)
        .or_insert(json!({}));

    let mut env = json!({});
    if let Some(url) = registry_url {
        env["SKILLCLUB_REGISTRY_URL"] = json!(url);
    }

    mcp_servers["skillrunner"] = json!({
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
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Return the VS Code user settings path for the current OS.
fn vscode_settings_path(home: &std::path::Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Code")
                .join("User")
                .join("settings.json"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        Some(
            home.join(".config")
                .join("Code")
                .join("User")
                .join("settings.json"),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// An MCP server entry found in a client's config file that is NOT managed by SkillRunner.
#[derive(Debug, Clone)]
pub struct UnmanagedServer {
    /// Name/key of the server in the config file (e.g. "github-mcp").
    pub server_name: String,
    /// Which AI client config file contained this entry.
    pub config_path: String,
    /// Which AI client (e.g. "Claude Code", "Cursor").
    pub client_name: String,
}

/// Scan all detected AI client config files and return MCP servers not managed by SkillRunner.
///
/// A server is "managed" if its key is `"skillrunner"`. Everything else is unmanaged.
pub fn detect_unmanaged_servers() -> Vec<UnmanagedServer> {
    let clients = detect_ai_clients("");
    let mut unmanaged = Vec::new();

    for client in &clients {
        if !client.config_path.exists() {
            continue;
        }
        let text = match fs::read_to_string(&client.config_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let config: serde_json::Value = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let servers = match config.get(&client.mcp_key).and_then(|v| v.as_object()) {
            Some(s) => s,
            None => continue,
        };

        for key in servers.keys() {
            if key == "skillrunner" {
                continue;
            }
            unmanaged.push(UnmanagedServer {
                server_name: key.clone(),
                config_path: client.config_path.display().to_string(),
                client_name: client.name.clone(),
            });
        }
    }

    unmanaged
}

/// Get the machine hostname for audit event identification.
pub fn get_machine_id() -> String {
    // Try HOSTNAME env var first (common on Linux), then shell out
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .map_err(|_| std::env::VarError::NotPresent)
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

fn is_skillrunner_configured(config_path: &PathBuf, mcp_key: &str) -> bool {
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
        .get(mcp_key)
        .and_then(|s| s.get("skillrunner"))
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

        let client = ClientConfig {
            name: "Test Client".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
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
            content["mcpServers"]["skillrunner"]["command"],
            "/usr/local/bin/skillrunner"
        );
        assert_eq!(content["mcpServers"]["skillrunner"]["args"][0], "mcp");
        assert_eq!(content["mcpServers"]["skillrunner"]["args"][1], "serve");
        assert_eq!(
            content["mcpServers"]["skillrunner"]["env"]["SKILLCLUB_REGISTRY_URL"],
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

        let client = ClientConfig {
            name: "Test".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
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
            content["mcpServers"]["skillrunner"]["command"],
            "/usr/local/bin/skillrunner"
        );

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn configure_client_uses_custom_mcp_key() {
        let tmp = temp_root("custom-key");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("settings.json").into_std_path_buf();

        let client = ClientConfig {
            name: "Custom Client".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        configure_client(&client, "/usr/local/bin/skillrunner", &None).unwrap();

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        // Written under the custom key
        assert_eq!(
            content["mcpServers"]["skillrunner"]["command"],
            "/usr/local/bin/skillrunner"
        );
        // The default "mcpServers" key should NOT be present
        assert!(content.get("mcpServers").is_some());

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn detect_windsurf_when_dir_exists() {
        let tmp = temp_root("windsurf");
        let windsurf_dir = tmp.join(".codeium").join("windsurf");
        fs::create_dir_all(&windsurf_dir).unwrap();

        // Override HOME so detect_ai_clients looks in our temp dir
        std::env::set_var("HOME", tmp.as_str());

        let clients = detect_ai_clients("/usr/local/bin/skillrunner");
        let windsurf = clients.iter().find(|c| c.name == "Windsurf");
        assert!(windsurf.is_some(), "Windsurf should be detected");
        assert_eq!(windsurf.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn detect_gemini_cli_when_dir_exists() {
        let tmp = temp_root("gemini");
        let gemini_dir = tmp.join(".gemini");
        fs::create_dir_all(&gemini_dir).unwrap();

        std::env::set_var("HOME", tmp.as_str());

        let clients = detect_ai_clients("/usr/local/bin/skillrunner");
        let gemini = clients.iter().find(|c| c.name == "Gemini CLI");
        assert!(gemini.is_some(), "Gemini CLI should be detected");
        assert_eq!(gemini.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn detect_vscode_when_settings_dir_exists() {
        let tmp = temp_root("vscode");

        #[cfg(target_os = "macos")]
        let vscode_dir = tmp
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User");
        #[cfg(target_os = "linux")]
        let vscode_dir = tmp.join(".config").join("Code").join("User");
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            // VS Code detection not supported on this OS, skip
            return;
        }

        fs::create_dir_all(&vscode_dir).unwrap();

        std::env::set_var("HOME", tmp.as_str());

        let clients = detect_ai_clients("/usr/local/bin/skillrunner");
        let vscode = clients.iter().find(|c| c.name == "VS Code");
        assert!(vscode.is_some(), "VS Code should be detected");
        assert_eq!(vscode.unwrap().mcp_key, "mcpServers");

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn windsurf_configure_writes_correct_config() {
        let tmp = temp_root("windsurf-cfg");
        let windsurf_dir = tmp.join(".codeium").join("windsurf");
        fs::create_dir_all(&windsurf_dir).unwrap();
        let config_path = windsurf_dir.join("mcp_config.json").into_std_path_buf();

        let client = ClientConfig {
            name: "Windsurf".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        configure_client(&client, "/usr/local/bin/skillrunner", &None).unwrap();

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        assert_eq!(
            content["mcpServers"]["skillrunner"]["command"],
            "/usr/local/bin/skillrunner"
        );

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn gemini_cli_configure_writes_correct_config() {
        let tmp = temp_root("gemini-cfg");
        let gemini_dir = tmp.join(".gemini");
        fs::create_dir_all(&gemini_dir).unwrap();
        let config_path = gemini_dir.join("settings.json").into_std_path_buf();

        let client = ClientConfig {
            name: "Gemini CLI".to_string(),
            config_path: config_path.clone(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        };

        configure_client(
            &client,
            "/usr/local/bin/skillrunner",
            &Some("https://registry.test.com".to_string()),
        )
        .unwrap();

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();

        assert_eq!(
            content["mcpServers"]["skillrunner"]["command"],
            "/usr/local/bin/skillrunner"
        );
        assert_eq!(
            content["mcpServers"]["skillrunner"]["env"]["SKILLCLUB_REGISTRY_URL"],
            "https://registry.test.com"
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
    fn detect_unmanaged_servers_finds_non_skillrunner_entries() {
        let tmp = temp_root("unmanaged");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        // Write a config with skillrunner + two other servers
        let config_path = tmp.join(".claude.json");
        fs::write(
            &config_path,
            r#"{
                "mcpServers": {
                    "skillrunner": {"command": "skillrunner", "args": ["mcp", "serve"]},
                    "github-mcp": {"command": "npx", "args": ["@modelcontextprotocol/server-github"]},
                    "slack-mcp": {"command": "npx", "args": ["@modelcontextprotocol/server-slack"]}
                }
            }"#,
        )
        .unwrap();

        std::env::set_var("HOME", tmp.as_str());

        let unmanaged = detect_unmanaged_servers();
        let claude_unmanaged: Vec<_> = unmanaged
            .iter()
            .filter(|u| u.client_name == "Claude Code")
            .collect();

        assert_eq!(claude_unmanaged.len(), 2);
        let names: Vec<&str> = claude_unmanaged.iter().map(|u| u.server_name.as_str()).collect();
        assert!(names.contains(&"github-mcp"));
        assert!(names.contains(&"slack-mcp"));

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn detect_unmanaged_servers_empty_when_only_skillrunner() {
        let tmp = temp_root("unmanaged-empty");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let config_path = tmp.join(".claude.json");
        fs::write(
            &config_path,
            r#"{"mcpServers": {"skillrunner": {"command": "skillrunner"}}}"#,
        )
        .unwrap();

        std::env::set_var("HOME", tmp.as_str());

        let unmanaged = detect_unmanaged_servers();
        let claude_unmanaged: Vec<_> = unmanaged
            .iter()
            .filter(|u| u.client_name == "Claude Code")
            .collect();
        assert!(claude_unmanaged.is_empty());

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn get_machine_id_returns_non_empty() {
        let id = get_machine_id();
        assert!(!id.is_empty());
        assert_ne!(id, "unknown");
    }

    #[test]
    fn is_skillrunner_configured_false_when_missing() {
        assert!(!is_skillrunner_configured(
            &PathBuf::from("/nonexistent/path.json"),
            "mcpServers"
        ));
    }

    #[test]
    fn is_skillrunner_configured_true_when_present() {
        let tmp = temp_root("configured-check");
        fs::create_dir_all(&tmp).unwrap();
        let config_path = tmp.join("check.json").into_std_path_buf();
        fs::write(
            &config_path,
            r#"{"mcpServers":{"skillrunner":{"command":"skillrunner"}}}"#,
        )
        .unwrap();

        assert!(is_skillrunner_configured(&config_path, "mcpServers"));

        let _ = fs::remove_dir_all(tmp.as_str());
    }
}
