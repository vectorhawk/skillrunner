use anyhow::{Context, Result};
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

    // ── Claude Desktop ───────────────────────────────────────────────────────
    // Config: ~/Library/Application Support/Claude/claude_desktop_config.json (macOS)
    //         ~/.config/Claude/claude_desktop_config.json (Linux)
    // Key: "mcpServers"
    if let Some(claude_desktop_config) = claude_desktop_config_path(&home) {
        let claude_desktop_dir = claude_desktop_config.parent().map(|p| p.to_path_buf());
        if claude_desktop_dir
            .as_ref()
            .map(|d| d.exists())
            .unwrap_or(false)
            || claude_desktop_config.exists()
        {
            let already = is_skillrunner_configured(&claude_desktop_config, "mcpServers");
            clients.push(ClientConfig {
                name: "Claude Desktop".to_string(),
                config_path: claude_desktop_config,
                mcp_key: "mcpServers".to_string(),
                already_configured: already,
            });
        }
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
    let windsurf_config = home
        .join(".codeium")
        .join("windsurf")
        .join("mcp_config.json");
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
        env["VECTORHAWK_REGISTRY_URL"] = json!(url);
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

/// Return the Claude Desktop config path for the current OS.
fn claude_desktop_config_path(home: &std::path::Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        Some(
            home.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json"),
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

/// Skill definitions for auto-installed slash commands.
/// Each tuple is (directory_name, SKILL.md content).
fn skill_definitions() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "skillclub",
            r#"---
name: skillclub
description: SkillClub hub — show auth status, installed skills, MCP servers, and available commands
---
Show the user a SkillClub status overview:

1. Call skillclub_login to check authentication status (if registry is configured)
2. Call skillclub_list to show installed skills count and names
3. Call skillclub_mcp_status to show active MCP server count (if registry is configured)
4. Then list all available SkillClub slash commands:
   - /mcp-login — Authenticate with SkillClub
   - /mcp-search — Browse approved MCP servers
   - /mcp-install — Install an approved MCP server
   - /mcp-request — Request access to a new MCP server
   - /mcp-status — Check MCP server request status
   - /skill-search — Search for skills in the registry
   - /skill-install — Install a skill
   - /skill-list — List installed skills
   - /skill-create — Create a new skill
   - /skill-publish — Publish a skill to the registry
   - /plugin-search — Search for plugins
   - /plugin-install — Install a plugin
   - /plugin-list — List installed plugins
"#,
        ),
        (
            "mcp-login",
            r#"---
name: mcp-login
description: Authenticate with the SkillClub registry
---
Log the user into SkillClub. Call the skillclub_login tool.

If it succeeds, confirm they are logged in and show their identity.
If it fails, show the error and suggest checking their registry URL.
"#,
        ),
        (
            "mcp-search",
            r#"---
name: mcp-search
description: Browse approved MCP servers in your organization's catalog
---
Browse available MCP servers. Call the skillclub_mcp_catalog tool.

$ARGUMENTS

Show results in a clean table with server name, status, and description.
If no servers are found, suggest the user contact their IT admin.
"#,
        ),
        (
            "mcp-install",
            r#"---
name: mcp-install
description: Install an approved MCP server through SkillClub governance
---
Install an MCP server through governance. Call the skillclub_mcp_install tool with the server ID from the arguments.

$ARGUMENTS

If the server is not yet approved, suggest using /mcp-request first.
"#,
        ),
        (
            "mcp-request",
            r#"---
name: mcp-request
description: Request access to a new MCP server from your organization
---
Request access to an MCP server. Call the skillclub_mcp_request tool with the server ID from the arguments.

$ARGUMENTS

Explain the approval status to the user (auto-approved, pending review, etc.).
Suggest using /mcp-status to check back on pending requests.
"#,
        ),
        (
            "mcp-status",
            r#"---
name: mcp-status
description: Check the status of your MCP server access requests
---
Check MCP server request status. Call the skillclub_mcp_status tool.

Show results clearly — which requests are approved, pending, or denied.
For approved servers, suggest using /mcp-install to activate them.
"#,
        ),
        (
            "skill-search",
            r#"---
name: skill-search
description: Search the SkillClub registry for available skills
---
Search for skills in the SkillClub registry. Call the skillclub_search tool with the query from the arguments. Use an empty query to list all available skills.

$ARGUMENTS

Show results with skill name, version, and description.
"#,
        ),
        (
            "skill-install",
            r#"---
name: skill-install
description: Install a skill from the SkillClub registry
---
Install a skill. Call the skillclub_install tool with the skill ID from the arguments.

$ARGUMENTS

Confirm installation and show what the skill does.
"#,
        ),
        (
            "skill-list",
            r#"---
name: skill-list
description: List all installed SkillClub skills
---
List installed skills. Call the skillclub_list tool.

Show each skill's name, version, and a brief description.
"#,
        ),
        (
            "skill-create",
            r#"---
name: skill-create
description: Create a new SkillClub skill from a name and system prompt
---
Create a new skill. Call the skillclub_author tool with the name and description from the arguments.

$ARGUMENTS

Walk the user through the result — show the generated bundle path and suggest next steps (validate, test, publish).
"#,
        ),
        (
            "skill-publish",
            r#"---
name: skill-publish
description: Publish a skill bundle to the SkillClub registry
---
Publish a skill to the registry. Call the skillclub_publish tool with the skill path from the arguments.

$ARGUMENTS

If not authenticated, suggest using /mcp-login first.
Show the publish result and the skill's registry URL.
"#,
        ),
        (
            "plugin-search",
            r#"---
name: plugin-search
description: Search for SkillClub plugins (composite bundles of skills + MCP servers + commands)
---
Search for plugins. Call the skillclub_plugin_search tool with the query from the arguments.

$ARGUMENTS

Show results with plugin name, description, and component summary (skills, MCP servers, commands).
"#,
        ),
        (
            "plugin-install",
            r#"---
name: plugin-install
description: Install a SkillClub plugin from a local directory or registry
---
Install a plugin. Call the skillclub_plugin_install tool with the path or plugin ID from the arguments.

$ARGUMENTS

Show what was installed (skills, MCP servers, commands) and any pending approvals.
If MCP servers need approval, suggest using /mcp-request and /mcp-status.
"#,
        ),
        (
            "plugin-list",
            r#"---
name: plugin-list
description: List all installed SkillClub plugins with their components
---
List installed plugins. Call the skillclub_plugin_list tool.

Show each plugin's name, version, status, and component breakdown (skills, MCP servers, commands).
"#,
        ),
    ]
}

// ── Phase 3: Multi-agent fan-out ────────────────────────────────────────────

/// Report from a fan-out install or uninstall sweep.
#[derive(Debug, Default)]
pub struct FanoutReport {
    /// Display names of clients where the skill was successfully linked.
    pub installed: Vec<String>,
    /// (display name, reason) for clients that were skipped.
    pub skipped: Vec<(String, String)>,
}

/// Return the user-level skill directory for a client, or `None` if the
/// client does not support a skill directory (MCP-config-only clients).
///
/// Claude Code  → `~/.claude/skills/`
/// Cursor       → `~/.cursor/skills/`
/// Windsurf     → `~/.codeium/windsurf/skills/`
/// All others   → None
pub fn client_skill_dir(client: &ClientConfig) -> Option<PathBuf> {
    let home = dirs_home()?;
    client_skill_dir_in(&home, client)
}

/// Inner implementation that accepts an explicit home directory.
/// Used by the public API and by tests that supply a tempdir home.
fn client_skill_dir_in(home: &std::path::Path, client: &ClientConfig) -> Option<PathBuf> {
    match client.name.as_str() {
        "Claude Code" => Some(home.join(".claude").join("skills")),
        "Cursor" => Some(home.join(".cursor").join("skills")),
        "Windsurf" => Some(home.join(".codeium").join("windsurf").join("skills")),
        _ => None,
    }
}

/// Like `fanout_skill_to_clients` but uses an explicit home for the skill dirs.
/// Used in tests to avoid touching the global HOME env var.
#[cfg(test)]
fn fanout_skill_to_clients_in(
    skill_id: &str,
    source: &std::path::Path,
    clients: &[&ClientConfig],
    home: &std::path::Path,
) -> Result<FanoutReport> {
    let mut report = FanoutReport::default();

    for client in clients {
        let skill_dir = match client_skill_dir_in(home, client) {
            Some(d) => d,
            None => {
                report
                    .skipped
                    .push((client.name.clone(), "MCP config only".to_string()));
                continue;
            }
        };

        let entry = skill_dir.join(skill_id);

        let skip_reason = check_existing_entry(&entry, source)?;
        if let Some(reason) = skip_reason {
            report.skipped.push((client.name.clone(), reason));
            continue;
        }

        fs::create_dir_all(&skill_dir).with_context(|| {
            format!(
                "could not create skill directory {} for {}",
                skill_dir.display(),
                client.name
            )
        })?;

        create_skill_link(&entry, source).with_context(|| {
            format!("could not link skill '{}' for {}", skill_id, client.name)
        })?;

        report.installed.push(client.name.clone());
    }

    Ok(report)
}

/// Like `fanout_uninstall_skill` but uses an explicit home for the skill dirs.
#[cfg(test)]
fn fanout_uninstall_skill_in(
    skill_id: &str,
    clients: &[&ClientConfig],
    home: &std::path::Path,
) -> FanoutReport {
    let mut report = FanoutReport::default();

    for client in clients {
        let skill_dir = match client_skill_dir_in(home, client) {
            Some(d) => d,
            None => {
                report
                    .skipped
                    .push((client.name.clone(), "MCP config only".to_string()));
                continue;
            }
        };

        let entry = skill_dir.join(skill_id);

        match fs::symlink_metadata(&entry) {
            Err(_) => {
                report
                    .skipped
                    .push((client.name.clone(), "not present".to_string()));
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                if let Err(e) = fs::remove_file(&entry) {
                    report
                        .skipped
                        .push((client.name.clone(), format!("remove failed: {e}")));
                } else {
                    report.installed.push(client.name.clone());
                }
            }
            Ok(_) => {
                report.skipped.push((
                    client.name.clone(),
                    "not a symlink, leaving untouched".to_string(),
                ));
            }
        }
    }

    report
}

/// Symlink (Unix) or copy (Windows) the canonical `active/` directory into
/// each client's skill directory as `{client_skill_dir}/{skill_id}`.
///
/// Idempotency rules:
/// - If the entry already is a symlink pointing at `source`, count as installed.
/// - If the entry is a symlink pointing elsewhere, or a real directory/file,
///   skip with reason "conflicting entry exists".
/// - Clients without a `client_skill_dir` are skipped with "MCP config only".
pub fn fanout_skill_to_clients(
    skill_id: &str,
    source: &std::path::Path,
    clients: &[&ClientConfig],
) -> Result<FanoutReport> {
    let mut report = FanoutReport::default();

    for client in clients {
        let skill_dir = match client_skill_dir(client) {
            Some(d) => d,
            None => {
                report
                    .skipped
                    .push((client.name.clone(), "MCP config only".to_string()));
                continue;
            }
        };

        let entry = skill_dir.join(skill_id);

        // Resolve whether the entry already exists and what it is.
        let skip_reason = check_existing_entry(&entry, source)?;
        if let Some(reason) = skip_reason {
            report.skipped.push((client.name.clone(), reason));
            continue;
        }

        // Create the parent directory and the symlink/copy.
        fs::create_dir_all(&skill_dir).with_context(|| {
            format!(
                "could not create skill directory {} for {}",
                skill_dir.display(),
                client.name
            )
        })?;

        create_skill_link(&entry, source).with_context(|| {
            format!(
                "could not link skill '{}' for {}",
                skill_id, client.name
            )
        })?;

        report.installed.push(client.name.clone());
    }

    Ok(report)
}

/// Sweep `{client_skill_dir}/{skill_id}` from each client.
///
/// Only removes entries that are symlinks (dangling or pointing anywhere).
/// Leaves real directories untouched (user-created content, skipped with reason).
/// Returns a `FanoutReport` describing what was removed vs skipped.
pub fn fanout_uninstall_skill(skill_id: &str, clients: &[&ClientConfig]) -> FanoutReport {
    let mut report = FanoutReport::default();

    for client in clients {
        let skill_dir = match client_skill_dir(client) {
            Some(d) => d,
            None => {
                report
                    .skipped
                    .push((client.name.clone(), "MCP config only".to_string()));
                continue;
            }
        };

        let entry = skill_dir.join(skill_id);

        // Use symlink_metadata so we can inspect the link itself, not its target.
        match fs::symlink_metadata(&entry) {
            Err(_) => {
                // Entry does not exist — nothing to do.
                report
                    .skipped
                    .push((client.name.clone(), "not present".to_string()));
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                if let Err(e) = fs::remove_file(&entry) {
                    report
                        .skipped
                        .push((client.name.clone(), format!("remove failed: {e}")));
                } else {
                    report.installed.push(client.name.clone());
                }
            }
            Ok(_) => {
                report
                    .skipped
                    .push((client.name.clone(), "not a symlink, leaving untouched".to_string()));
            }
        }
    }

    report
}

/// Check whether `entry` already exists and decide what to do.
///
/// Returns `None` if the path is clear (no action needed).
/// Returns `Some(reason)` if the caller should skip creating the link.
fn check_existing_entry(
    entry: &std::path::Path,
    source: &std::path::Path,
) -> Result<Option<String>> {
    // Use symlink_metadata so we inspect the link, not its target.
    let meta = match fs::symlink_metadata(entry) {
        Err(_) => return Ok(None), // does not exist — proceed
        Ok(m) => m,
    };

    if meta.file_type().is_symlink() {
        match fs::read_link(entry) {
            Ok(target) if target == source => {
                // Already points at the right place — idempotent success.
                return Ok(Some("already linked".to_string()));
            }
            _ => {
                // Dangling symlink or points elsewhere.
                return Ok(Some("conflicting entry exists".to_string()));
            }
        }
    }

    // Real file or directory — do not touch.
    Ok(Some("conflicting entry exists".to_string()))
}

/// Create a symlink on Unix or copy the directory on Windows.
fn create_skill_link(entry: &std::path::Path, source: &std::path::Path) -> Result<()> {
    #[cfg(target_family = "unix")]
    {
        std::os::unix::fs::symlink(source, entry)
            .with_context(|| format!("symlink {} -> {}", entry.display(), source.display()))?;
    }
    #[cfg(target_family = "windows")]
    {
        copy_dir_recursive(source, entry).with_context(|| {
            format!(
                "copy {} -> {}",
                source.display(),
                entry.display()
            )
        })?;
    }
    Ok(())
}

/// Recursively copy a directory tree (Windows fallback for symlinks).
#[cfg(target_family = "windows")]
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry_res in fs::read_dir(src)? {
        let entry = entry_res?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Install SkillClub slash command skills to `~/.claude/skills/`.
///
/// Each skill is a SKILL.md file that wraps a SkillRunner MCP tool,
/// giving users clean top-level slash commands in Claude Code.
/// Skips writing if the skill file already exists with identical content.
pub fn install_claude_skills() -> Result<Vec<String>> {
    let home = dirs_home().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let skills_dir = home.join(".claude").join("skills");
    let mut installed = Vec::new();

    for (dir_name, content) in skill_definitions() {
        let skill_dir = skills_dir.join(dir_name);
        let skill_file = skill_dir.join("SKILL.md");

        // Skip if already exists with identical content
        if skill_file.exists() {
            if let Ok(existing) = fs::read_to_string(&skill_file) {
                if existing == content {
                    continue;
                }
            }
        }

        fs::create_dir_all(&skill_dir)?;
        fs::write(&skill_file, content)?;
        installed.push(dir_name.to_string());
    }

    Ok(installed)
}

/// Install skills to a custom root directory (for testing).
#[cfg(test)]
fn install_claude_skills_in(home: &std::path::Path) -> Result<Vec<String>> {
    let skills_dir = home.join(".claude").join("skills");
    let mut installed = Vec::new();

    for (dir_name, content) in skill_definitions() {
        let skill_dir = skills_dir.join(dir_name);
        let skill_file = skill_dir.join("SKILL.md");

        if skill_file.exists() {
            if let Ok(existing) = fs::read_to_string(&skill_file) {
                if existing == content {
                    continue;
                }
            }
        }

        fs::create_dir_all(&skill_dir)?;
        fs::write(&skill_file, content)?;
        installed.push(dir_name.to_string());
    }

    Ok(installed)
}

// ── NPX Guard Hook ──────────────────────────────────────────────────────────

const NPX_GUARD_SCRIPT: &str = r#"#!/bin/bash
# SkillClub NPX Guard — Claude Code PreToolUse hook
# Intercepts npx commands that install MCP servers and redirects to governance.

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('tool_input',{}).get('command',''))" 2>/dev/null)

if [ -z "$COMMAND" ]; then
  exit 0
fi

# Check if the npx command targets an MCP-related package
if echo "$COMMAND" | grep -qE '(npx\s+(-y\s+)?(@modelcontextprotocol/|@anthropic-ai/mcp|mcp-server-|@smithery/))'; then
  PACKAGE=$(echo "$COMMAND" | grep -oE '(@modelcontextprotocol/[^ ]+|@anthropic-ai/mcp[^ ]*|mcp-server-[^ ]+|@smithery/[^ ]+)')
  cat <<EOF
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "MCP servers must be installed through SkillClub governance, not directly via npx.\n\nTo install '${PACKAGE:-this server}':\n  1. Use /mcp-request to request access\n  2. Use /mcp-install to activate after approval\n\nOr use /mcp-search to browse the approved catalog."
  }
}
EOF
  exit 0
fi
"#;

const NPX_SHELL_WRAPPER: &str = r#"#!/bin/bash
# SkillClub NPX Wrapper — intercepts MCP server installs in the terminal.
# Deployed by: skillrunner mcp setup

for arg in "$@"; do
  if echo "$arg" | grep -qE '(@modelcontextprotocol/|@anthropic-ai/mcp|mcp-server-|@smithery/)'; then
    echo ""
    echo "  SkillClub: MCP servers should be installed through governance."
    echo ""
    echo "  Instead, run one of:"
    echo "    skillrunner mcp setup        # configure SkillRunner for your AI client"
    echo "    /mcp-request <server>        # request access in Claude Code"
    echo "    /mcp-search                  # browse approved servers"
    echo ""
    echo "  To bypass this check:"
    REAL_NPX=$(which -a npx 2>/dev/null | grep -v skillclub | head -1)
    if [ -n "$REAL_NPX" ]; then
      echo "    $REAL_NPX $*"
    else
      echo "    command npx $*"
    fi
    echo ""
    exit 1
  fi
done

# Pass through to real npx
REAL_NPX=$(which -a npx 2>/dev/null | grep -v skillclub | head -1)
if [ -n "$REAL_NPX" ]; then
  exec "$REAL_NPX" "$@"
else
  echo "Error: npx not found" >&2
  exit 1
fi
"#;

/// Install the Claude Code PreToolUse hook for npx interception.
///
/// Writes the guard script to `~/.claude/hooks/skillclub-npx-guard.sh`
/// and adds the hook config to `~/.claude/settings.json`.
/// Returns true if any changes were made.
pub fn install_npx_claude_hook() -> Result<bool> {
    let home = dirs_home().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    install_npx_claude_hook_in(&home)
}

fn install_npx_claude_hook_in(home: &std::path::Path) -> Result<bool> {
    let mut changed = false;

    // 1. Write the hook script
    let hooks_dir = home.join(".claude").join("hooks");
    let script_path = hooks_dir.join("skillclub-npx-guard.sh");

    if !script_path.exists()
        || fs::read_to_string(&script_path).unwrap_or_default() != NPX_GUARD_SCRIPT
    {
        fs::create_dir_all(&hooks_dir)?;
        fs::write(&script_path, NPX_GUARD_SCRIPT)?;
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;
        }
        changed = true;
    }

    // 2. Add hook config to ~/.claude/settings.json
    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let text = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&text).unwrap_or(json!({}))
    } else {
        json!({})
    };

    let hook_entry = json!({
        "matcher": "Bash",
        "hooks": [{
            "type": "command",
            "command": script_path.to_string_lossy()
        }]
    });

    // Check if our hook is already present
    let pre_tool_use = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings is not a JSON object"))?
        .entry("hooks")
        .or_insert(json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks is not a JSON object"))?
        .entry("PreToolUse")
        .or_insert(json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("PreToolUse is not an array"))?;

    let already_has_hook = pre_tool_use.iter().any(|h| {
        h.get("hooks")
            .and_then(|hooks| hooks.as_array())
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("skillclub-npx-guard"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !already_has_hook {
        pre_tool_use.push(hook_entry);
        fs::create_dir_all(settings_path.parent().unwrap())?;
        let formatted = serde_json::to_string_pretty(&settings)?;
        fs::write(&settings_path, formatted)?;
        changed = true;
    }

    Ok(changed)
}

/// Install the shell npx wrapper to SkillRunner's bin directory.
///
/// Writes the wrapper to `~/Library/Application Support/SkillClub/SkillRunner/bin/npx`.
/// The user or IT can add this directory to PATH to activate the wrapper.
/// Returns the path to the wrapper if changes were made.
pub fn install_npx_shell_wrapper(
    state: &skillrunner_core::state::AppState,
) -> Result<Option<String>> {
    let bin_dir = state.root_dir.join("bin");
    let wrapper_path = bin_dir.join("npx");

    if wrapper_path.exists()
        && fs::read_to_string(&wrapper_path).unwrap_or_default() == NPX_SHELL_WRAPPER
    {
        return Ok(None);
    }

    fs::create_dir_all(&bin_dir)?;
    fs::write(&wrapper_path, NPX_SHELL_WRAPPER)?;
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755))?;
    }

    Ok(Some(wrapper_path.to_string()))
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
            &Some("https://app.vectorhawk.ai".to_string()),
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
            content["mcpServers"]["skillrunner"]["env"]["VECTORHAWK_REGISTRY_URL"],
            "https://app.vectorhawk.ai"
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
    fn detect_claude_desktop_when_dir_exists() {
        let tmp = temp_root("claude-desktop");

        #[cfg(target_os = "macos")]
        let claude_desktop_dir = tmp
            .join("Library")
            .join("Application Support")
            .join("Claude");
        #[cfg(target_os = "linux")]
        let claude_desktop_dir = tmp.join(".config").join("Claude");
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            // Claude Desktop detection not supported on this OS, skip
            return;
        }

        fs::create_dir_all(&claude_desktop_dir).unwrap();

        // Override HOME so detect_ai_clients looks in our temp dir
        std::env::set_var("HOME", tmp.as_str());

        let clients = detect_ai_clients("/usr/local/bin/skillrunner");
        let claude_desktop = clients.iter().find(|c| c.name == "Claude Desktop");
        assert!(
            claude_desktop.is_some(),
            "Claude Desktop should be detected"
        );
        assert_eq!(claude_desktop.unwrap().mcp_key, "mcpServers");

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
            content["mcpServers"]["skillrunner"]["env"]["VECTORHAWK_REGISTRY_URL"],
            "https://registry.test.com"
        );

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn first_run_tracking() {
        let state_root = temp_root("first-run");
        let state = skillrunner_core::state::AppState::bootstrap_in(state_root.clone()).unwrap();

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
        let names: Vec<&str> = claude_unmanaged
            .iter()
            .map(|u| u.server_name.as_str())
            .collect();
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
    fn install_claude_skills_creates_all_skill_files() {
        let tmp = temp_root("skills-install");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let installed = install_claude_skills_in(tmp.as_ref()).unwrap();

        // Should install all 11 skills
        assert_eq!(installed.len(), 14);
        assert!(installed.contains(&"skillclub".to_string()));
        assert!(installed.contains(&"mcp-login".to_string()));
        assert!(installed.contains(&"mcp-search".to_string()));
        assert!(installed.contains(&"mcp-install".to_string()));
        assert!(installed.contains(&"mcp-request".to_string()));
        assert!(installed.contains(&"mcp-status".to_string()));
        assert!(installed.contains(&"skill-search".to_string()));
        assert!(installed.contains(&"skill-install".to_string()));
        assert!(installed.contains(&"skill-list".to_string()));
        assert!(installed.contains(&"skill-create".to_string()));
        assert!(installed.contains(&"skill-publish".to_string()));

        // Verify SKILL.md files exist and have YAML frontmatter
        for name in &installed {
            let skill_file = tmp
                .join(".claude")
                .join("skills")
                .join(name)
                .join("SKILL.md");
            assert!(skill_file.exists(), "SKILL.md missing for {name}");
            let content = fs::read_to_string(&skill_file).unwrap();
            assert!(
                content.starts_with("---\n"),
                "SKILL.md for {name} must start with YAML frontmatter"
            );
            assert!(
                content.contains(&format!("name: {name}")),
                "SKILL.md for {name} must contain name field"
            );
        }

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn install_claude_skills_skips_identical() {
        let tmp = temp_root("skills-skip");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        // First install
        let first = install_claude_skills_in(tmp.as_ref()).unwrap();
        assert_eq!(first.len(), 14);

        // Second install — should skip all (identical content)
        let second = install_claude_skills_in(tmp.as_ref()).unwrap();
        assert!(
            second.is_empty(),
            "should skip all skills on re-install, got: {:?}",
            second
        );

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn install_claude_skills_updates_changed_content() {
        let tmp = temp_root("skills-update");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        // First install
        install_claude_skills_in(tmp.as_ref()).unwrap();

        // Modify one skill file
        let skill_file = tmp
            .join(".claude")
            .join("skills")
            .join("mcp-login")
            .join("SKILL.md");
        fs::write(&skill_file, "old content").unwrap();

        // Re-install — should update the modified one
        let updated = install_claude_skills_in(tmp.as_ref()).unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0], "mcp-login");

        // Verify content was restored
        let content = fs::read_to_string(&skill_file).unwrap();
        assert!(content.contains("skillclub_login"));

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn skill_definitions_have_valid_structure() {
        for (name, content) in skill_definitions() {
            assert!(!name.is_empty(), "skill dir name must not be empty");
            assert!(
                content.starts_with("---\n"),
                "{name}: must start with YAML frontmatter"
            );
            assert!(
                content.contains("description:"),
                "{name}: must have description field"
            );
            assert!(
                content.contains(&format!("name: {name}")),
                "{name}: name field must match dir name"
            );
        }
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

    #[test]
    fn install_npx_claude_hook_creates_script_and_settings() {
        let tmp = temp_root("npx-hook");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        let changed = install_npx_claude_hook_in(tmp.as_ref()).unwrap();
        assert!(changed, "first install should report changes");

        // Verify script exists and is executable
        let script = tmp.join(".claude/hooks/skillclub-npx-guard.sh");
        assert!(script.exists(), "hook script should exist");
        let content = fs::read_to_string(&script).unwrap();
        assert!(content.contains("SkillClub NPX Guard"));
        assert!(content.contains("permissionDecision"));

        // Verify settings.json has the hook
        let settings_path = tmp.join(".claude/settings.json");
        assert!(settings_path.exists(), "settings.json should exist");
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let hooks = &settings["hooks"]["PreToolUse"];
        assert!(hooks.is_array());
        assert!(!hooks.as_array().unwrap().is_empty());

        // Second install should be idempotent
        let changed2 = install_npx_claude_hook_in(tmp.as_ref()).unwrap();
        assert!(!changed2, "second install should be no-op");

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn install_npx_claude_hook_preserves_existing_settings() {
        let tmp = temp_root("npx-hook-preserve");
        fs::create_dir_all(tmp.join(".claude")).unwrap();

        // Write existing settings
        let settings_path = tmp.join(".claude/settings.json");
        fs::write(&settings_path, r#"{"customKey": "value", "hooks": {}}"#).unwrap();

        install_npx_claude_hook_in(tmp.as_ref()).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["customKey"], "value", "existing keys preserved");
        assert!(settings["hooks"]["PreToolUse"].is_array());

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    fn install_npx_shell_wrapper_creates_executable() {
        let state_root = temp_root("npx-wrapper");
        let state = skillrunner_core::state::AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = install_npx_shell_wrapper(&state).unwrap();
        assert!(result.is_some(), "first install should return path");

        let wrapper_path = state.root_dir.join("bin/npx");
        assert!(wrapper_path.exists(), "wrapper should exist");
        let content = fs::read_to_string(&wrapper_path).unwrap();
        assert!(content.contains("SkillClub NPX Wrapper"));
        assert!(content.contains("@modelcontextprotocol"));

        // Second install should be idempotent
        let result2 = install_npx_shell_wrapper(&state).unwrap();
        assert!(result2.is_none(), "second install should be no-op");

        let _ = fs::remove_dir_all(state_root.as_str());
    }

    // ── Phase 3 fan-out tests ───────────────────────────────────────────────

    /// Build a ClientConfig with the given display name pointing at an
    /// arbitrary (non-existent) config path — enough for skill-dir mapping tests.
    fn make_client(name: &str, config_path: &std::path::Path) -> ClientConfig {
        ClientConfig {
            name: name.to_string(),
            config_path: config_path.to_path_buf(),
            mcp_key: "mcpServers".to_string(),
            already_configured: false,
        }
    }

    #[test]
    fn client_skill_dir_in_returns_correct_paths() {
        let tmp = temp_root("skill-dir-paths");
        let dummy = tmp.join("dummy.json").into_std_path_buf();
        let home = tmp.as_std_path();

        let cc = make_client("Claude Code", &dummy);
        let cursor = make_client("Cursor", &dummy);
        let ws = make_client("Windsurf", &dummy);
        let cd = make_client("Claude Desktop", &dummy);
        let vsc = make_client("VS Code", &dummy);

        assert_eq!(
            client_skill_dir_in(home, &cc).unwrap(),
            home.join(".claude").join("skills")
        );
        assert_eq!(
            client_skill_dir_in(home, &cursor).unwrap(),
            home.join(".cursor").join("skills")
        );
        assert_eq!(
            client_skill_dir_in(home, &ws).unwrap(),
            home.join(".codeium").join("windsurf").join("skills")
        );
        assert!(
            client_skill_dir_in(home, &cd).is_none(),
            "Claude Desktop is MCP-only"
        );
        assert!(
            client_skill_dir_in(home, &vsc).is_none(),
            "VS Code is MCP-only"
        );

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    #[cfg(target_family = "unix")]
    fn fanout_skill_to_clients_creates_symlinks() {
        let tmp = temp_root("fanout-install");
        fs::create_dir_all(&tmp).unwrap();

        let source = tmp.join("active").into_std_path_buf();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("manifest.json"), r#"{"id":"my-skill"}"#).unwrap();

        let dummy = tmp.join("dummy.json").into_std_path_buf();
        let cc = make_client("Claude Code", &dummy);
        let cursor = make_client("Cursor", &dummy);
        let ws = make_client("Windsurf", &dummy);
        let cd = make_client("Claude Desktop", &dummy);

        let clients: Vec<&ClientConfig> = vec![&cc, &cursor, &ws, &cd];
        let home = tmp.as_std_path();
        let report = fanout_skill_to_clients_in("my-skill", &source, &clients, home).unwrap();

        assert_eq!(
            report.installed.len(),
            3,
            "should install to all three skill-dir clients"
        );
        assert!(report.installed.contains(&"Claude Code".to_string()));
        assert!(report.installed.contains(&"Cursor".to_string()));
        assert!(report.installed.contains(&"Windsurf".to_string()));

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "Claude Desktop");
        assert_eq!(report.skipped[0].1, "MCP config only");

        let cc_link = home.join(".claude").join("skills").join("my-skill");
        let cursor_link = home.join(".cursor").join("skills").join("my-skill");
        let ws_link = home
            .join(".codeium")
            .join("windsurf")
            .join("skills")
            .join("my-skill");

        assert_eq!(fs::read_link(&cc_link).unwrap(), source);
        assert_eq!(fs::read_link(&cursor_link).unwrap(), source);
        assert_eq!(fs::read_link(&ws_link).unwrap(), source);

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    #[cfg(target_family = "unix")]
    fn fanout_skill_to_clients_is_idempotent() {
        let tmp = temp_root("fanout-idem");
        fs::create_dir_all(&tmp).unwrap();

        let source = tmp.join("active").into_std_path_buf();
        fs::create_dir_all(&source).unwrap();

        let dummy = tmp.join("dummy.json").into_std_path_buf();
        let cc = make_client("Claude Code", &dummy);
        let clients: Vec<&ClientConfig> = vec![&cc];
        let home = tmp.as_std_path();

        let r1 = fanout_skill_to_clients_in("test-skill", &source, &clients, home).unwrap();
        assert_eq!(r1.installed, vec!["Claude Code".to_string()]);

        // Second call: already linked — no error, not re-installed.
        let r2 = fanout_skill_to_clients_in("test-skill", &source, &clients, home).unwrap();
        assert!(r2.installed.is_empty(), "should not re-install");
        assert_eq!(r2.skipped.len(), 1);
        assert_eq!(r2.skipped[0].1, "already linked");

        let _ = fs::remove_dir_all(tmp.as_str());
    }

    #[test]
    #[cfg(target_family = "unix")]
    fn fanout_uninstall_skill_removes_symlinks_and_skips_real_dirs() {
        let tmp = temp_root("fanout-uninstall");
        fs::create_dir_all(&tmp).unwrap();

        let source = tmp.join("active").into_std_path_buf();
        fs::create_dir_all(&source).unwrap();

        // Claude Code — symlink we own.
        let cc_skills = tmp.join(".claude").join("skills");
        fs::create_dir_all(&cc_skills).unwrap();
        let cc_entry = cc_skills.join("my-skill");
        std::os::unix::fs::symlink(&source, &cc_entry).unwrap();

        // Cursor — a real directory (user content, must not be removed).
        let cursor_skills = tmp.join(".cursor").join("skills");
        fs::create_dir_all(&cursor_skills).unwrap();
        let cursor_entry = cursor_skills.join("my-skill");
        fs::create_dir_all(&cursor_entry).unwrap();

        // Windsurf — skill entry not present (skills dir also absent is fine).

        let dummy = tmp.join("dummy.json").into_std_path_buf();
        let cc = make_client("Claude Code", &dummy);
        let cursor = make_client("Cursor", &dummy);
        let ws = make_client("Windsurf", &dummy);
        let clients: Vec<&ClientConfig> = vec![&cc, &cursor, &ws];
        let home = tmp.as_std_path();

        let report = fanout_uninstall_skill_in("my-skill", &clients, home);

        assert!(
            report.installed.contains(&"Claude Code".to_string()),
            "symlink should be removed"
        );
        assert!(!cc_entry.exists(), "symlink should no longer exist");

        let cursor_skip = report
            .skipped
            .iter()
            .find(|(n, _)| n == "Cursor")
            .expect("Cursor should be in skipped");
        assert!(
            cursor_skip.1.contains("not a symlink"),
            "real dir should be skipped: {}",
            cursor_skip.1
        );
        assert!(cursor_entry.exists(), "real dir must not be removed");

        let ws_skip = report
            .skipped
            .iter()
            .find(|(n, _)| n == "Windsurf")
            .expect("Windsurf should be in skipped");
        assert_eq!(ws_skip.1, "not present");

        let _ = fs::remove_dir_all(tmp.as_str());
    }
}
