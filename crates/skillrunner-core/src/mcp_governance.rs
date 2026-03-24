//! MCP fleet governance: approved server fetching, caching, and audit buffering.
//!
//! The file-writing `sync_mcp_config` path has been removed. The aggregator
//! (`skillrunner-mcp::aggregator::BackendRegistry`) now handles all server
//! management internally — no config files are written after initial setup.

use crate::{registry::RegistryClient, state::AppState};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
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

    // ── Aggregator-specific fields (G2) ──────────────────────────────────────
    /// Stable identifier used for tool namespacing (e.g. `"github"` →
    /// tools become `github__create_issue`). Defaults to `name` when absent.
    #[serde(default)]
    pub server_id: Option<String>,

    /// Transport type for the aggregator to use when connecting upstream.
    /// One of `"stdio"`, `"http"`, `"gateway"`. Defaults to `"http"`.
    #[serde(default)]
    pub transport_type: Option<String>,

    /// For `gateway` transport: the upstream gateway URL to connect to.
    #[serde(default)]
    pub gateway_url: Option<String>,

    /// Tool visibility policy. One of `"all"`, `"curated"`, `"on_demand"`.
    /// Defaults to `"all"` if absent.
    #[serde(default)]
    pub tool_visibility: Option<String>,

    /// For `tool_visibility = "curated"`: the list of tool names to surface.
    #[serde(default)]
    pub visible_tools: Option<Vec<String>>,

    /// Admin-assigned priority (higher = more important for tool budget).
    /// Governance tools use priority 100; default server priority is 50.
    #[serde(default)]
    pub priority: Option<u8>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct McpServersResponse {
    pub approval_mode: String,
    pub servers: Vec<McpServerEntry>,
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

// ── Public fetch helper ───────────────────────────────────────────────────────

/// Fetch the approved MCP server list, with SQLite cache fallback.
///
/// This is the public entry point for the aggregator (`BackendRegistry`) to
/// retrieve which servers should be proxied. It is intentionally separate from
/// the file-writing `sync_mcp_config` path.
///
/// Behaviour:
/// - On network success: updates the SQLite cache and returns the fresh list.
/// - On network failure: returns the cached list if it is within the 7-day
///   offline grace window, otherwise returns an error.
pub fn fetch_approved_servers(
    state: &AppState,
    registry: &RegistryClient,
) -> Result<McpServersResponse> {
    match registry.fetch_mcp_servers() {
        Ok(resp) => {
            cache_mcp_config(state, &resp)?;
            Ok(resp)
        }
        Err(fetch_err) => {
            warn!(error = %fetch_err, "failed to fetch approved servers, trying cache");
            match load_cached_mcp_config(state)? {
                Some(cached) => {
                    info!("using cached MCP server list (offline mode)");
                    Ok(cached)
                }
                None => Err(fetch_err.context("no cached server list available")),
            }
        }
    }
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
    use std::fs;
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
                server_id: None,
                transport_type: None,
                gateway_url: None,
                tool_visibility: None,
                visible_tools: None,
                priority: None,
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

    #[test]
    fn submit_mcp_request_sends_correct_payload() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/portal/mcp/requests")
            .match_header("authorization", "Bearer test-token")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{
                "id": "req-123",
                "server_name": "slack-mcp",
                "status": "approved",
                "reviewed_by": "system/auto-approve"
            }"#)
            .create();

        let client = RegistryClient::new(server.url());
        let result = client
            .submit_mcp_request("slack-mcp", Some("@modelcontextprotocol/server-slack"), "test-token")
            .unwrap();

        assert_eq!(result["status"].as_str().unwrap(), "approved");
        assert_eq!(result["server_name"].as_str().unwrap(), "slack-mcp");
        mock.assert();
    }

    #[test]
    fn submit_mcp_request_returns_error_on_401() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/portal/mcp/requests")
            .with_status(401)
            .with_body(r#"{"detail":"Unauthorized"}"#)
            .create();

        let client = RegistryClient::new(server.url());
        let err = client
            .submit_mcp_request("test", None, "bad-token")
            .unwrap_err();

        assert!(err.to_string().contains("401"));
        mock.assert();
    }

    #[test]
    fn list_mcp_requests_parses_response() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/portal/mcp/requests")
            .match_header("authorization", "Bearer user-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{
                "items": [
                    {"id": "r1", "server_name": "github", "status": "approved"},
                    {"id": "r2", "server_name": "slack", "status": "pending"}
                ],
                "total": 2
            }"#)
            .create();

        let client = RegistryClient::new(server.url());
        let result = client.list_mcp_requests("user-token").unwrap();

        let items = result["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["status"].as_str().unwrap(), "approved");
        assert_eq!(items[1]["status"].as_str().unwrap(), "pending");
        mock.assert();
    }

    #[test]
    fn upload_audit_batch_sends_events() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/api/runner/mcp-audit")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"created": 2}"#)
            .create();

        let client = RegistryClient::new(server.url());
        let events = vec![
            serde_json::json!({"event_type": "server_accessed", "server_name": "github"}),
            serde_json::json!({"event_type": "tool_called", "tool_name": "read_file"}),
        ];

        client.upload_audit_batch(&events).unwrap();
        mock.assert();
    }

    #[test]
    fn upload_audit_batch_returns_error_on_failure() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/api/runner/mcp-audit")
            .with_status(500)
            .with_body("Internal Server Error")
            .create();

        let client = RegistryClient::new(server.url());
        let err = client
            .upload_audit_batch(&[serde_json::json!({"event_type": "test"})])
            .unwrap_err();

        assert!(err.to_string().contains("500"));
        mock.assert();
    }

    #[test]
    fn fetch_mcp_servers_returns_error_on_network_failure() {
        let client = RegistryClient::new("http://127.0.0.1:1".to_string());
        let err = client.fetch_mcp_servers().unwrap_err();
        assert!(err.to_string().contains("failed to reach registry"));
    }

    // ── fetch_approved_servers tests ─────────────────────────────────────────

    #[test]
    fn fetch_approved_servers_returns_live_response() {
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
                            "name": "github",
                            "package_source": "@modelcontextprotocol/server-github",
                            "status": "approved",
                            "server_id": "github",
                            "transport_type": "http",
                            "tool_visibility": "all",
                            "priority": 50
                        }
                    ]
                }"#,
            )
            .create();

        let root = temp_root("fetch-approved-live");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let registry = RegistryClient::new(server.url());

        let resp = fetch_approved_servers(&state, &registry).unwrap();
        assert_eq!(resp.servers.len(), 1);
        assert_eq!(resp.servers[0].name, "github");
        assert_eq!(resp.servers[0].server_id, Some("github".to_string()));
        assert_eq!(resp.servers[0].transport_type, Some("http".to_string()));
        assert_eq!(resp.servers[0].tool_visibility, Some("all".to_string()));
        assert_eq!(resp.servers[0].priority, Some(50));
        mock.assert();

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fetch_approved_servers_falls_back_to_cache() {
        let root = temp_root("fetch-approved-cache");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        // Pre-populate cache
        let cached = McpServersResponse {
            approval_mode: "trust".to_string(),
            servers: vec![McpServerEntry {
                name: "cached-server".to_string(),
                package_source: "cached-pkg".to_string(),
                version_pin: None,
                status: "approved".to_string(),
                credential_note: None,
                server_config: None,
                server_id: None,
                transport_type: None,
                gateway_url: None,
                tool_visibility: None,
                visible_tools: None,
                priority: None,
            }],
        };
        cache_mcp_config(&state, &cached).unwrap();

        // Use an unreachable registry — should fall back to cache
        let registry = RegistryClient::new("http://127.0.0.1:1".to_string());
        let resp = fetch_approved_servers(&state, &registry).unwrap();

        assert_eq!(resp.servers.len(), 1);
        assert_eq!(resp.servers[0].name, "cached-server");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fetch_approved_servers_errors_when_no_cache_and_offline() {
        let root = temp_root("fetch-approved-no-cache");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        // No cache pre-populated, unreachable registry
        let registry = RegistryClient::new("http://127.0.0.1:1".to_string());
        let err = fetch_approved_servers(&state, &registry).unwrap_err();
        assert!(
            err.to_string().contains("no cached server list available")
                || err.to_string().contains("failed to reach registry"),
            "unexpected error: {err}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mcpserverentry_aggregator_fields_default_to_none() {
        let json = r#"{
            "name": "test",
            "package_source": "test-pkg",
            "status": "approved"
        }"#;
        let entry: McpServerEntry = serde_json::from_str(json).unwrap();
        assert!(entry.server_id.is_none());
        assert!(entry.transport_type.is_none());
        assert!(entry.gateway_url.is_none());
        assert!(entry.tool_visibility.is_none());
        assert!(entry.visible_tools.is_none());
        assert!(entry.priority.is_none());
    }

}
