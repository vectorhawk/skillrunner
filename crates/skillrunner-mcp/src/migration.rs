//! Migration of existing MCP server entries from AI client configs into
//! SkillRunner's `backends.yaml` aggregator config.
//!
//! The high-level flow:
//! 1. Scan each detected AI client config for MCP server entries not managed
//!    by SkillRunner (key != `"skillrunner"`, not `disabled: true`).
//! 2. Convert each entry to a [`BackendEntry`].
//! 3. Deduplicate across clients (same server name = one entry).
//! 4. Back up the original client config to `{state_dir}/backups/`.
//! 5. Append converted entries to `{state_dir}/backends.yaml`.
//! 6. Remove the migrated entries from each client config (leave `skillrunner`
//!    and any disabled entries in place so they self-document).
//! 7. Return a [`MigrationReport`] describing what happened.

use crate::backends_config::{append_backends, BackendEntry};
use crate::setup::ClientConfig;
use anyhow::{Context, Result};
use skillrunner_core::state::AppState;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

// ── Public types ───────────────────────────────────────────────────────────────

/// Summary of a single server that was migrated.
#[derive(Debug, Clone)]
pub struct MigratedServer {
    /// Key name in the client config (e.g. `"github"`).
    pub server_name: String,
    /// Which client the entry came from.
    pub client_name: String,
    /// Transport detected: `"stdio"` or `"http"`.
    pub transport: String,
}

/// Summary of a server that was skipped and why.
#[derive(Debug, Clone)]
pub struct SkippedServer {
    pub server_name: String,
    pub client_name: String,
    pub reason: SkipReason,
}

/// Reason a server was not migrated.
#[derive(Debug, Clone)]
pub enum SkipReason {
    /// The entry had `"disabled": true`.
    Disabled,
    /// A server with the same name was already present in `backends.yaml`.
    AlreadyPresent,
    /// The entry existed in multiple clients and was deduplicated.
    Duplicate,
    /// The entry was unparseable (malformed JSON structure).
    Malformed,
}

/// Path to a client config backup file plus metadata.
#[derive(Debug, Clone)]
pub struct BackupInfo {
    /// Full path to the backup file.
    pub path: PathBuf,
    /// Original config file that was backed up.
    pub original_path: PathBuf,
    /// Which client owns this config.
    pub client_name: String,
    /// Timestamp suffix used in the filename (RFC-3339-like, filesystem-safe).
    pub timestamp: String,
}

/// Full report returned by [`migrate_existing_servers`].
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub migrated: Vec<MigratedServer>,
    pub skipped: Vec<SkippedServer>,
    pub backups: Vec<BackupInfo>,
}

// ── Migration entry point ──────────────────────────────────────────────────────

/// Migrate MCP server entries from all detected AI client configs into
/// `backends.yaml`.
///
/// - `state` — SkillRunner state directory (determines backup dir and
///   `backends.yaml` location).
/// - `clients` — detected AI client configs (from [`crate::setup::detect_ai_clients`]).
///
/// The function is idempotent: running it twice produces no duplicates and
/// leaves the second backup alongside the first.
pub fn migrate_existing_servers(
    state: &AppState,
    clients: &[ClientConfig],
) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();

    // Collect per-client candidate entries, tracking which clients contribute each name.
    // client_map: server_name -> Vec<(client_name, ClientConfig, raw JSON value)>
    let mut client_map: HashMap<String, Vec<(String, PathBuf, serde_json::Value)>> = HashMap::new();

    for client in clients {
        if !client.config_path.exists() {
            debug!(client = %client.name, "config path does not exist — skipping");
            continue;
        }

        let text = match fs::read_to_string(&client.config_path) {
            Ok(t) => t,
            Err(e) => {
                warn!(client = %client.name, error = %e, "could not read client config — skipping");
                continue;
            }
        };

        let config: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!(client = %client.name, error = %e, "could not parse client config JSON — skipping");
                continue;
            }
        };

        let Some(servers) = config.get(&client.mcp_key).and_then(|v| v.as_object()) else {
            debug!(client = %client.name, key = %client.mcp_key, "no MCP servers key found");
            continue;
        };

        for (key, value) in servers {
            // Skip the SkillRunner entry — it is what we just installed.
            if key == "skillrunner" {
                continue;
            }
            // Skip disabled entries — leave them in the client config.
            if value
                .get("disabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                report.skipped.push(SkippedServer {
                    server_name: key.clone(),
                    client_name: client.name.clone(),
                    reason: SkipReason::Disabled,
                });
                continue;
            }
            client_map.entry(key.clone()).or_default().push((
                client.name.clone(),
                client.config_path.clone(),
                value.clone(),
            ));
        }
    }

    if client_map.is_empty() {
        debug!("no non-skillrunner MCP servers found in any client config");
        return Ok(report);
    }

    // Convert to BackendEntry list; deduplicate across clients.
    let mut entries_to_add: Vec<BackendEntry> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();

    // Sort for deterministic ordering.
    let mut server_names: Vec<String> = client_map.keys().cloned().collect();
    server_names.sort();

    for name in &server_names {
        let occurrences = &client_map[name];

        // Use the first occurrence; mark subsequent as duplicates.
        let (client_name, _path, value) = &occurrences[0];

        if seen_names.contains(name) {
            continue;
        }
        seen_names.insert(name.clone());

        // Mark duplicates from other clients.
        for (dup_client, _, _) in occurrences.iter().skip(1) {
            report.skipped.push(SkippedServer {
                server_name: name.clone(),
                client_name: dup_client.clone(),
                reason: SkipReason::Duplicate,
            });
        }

        match json_value_to_backend_entry(name, value) {
            Some(entry) => {
                report.migrated.push(MigratedServer {
                    server_name: name.clone(),
                    client_name: client_name.clone(),
                    transport: entry.transport.clone(),
                });
                entries_to_add.push(entry);
            }
            None => {
                warn!(server = %name, "could not convert MCP server entry to BackendEntry — skipping");
                report.skipped.push(SkippedServer {
                    server_name: name.clone(),
                    client_name: client_name.clone(),
                    reason: SkipReason::Malformed,
                });
            }
        }
    }

    if entries_to_add.is_empty() {
        return Ok(report);
    }

    // Persist to backends.yaml (append_backends handles dedup by name).
    append_backends(state, entries_to_add).context("failed to append entries to backends.yaml")?;

    // Back up and strip migrated entries from each affected client config.
    let migrated_names: HashSet<String> = report
        .migrated
        .iter()
        .map(|m| m.server_name.clone())
        .collect();

    // Collect unique config paths that need updating.
    let mut paths_to_update: Vec<(String, PathBuf, String)> = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    for client in clients {
        if !client.config_path.exists() {
            continue;
        }
        // Only process clients that contributed at least one migrated server.
        let contributed = client_map.values().any(|occ| {
            occ.iter()
                .any(|(cn, p, _)| cn == &client.name && p == &client.config_path)
        });
        if !contributed {
            continue;
        }
        if seen_paths.contains(&client.config_path) {
            continue;
        }
        seen_paths.insert(client.config_path.clone());
        paths_to_update.push((
            client.name.clone(),
            client.config_path.clone(),
            client.mcp_key.clone(),
        ));
    }

    let backups_dir = state.root_dir.join("backups");
    fs::create_dir_all(&backups_dir).context("failed to create backups directory")?;

    let timestamp = current_timestamp();

    for (client_name, config_path, mcp_key) in &paths_to_update {
        let backup_info = backup_config(
            config_path,
            backups_dir.as_std_path(),
            client_name,
            &timestamp,
        )
        .with_context(|| format!("failed to back up config for {client_name}"))?;
        report.backups.push(backup_info);

        strip_migrated_servers(config_path, mcp_key, &migrated_names)
            .with_context(|| format!("failed to strip migrated servers from {client_name}"))?;
    }

    info!(
        migrated = report.migrated.len(),
        skipped = report.skipped.len(),
        backups = report.backups.len(),
        "migration complete"
    );
    Ok(report)
}

// ── Restore ────────────────────────────────────────────────────────────────────

/// Copy a backup file back to its original location.
///
/// The original path is inferred from the backup filename convention:
/// `{client_name}-{timestamp}.json` stored alongside a sidecar
/// `{client_name}-{timestamp}.origin` file containing the original path.
pub fn restore_backup(backup_path: &Path) -> Result<()> {
    let origin_path = backup_path.with_extension("origin");
    let original_dest = fs::read_to_string(&origin_path)
        .with_context(|| format!("failed to read origin sidecar {}", origin_path.display()))?;
    let original_dest = original_dest.trim();

    // Ensure parent directories of the destination exist.
    if let Some(parent) = Path::new(original_dest).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dirs for {original_dest}"))?;
    }

    fs::copy(backup_path, original_dest)
        .with_context(|| format!("failed to copy backup to {original_dest}"))?;

    info!(
        backup = %backup_path.display(),
        destination = %original_dest,
        "restored backup"
    );
    Ok(())
}

/// List all backup files in `{state_dir}/backups/`.
pub fn list_backups(state: &AppState) -> Result<Vec<BackupInfo>> {
    let backups_dir = state.root_dir.join("backups");
    if !backups_dir.exists() {
        return Ok(vec![]);
    }

    let mut result = Vec::new();
    let entries = fs::read_dir(&backups_dir)
        .with_context(|| format!("failed to read backups directory {backups_dir}"))?;

    for entry in entries {
        let entry = entry.context("failed to read directory entry")?;
        let path = entry.path();

        // Only look at .json files; skip .origin sidecars.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let origin_path = path.with_extension("origin");
        let original_path = if origin_path.exists() {
            let dest = fs::read_to_string(&origin_path).unwrap_or_default();
            PathBuf::from(dest.trim())
        } else {
            PathBuf::new()
        };

        // Parse client_name and timestamp from filename: {client_name}-{timestamp}.json
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let (client_name, timestamp) = split_backup_stem(&stem);

        result.push(BackupInfo {
            path,
            original_path,
            client_name,
            timestamp,
        });
    }

    result.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    Ok(result)
}

// ── Internal helpers ───────────────────────────────────────────────────────────

/// Convert a raw JSON value from a client config's `mcpServers` map into a
/// [`BackendEntry`]. Returns `None` if the entry is malformed.
fn json_value_to_backend_entry(name: &str, value: &serde_json::Value) -> Option<BackendEntry> {
    // HTTP transport: entry has a "url" field.
    if let Some(url) = value.get("url").and_then(|v| v.as_str()) {
        return Some(BackendEntry {
            name: name.to_string(),
            server_id: Some(name.to_string()),
            transport: "http".to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.to_string()),
            priority: 50,
        });
    }

    // Stdio transport: entry has a "command" field.
    if let Some(command) = value.get("command").and_then(|v| v.as_str()) {
        let args: Vec<String> = value
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let env: HashMap<String, String> = value
            .get("env")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        return Some(BackendEntry {
            name: name.to_string(),
            server_id: Some(name.to_string()),
            transport: "stdio".to_string(),
            command: Some(command.to_string()),
            args,
            env,
            url: None,
            priority: 50,
        });
    }

    None
}

/// Back up a config file to the backups directory and write a `.origin` sidecar.
/// Returns a [`BackupInfo`] describing the backup.
fn backup_config(
    config_path: &Path,
    backups_dir: &Path,
    client_name: &str,
    timestamp: &str,
) -> Result<BackupInfo> {
    let safe_client = client_name.replace(' ', "-").to_lowercase();
    let filename = format!("{safe_client}-{timestamp}.json");
    let backup_path = backups_dir.join(&filename);
    let origin_path = backups_dir.join(format!("{safe_client}-{timestamp}.origin"));

    fs::copy(config_path, &backup_path).with_context(|| {
        format!(
            "failed to copy {} to {}",
            config_path.display(),
            backup_path.display()
        )
    })?;

    fs::write(&origin_path, config_path.display().to_string())
        .with_context(|| format!("failed to write origin sidecar {}", origin_path.display()))?;

    debug!(
        source = %config_path.display(),
        backup = %backup_path.display(),
        "backed up client config"
    );

    Ok(BackupInfo {
        path: backup_path,
        original_path: config_path.to_path_buf(),
        client_name: client_name.to_string(),
        timestamp: timestamp.to_string(),
    })
}

/// Remove the given server keys from a client config's MCP servers map.
/// Writes the result back atomically.
fn strip_migrated_servers(
    config_path: &Path,
    mcp_key: &str,
    names_to_remove: &HashSet<String>,
) -> Result<()> {
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;

    let mut config: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;

    let Some(servers) = config
        .as_object_mut()
        .and_then(|obj| obj.get_mut(mcp_key))
        .and_then(|v| v.as_object_mut())
    else {
        // Nothing to strip if the key is missing.
        return Ok(());
    };

    for name in names_to_remove {
        servers.remove(name);
    }

    let formatted = serde_json::to_string_pretty(&config)
        .context("failed to serialize updated client config")?;

    // Write via temp path alongside the real file.
    let tmp_path = config_path.with_extension("json.tmp");
    fs::write(&tmp_path, &formatted)
        .with_context(|| format!("failed to write temp file {}", tmp_path.display()))?;
    fs::rename(&tmp_path, config_path)
        .with_context(|| format!("failed to rename temp file to {}", config_path.display()))?;

    debug!(path = %config_path.display(), removed = names_to_remove.len(), "stripped migrated servers from client config");
    Ok(())
}

/// Return a filesystem-safe timestamp string: `YYYYMMDD-HHMMSS`.
fn current_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Manual decomposition to avoid pulling in chrono.
    let seconds = secs % 60;
    let minutes = (secs / 60) % 60;
    let hours = (secs / 3600) % 24;
    let days_since_epoch = secs / 86400;

    // Gregorian calendar approximation from days since Unix epoch (1970-01-01).
    let (year, month, day) = days_to_ymd(days_since_epoch);

    format!("{year:04}{month:02}{day:02}-{hours:02}{minutes:02}{seconds:02}")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // 400-year cycle has 97 leap years = 146097 days.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Split a backup filename stem into (client_name, timestamp).
/// Expects format: `{client-name}-{YYYYMMDD-HHMMSS}`.
fn split_backup_stem(stem: &str) -> (String, String) {
    // Timestamp pattern: 8 digits, dash, 6 digits at the end.
    // Find the last occurrence of a '-' that precedes a 15-char timestamp suffix.
    if stem.len() > 15 {
        let potential_ts_start = stem.len() - 15;
        let (client_part, ts_part) = stem.split_at(potential_ts_start);
        // Remove trailing '-' from client_part.
        let client = client_part.trim_end_matches('-').to_string();
        return (client, ts_part.to_string());
    }
    (stem.to_string(), String::new())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use skillrunner_core::state::AppState;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_state(label: &str) -> AppState {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sr-migration-test-{label}-{nanos}"));
        let root = Utf8PathBuf::from_path_buf(path).unwrap();
        AppState::bootstrap_in(root).unwrap()
    }

    fn cleanup(state: &AppState) {
        let _ = fs::remove_dir_all(&state.root_dir);
    }

    fn write_client_config(path: &camino::Utf8PathBuf, json: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path.as_std_path(), json).unwrap();
    }

    fn make_client(name: &str, config_path: camino::Utf8PathBuf) -> ClientConfig {
        ClientConfig {
            name: name.to_string(),
            config_path: config_path.into_std_path_buf(),
            mcp_key: "mcpServers".to_string(),
            already_configured: true,
        }
    }

    // ── json_value_to_backend_entry ──────────────────────────────────────────

    #[test]
    fn converts_stdio_entry() {
        let value = serde_json::json!({
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-github"],
            "env": { "GITHUB_TOKEN": "ghp_xxx" }
        });
        let entry = json_value_to_backend_entry("github", &value).unwrap();
        assert_eq!(entry.name, "github");
        assert_eq!(entry.transport, "stdio");
        assert_eq!(entry.command.as_deref(), Some("npx"));
        assert_eq!(
            entry.args,
            vec!["-y", "@modelcontextprotocol/server-github"]
        );
        assert_eq!(
            entry.env.get("GITHUB_TOKEN").map(String::as_str),
            Some("ghp_xxx")
        );
        assert!(entry.url.is_none());
    }

    #[test]
    fn converts_http_entry() {
        let value = serde_json::json!({ "url": "http://localhost:3001/mcp" });
        let entry = json_value_to_backend_entry("sentry", &value).unwrap();
        assert_eq!(entry.name, "sentry");
        assert_eq!(entry.transport, "http");
        assert_eq!(entry.url.as_deref(), Some("http://localhost:3001/mcp"));
        assert!(entry.command.is_none());
    }

    #[test]
    fn returns_none_for_malformed_entry() {
        let value = serde_json::json!({ "type": "unknown" });
        assert!(json_value_to_backend_entry("bad", &value).is_none());
    }

    // ── migrate_existing_servers ─────────────────────────────────────────────

    #[test]
    fn migrate_converts_three_servers_from_claude_code() {
        let state = temp_state("three-servers");
        let config_path = state.root_dir.join("claude.json");

        write_client_config(
            &config_path,
            r#"{
                "mcpServers": {
                    "skillrunner": { "command": "skillrunner", "args": ["mcp", "serve"] },
                    "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": { "GITHUB_TOKEN": "ghp_x" } },
                    "sentry": { "url": "http://localhost:3001/mcp" },
                    "files": { "command": "node", "args": ["files-server.js"] }
                }
            }"#,
        );

        let clients = vec![make_client("Claude Code", config_path.clone())];
        let report = migrate_existing_servers(&state, &clients).unwrap();

        assert_eq!(
            report.migrated.len(),
            3,
            "should migrate github, sentry, files"
        );
        assert_eq!(report.skipped.len(), 0);
        assert_eq!(report.backups.len(), 1);

        // Verify backends.yaml was written.
        let backends_path = state.root_dir.join("backends.yaml");
        assert!(backends_path.exists());
        let contents = fs::read_to_string(&backends_path).unwrap();
        assert!(contents.contains("github"));
        assert!(contents.contains("sentry"));
        assert!(contents.contains("files"));

        // Verify client config no longer has migrated servers.
        let updated = fs::read_to_string(&config_path).unwrap();
        let updated_json: serde_json::Value = serde_json::from_str(&updated).unwrap();
        let servers = updated_json["mcpServers"].as_object().unwrap();
        assert!(
            servers.contains_key("skillrunner"),
            "skillrunner must remain"
        );
        assert!(!servers.contains_key("github"), "github should be removed");
        assert!(!servers.contains_key("sentry"), "sentry should be removed");
        assert!(!servers.contains_key("files"), "files should be removed");

        cleanup(&state);
    }

    #[test]
    fn migrate_skips_disabled_servers() {
        let state = temp_state("disabled");
        let config_path = state.root_dir.join("claude.json");

        write_client_config(
            &config_path,
            r#"{
                "mcpServers": {
                    "github": { "command": "npx", "args": ["-y", "@mcp/github"], "disabled": true },
                    "sentry": { "url": "http://localhost:3001/mcp" }
                }
            }"#,
        );

        let clients = vec![make_client("Claude Code", config_path.clone())];
        let report = migrate_existing_servers(&state, &clients).unwrap();

        assert_eq!(report.migrated.len(), 1, "only sentry should be migrated");
        assert_eq!(
            report.skipped.len(),
            1,
            "github should be skipped as disabled"
        );
        assert!(matches!(report.skipped[0].reason, SkipReason::Disabled));
        assert_eq!(report.skipped[0].server_name, "github");

        cleanup(&state);
    }

    #[test]
    fn migrate_deduplicates_across_clients() {
        let state = temp_state("dedup");
        let claude_config = state.root_dir.join("claude.json");
        let cursor_config = state.root_dir.join("cursor.json");

        write_client_config(
            &claude_config,
            r#"{ "mcpServers": { "github": { "command": "npx", "args": ["-y", "@mcp/github"] } } }"#,
        );
        write_client_config(
            &cursor_config,
            r#"{ "mcpServers": { "github": { "command": "npx", "args": ["-y", "@mcp/github"] } } }"#,
        );

        let clients = vec![
            make_client("Claude Code", claude_config.clone()),
            make_client("Cursor", cursor_config.clone()),
        ];
        let report = migrate_existing_servers(&state, &clients).unwrap();

        assert_eq!(report.migrated.len(), 1, "github should appear once");
        let dup_skipped = report
            .skipped
            .iter()
            .filter(|s| matches!(s.reason, SkipReason::Duplicate))
            .count();
        assert_eq!(
            dup_skipped, 1,
            "github from Cursor should be marked as duplicate"
        );

        // Only one backends.yaml entry.
        let contents = fs::read_to_string(state.root_dir.join("backends.yaml")).unwrap();
        assert_eq!(contents.matches("name: github").count(), 1);

        cleanup(&state);
    }

    #[test]
    fn migrate_is_idempotent_no_duplicates_in_backends_yaml() {
        let state = temp_state("idempotent");
        let config_path = state.root_dir.join("claude.json");

        write_client_config(
            &config_path,
            r#"{ "mcpServers": { "github": { "command": "npx", "args": [] } } }"#,
        );

        let clients = vec![make_client("Claude Code", config_path.clone())];
        // Run once.
        migrate_existing_servers(&state, &clients).unwrap();

        // After first run the client config has github removed and backends.yaml has it.
        // Restore the client config to simulate running again on fresh data.
        write_client_config(
            &config_path,
            r#"{ "mcpServers": { "github": { "command": "npx", "args": [] } } }"#,
        );
        let report2 = migrate_existing_servers(&state, &clients).unwrap();

        // append_backends should skip the duplicate.
        let contents = fs::read_to_string(state.root_dir.join("backends.yaml")).unwrap();
        assert_eq!(
            contents.matches("name: github").count(),
            1,
            "github should appear exactly once even after two runs"
        );
        // The server was attempted but already in backends.yaml, so migrated list
        // may be empty or present depending on whether append_backends filtered it.
        // Either way, no panic and no duplicate YAML entry is the contract.
        let _ = report2;

        cleanup(&state);
    }

    #[test]
    fn migrate_handles_empty_config_gracefully() {
        let state = temp_state("empty-config");
        let config_path = state.root_dir.join("claude.json");
        write_client_config(&config_path, r#"{ "mcpServers": {} }"#);

        let clients = vec![make_client("Claude Code", config_path)];
        let report = migrate_existing_servers(&state, &clients).unwrap();

        assert!(report.migrated.is_empty());
        assert!(report.skipped.is_empty());
        assert!(report.backups.is_empty());

        cleanup(&state);
    }

    #[test]
    fn migrate_handles_missing_config_gracefully() {
        let state = temp_state("missing-config");
        let config_path = state.root_dir.join("nonexistent.json");

        let clients = vec![make_client("Claude Code", config_path)];
        let report = migrate_existing_servers(&state, &clients).unwrap();

        assert!(report.migrated.is_empty());
        cleanup(&state);
    }

    #[test]
    fn migrate_handles_config_with_only_skillrunner() {
        let state = temp_state("only-skillrunner");
        let config_path = state.root_dir.join("claude.json");
        write_client_config(
            &config_path,
            r#"{ "mcpServers": { "skillrunner": { "command": "skillrunner", "args": ["mcp", "serve"] } } }"#,
        );

        let clients = vec![make_client("Claude Code", config_path)];
        let report = migrate_existing_servers(&state, &clients).unwrap();

        assert!(report.migrated.is_empty());
        assert!(report.backups.is_empty());

        cleanup(&state);
    }

    // ── backup / restore ─────────────────────────────────────────────────────

    #[test]
    fn backup_creates_json_and_origin_sidecar() {
        let state = temp_state("backup-sidecar");
        let config_path = state.root_dir.join("claude.json");
        fs::write(&config_path, r#"{"mcpServers":{}}"#).unwrap();

        let backups_dir = state.root_dir.join("backups");
        fs::create_dir_all(&backups_dir).unwrap();

        let info = backup_config(
            config_path.as_std_path(),
            backups_dir.as_std_path(),
            "Claude Code",
            "20260407-120000",
        )
        .unwrap();

        assert!(info.path.exists(), "backup file should exist");
        let origin = info.path.with_extension("origin");
        assert!(origin.exists(), "origin sidecar should exist");

        let origin_content = fs::read_to_string(&origin).unwrap();
        assert_eq!(
            origin_content.trim(),
            config_path.as_std_path().display().to_string()
        );

        cleanup(&state);
    }

    #[test]
    fn restore_backup_copies_file_back() {
        let state = temp_state("restore");
        let original_path = state.root_dir.join("original.json");
        fs::write(&original_path, r#"{"original":true}"#).unwrap();

        let backups_dir = state.root_dir.join("backups");
        fs::create_dir_all(&backups_dir).unwrap();
        let info = backup_config(
            original_path.as_std_path(),
            backups_dir.as_std_path(),
            "Test Client",
            "20260407-130000",
        )
        .unwrap();

        // Overwrite the original to simulate it being modified.
        fs::write(original_path.as_std_path(), r#"{"modified":true}"#).unwrap();

        restore_backup(&info.path).unwrap();

        let restored = fs::read_to_string(&original_path).unwrap();
        assert!(
            restored.contains("\"original\":true"),
            "restored content should match backup"
        );

        cleanup(&state);
    }

    #[test]
    fn list_backups_returns_sorted_backups() {
        let state = temp_state("list-backups");
        let backups_dir = state.root_dir.join("backups");
        fs::create_dir_all(&backups_dir).unwrap();

        // Create two fake backup files.
        for ts in &["20260407-100000", "20260407-110000"] {
            let path = backups_dir.join(format!("claude-code-{ts}.json"));
            let origin = backups_dir.join(format!("claude-code-{ts}.origin"));
            fs::write(&path, "{}").unwrap();
            fs::write(&origin, "/tmp/fake.json").unwrap();
        }

        let backups = list_backups(&state).unwrap();
        assert_eq!(backups.len(), 2);
        assert!(
            backups[0].timestamp < backups[1].timestamp,
            "should be sorted chronologically"
        );

        cleanup(&state);
    }

    // ── timestamp helpers ─────────────────────────────────────────────────────

    #[test]
    fn timestamp_has_correct_format() {
        let ts = current_timestamp();
        assert_eq!(ts.len(), 15, "format should be YYYYMMDD-HHMMSS (15 chars)");
        assert_eq!(&ts[8..9], "-", "ninth char should be hyphen");
    }

    #[test]
    fn split_backup_stem_parses_correctly() {
        let (client, ts) = split_backup_stem("claude-code-20260407-120000");
        assert_eq!(client, "claude-code");
        assert_eq!(ts, "20260407-120000");
    }
}
