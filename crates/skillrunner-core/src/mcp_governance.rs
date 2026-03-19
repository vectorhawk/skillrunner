//! MCP fleet governance: sync approved servers from registry, build managed-mcp.json.

use crate::{registry::RegistryClient, state::AppState};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, info, warn};

// ── Wire types — must match Python RunnerMcpServersResponse exactly ──────────

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct McpServerEntry {
    pub name: String,
    pub package_source: String,
    pub version_pin: Option<String>,
    pub status: String,
    pub credential_note: Option<String>,
    pub server_config: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct McpServersResponse {
    pub approval_mode: String,
    pub servers: Vec<McpServerEntry>,
}

// ── Managed MCP config (written to disk) ─────────────────────────────────────

/// The `managed-mcp.json` structure written for Claude Code / Cursor.
#[derive(Debug, Serialize, Deserialize)]
pub struct ManagedMcpConfig {
    /// Governance mode: "allowlist" (default) or "exclusive" (enterprise opt-in)
    #[serde(default = "default_governance_mode")]
    pub mcp_governance_mode: String,
    /// MCP server entries keyed by name
    #[serde(rename = "mcpServers")]
    pub mcp_servers: serde_json::Map<String, serde_json::Value>,
}

fn default_governance_mode() -> String {
    "allowlist".to_string()
}

// ── Sync result ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SyncResult {
    pub servers_synced: usize,
    pub servers_blocked: usize,
    pub approval_mode: String,
    pub config_path: String,
}

// ── RegistryClient extension ─────────────────────────────────────────────────

impl RegistryClient {
    /// Fetch the approved MCP server list from the registry.
    pub fn fetch_mcp_servers(&self) -> Result<McpServersResponse> {
        let url = format!(
            "{}/api/runner/mcp-servers",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, "fetching MCP server list");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for MCP servers: {body}");
        }

        resp.json().context("failed to deserialize MCP servers response")
    }
}

impl RegistryClient {
    /// Submit an MCP server access request (portal endpoint).
    pub fn submit_mcp_request(
        &self,
        server_name: &str,
        package_source: Option<&str>,
        auth_token: &str,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/portal/mcp/requests",
            self.base_url.trim_end_matches('/')
        );

        let mut body = serde_json::json!({ "server_name": server_name });
        if let Some(src) = package_source {
            body["package_source"] = serde_json::json!(src);
        }

        let resp = self
            .http
            .post(&url)
            .bearer_auth(auth_token)
            .json(&body)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("request submission failed (HTTP {status}): {body}");
        }

        resp.json().context("failed to deserialize request response")
    }

    /// List the current user's MCP server requests (portal endpoint).
    pub fn list_mcp_requests(&self, auth_token: &str) -> Result<serde_json::Value> {
        let url = format!(
            "{}/portal/mcp/requests",
            self.base_url.trim_end_matches('/')
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(auth_token)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("failed to fetch requests (HTTP {status}): {body}");
        }

        resp.json().context("failed to deserialize requests response")
    }
}

// ── Sync logic ───────────────────────────────────────────────────────────────

/// Sync approved MCP servers from registry and write `managed-mcp.json`.
///
/// - Fetches the server list from registry
/// - Caches in local SQLite for offline resilience
/// - Builds `managed-mcp.json` with SkillRunner always as entry #1
/// - Blocked servers are excluded from the config
pub fn sync_mcp_config(
    state: &AppState,
    registry: &RegistryClient,
    skillrunner_path: &str,
    registry_url: &str,
    dry_run: bool,
) -> Result<SyncResult> {
    // Try to fetch from registry, fall back to cache on failure
    let response = match registry.fetch_mcp_servers() {
        Ok(resp) => {
            // Cache the response
            cache_mcp_config(state, &resp)?;
            resp
        }
        Err(fetch_err) => {
            warn!(error = %fetch_err, "failed to fetch MCP servers, trying cache");
            match load_cached_mcp_config(state)? {
                Some(cached) => {
                    info!("using cached MCP server list");
                    cached
                }
                None => return Err(fetch_err.context("no cached MCP config available")),
            }
        }
    };

    let mut servers = serde_json::Map::new();
    let mut servers_synced = 0;
    let mut servers_blocked = 0;

    // SkillRunner is always entry #1
    let mut skillrunner_env = serde_json::Map::new();
    skillrunner_env.insert(
        "SKILLCLUB_REGISTRY_URL".to_string(),
        serde_json::Value::String(registry_url.to_string()),
    );
    servers.insert(
        "skillclub".to_string(),
        serde_json::json!({
            "command": skillrunner_path,
            "args": ["mcp", "serve"],
            "env": skillrunner_env,
        }),
    );

    // Add approved servers, skip blocked ones
    for entry in &response.servers {
        if entry.status == "blocked" {
            servers_blocked += 1;
            continue;
        }

        if let Some(config) = &entry.server_config {
            servers.insert(entry.name.clone(), config.clone());
        } else {
            // Build default npx config from package_source
            let mut args = vec![
                serde_json::Value::String("-y".to_string()),
                serde_json::Value::String(entry.package_source.clone()),
            ];
            if let Some(pin) = &entry.version_pin {
                // Replace last arg with pinned version
                let pinned = format!("{}@{}", entry.package_source, pin);
                args[1] = serde_json::Value::String(pinned);
            }
            servers.insert(
                entry.name.clone(),
                serde_json::json!({
                    "command": "npx",
                    "args": args,
                }),
            );
        }
        servers_synced += 1;
    }

    let config = ManagedMcpConfig {
        mcp_governance_mode: "allowlist".to_string(),
        mcp_servers: servers,
    };

    // Determine output path
    let config_path = managed_mcp_path();

    if dry_run {
        let json = serde_json::to_string_pretty(&config)?;
        info!("dry run — would write to {}:\n{}", config_path, json);
    } else {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(&config_path).parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&config)?;
        fs::write(&config_path, &json)
            .with_context(|| format!("failed to write {config_path}"))?;
        info!(path = config_path, servers = servers_synced, "wrote managed-mcp.json");
    }

    // Flush audit buffer on each sync
    match flush_audit_buffer(state, registry) {
        Ok(n) if n > 0 => info!(count = n, "flushed audit events during sync"),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "failed to flush audit buffer during sync"),
    }

    Ok(SyncResult {
        servers_synced,
        servers_blocked,
        approval_mode: response.approval_mode,
        config_path,
    })
}

/// Get the path for `managed-mcp.json` (Claude Code format).
fn managed_mcp_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.claude/managed-mcp.json", home)
}

// ── SQLite cache ─────────────────────────────────────────────────────────────

fn ensure_cache_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mcp_config_cache (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            config_json TEXT NOT NULL,
            fetched_at INTEGER NOT NULL
        )",
    )?;
    Ok(())
}

fn cache_mcp_config(state: &AppState, response: &McpServersResponse) -> Result<()> {
    let conn = Connection::open(&state.db_path)?;
    ensure_cache_table(&conn)?;

    let json = serde_json::to_string(response)?;
    let now = unix_now();

    conn.execute(
        "INSERT INTO mcp_config_cache (id, config_json, fetched_at)
         VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET
             config_json = excluded.config_json,
             fetched_at = excluded.fetched_at",
        params![json, now as i64],
    )?;

    Ok(())
}

fn load_cached_mcp_config(state: &AppState) -> Result<Option<McpServersResponse>> {
    let conn = Connection::open(&state.db_path)?;
    ensure_cache_table(&conn)?;

    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT config_json, fetched_at FROM mcp_config_cache WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    match row {
        Some((json, fetched_at)) => {
            // 7-day offline grace window
            const GRACE_SECONDS: u64 = 7 * 86400;
            let now = unix_now();
            if now > fetched_at as u64 + GRACE_SECONDS {
                warn!("cached MCP config is older than 7 days, ignoring");
                return Ok(None);
            }
            let resp: McpServersResponse = serde_json::from_str(&json)?;
            Ok(Some(resp))
        }
        None => Ok(None),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_secs()
}

// ── Audit buffer ─────────────────────────────────────────────────────────────

/// A single audit event to be buffered locally and batch-uploaded.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuditEvent {
    pub server_name: Option<String>,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub machine_id: Option<String>,
    pub event_type: String,
    pub tool_name: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub org_id: String,
}

fn ensure_audit_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mcp_audit_buffer (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_json TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
    )?;
    Ok(())
}

/// Buffer an audit event in local SQLite.
pub fn buffer_audit_event(state: &AppState, event: &AuditEvent) -> Result<()> {
    let conn = Connection::open(&state.db_path)?;
    ensure_audit_table(&conn)?;

    let json = serde_json::to_string(event)?;
    let now = unix_now();

    conn.execute(
        "INSERT INTO mcp_audit_buffer (event_json, created_at) VALUES (?1, ?2)",
        params![json, now as i64],
    )?;

    debug!(event_type = %event.event_type, "buffered audit event");
    Ok(())
}

/// Flush buffered audit events to the registry. Returns count of events uploaded.
pub fn flush_audit_buffer(state: &AppState, registry: &RegistryClient) -> Result<usize> {
    let conn = Connection::open(&state.db_path)?;
    ensure_audit_table(&conn)?;

    // Read all buffered events
    let mut stmt = conn.prepare(
        "SELECT id, event_json FROM mcp_audit_buffer ORDER BY id ASC LIMIT 500",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        return Ok(0);
    }

    let mut events: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    let mut ids: Vec<i64> = Vec::with_capacity(rows.len());

    for (id, json) in &rows {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json) {
            events.push(val);
            ids.push(*id);
        }
    }

    if events.is_empty() {
        return Ok(0);
    }

    // Upload to registry
    registry.upload_audit_batch(&events)?;

    // Delete uploaded events
    let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("DELETE FROM mcp_audit_buffer WHERE id IN ({placeholders})");
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    conn.execute(&sql, rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())))?;

    info!(count = ids.len(), "flushed audit events to registry");
    Ok(ids.len())
}

impl RegistryClient {
    /// Upload a batch of audit events to the registry.
    pub fn upload_audit_batch(&self, events: &[serde_json::Value]) -> Result<()> {
        let url = format!(
            "{}/api/runner/mcp-audit",
            self.base_url.trim_end_matches('/')
        );

        let body = serde_json::json!({ "events": events });

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .with_context(|| format!("failed to upload audit batch to {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("audit upload failed (HTTP {status}): {body}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use camino::Utf8PathBuf;
    use mockito::Server;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("mcp-gov-test-{label}-{nanos}")),
        )
        .unwrap()
    }

    #[test]
    fn fetch_mcp_servers_parses_response() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/api/runner/mcp-servers")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "approval_mode": "catalog_only",
                    "servers": [
                        {
                            "name": "GitHub MCP",
                            "package_source": "@modelcontextprotocol/server-github",
                            "version_pin": null,
                            "status": "approved",
                            "credential_note": "Requires GITHUB_TOKEN"
                        },
                        {
                            "name": "Playwright",
                            "package_source": "@anthropic/mcp-server-playwright",
                            "version_pin": "0.7.2",
                            "status": "blocked"
                        }
                    ]
                }"#,
            )
            .create();

        let client = RegistryClient::new(server.url());
        let resp = client.fetch_mcp_servers().unwrap();

        assert_eq!(resp.approval_mode, "catalog_only");
        assert_eq!(resp.servers.len(), 2);
        assert_eq!(resp.servers[0].name, "GitHub MCP");
        assert_eq!(resp.servers[0].status, "approved");
        assert_eq!(resp.servers[1].status, "blocked");
        mock.assert();
    }

    #[test]
    fn cache_roundtrip() {
        let root = temp_root("cache-roundtrip");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let response = McpServersResponse {
            approval_mode: "trust".to_string(),
            servers: vec![McpServerEntry {
                name: "test-server".to_string(),
                package_source: "test-pkg".to_string(),
                version_pin: None,
                status: "approved".to_string(),
                credential_note: None,
                server_config: None,
            }],
        };

        cache_mcp_config(&state, &response).unwrap();
        let cached = load_cached_mcp_config(&state).unwrap();

        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.approval_mode, "trust");
        assert_eq!(cached.servers.len(), 1);
        assert_eq!(cached.servers[0].name, "test-server");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_dry_run_does_not_write_file() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/api/runner/mcp-servers")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "approval_mode": "trust",
                    "servers": [
                        {
                            "name": "test-mcp",
                            "package_source": "test-pkg",
                            "status": "approved"
                        }
                    ]
                }"#,
            )
            .create();

        let root = temp_root("sync-dry");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let registry = RegistryClient::new(server.url());

        let result = sync_mcp_config(
            &state,
            &registry,
            "/usr/local/bin/skillrunner",
            &server.url(),
            true, // dry run
        )
        .unwrap();

        assert_eq!(result.servers_synced, 1);
        assert_eq!(result.servers_blocked, 0);
        assert_eq!(result.approval_mode, "trust");
        mock.assert();

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn audit_buffer_roundtrip() {
        let root = temp_root("audit-buf");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let event = AuditEvent {
            server_name: Some("test-mcp".to_string()),
            user_id: Some("user-1".to_string()),
            user_email: Some("user@example.com".to_string()),
            machine_id: Some("machine-abc".to_string()),
            event_type: "tool_called".to_string(),
            tool_name: Some("read_file".to_string()),
            metadata: None,
            org_id: "default".to_string(),
        };

        buffer_audit_event(&state, &event).unwrap();
        buffer_audit_event(&state, &event).unwrap();

        // Verify events are in the buffer
        let conn = Connection::open(&state.db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mcp_audit_buffer", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn flush_audit_uploads_and_clears() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/api/runner/mcp-audit")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"created": 2}"#)
            .create();

        let root = temp_root("audit-flush");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let registry = RegistryClient::new(server.url());

        // Buffer two events
        let event = AuditEvent {
            server_name: Some("github-mcp".to_string()),
            user_id: None,
            user_email: None,
            machine_id: None,
            event_type: "config_synced".to_string(),
            tool_name: None,
            metadata: None,
            org_id: "default".to_string(),
        };
        buffer_audit_event(&state, &event).unwrap();
        buffer_audit_event(&state, &event).unwrap();

        // Flush
        let flushed = flush_audit_buffer(&state, &registry).unwrap();
        assert_eq!(flushed, 2);
        mock.assert();

        // Buffer should be empty
        let conn = Connection::open(&state.db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mcp_audit_buffer", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn flush_empty_buffer_is_noop() {
        let root = temp_root("audit-empty");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let registry = RegistryClient::new("http://unused:9999".to_string());

        let flushed = flush_audit_buffer(&state, &registry).unwrap();
        assert_eq!(flushed, 0);

        let _ = fs::remove_dir_all(&root);
    }
}
