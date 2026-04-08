//! MCP aggregator — proxies all approved backend MCP servers through a single
//! SkillRunner stdio connection to the AI client.
//!
//! # Overview
//!
//! `BackendRegistry` is the central coordinator. It:
//!
//! 1. Fetches the approved server list from the registry (via
//!    `mcp_governance::fetch_approved_servers`).
//! 2. Maintains live HTTP connections to each approved backend.
//! 3. Exposes a merged tool list where every proxied tool is prefixed with
//!    `{server_id}__{tool_name}` to avoid name collisions.
//! 4. Routes `tools/call` requests to the correct backend.
//! 5. Enforces the `ToolBudget` (default: 100 tools max) with priority-based
//!    truncation.
//!
//! # Tool namespacing
//!
//! To prevent collisions when two backends expose the same tool name, all
//! proxied tools are namespaced:
//!
//! ```text
//! github__create_issue     ← GitHub MCP "create_issue"
//! sentry__search_issues    ← Sentry MCP "search_issues"
//! ```
//!
//! The separator is `__` (double underscore). Skills and governance tools retain
//! their existing `skillclub_` prefixes and are NOT passed through this module.
//!
//! # Tool budget
//!
//! Cursor and Windsurf cap MCP tool counts at 100. The `ToolBudget` tracks
//! how many slots are used and truncates lower-priority backend tools when the
//! limit would be exceeded. Priority order (highest to lowest):
//!
//! 1. Governance / management tools (handled outside this module, priority 100)
//! 2. Installed skill execution tools (handled outside this module, priority 90)
//! 3. Backend proxied tools, ordered by `McpServerEntry::priority` descending
//!    (default 50 when unset)

use anyhow::{Context, Result};
use serde_json::Value;
use skillrunner_core::{
    mcp_governance::{fetch_approved_servers, McpServerEntry, McpServersResponse},
    registry::RegistryClient,
    state::AppState,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

// ── Tool budget ───────────────────────────────────────────────────────────────

/// Maximum number of MCP tools this aggregator will surface to the AI client.
///
/// Reserve `RESERVED_SLOTS` for governance + skill tools managed outside this
/// module. The remaining slots go to backend proxied tools.
pub const TOOL_BUDGET_TOTAL: usize = 100;

/// Slots reserved for governance / management / skill execution tools that are
/// managed by the existing `tools.rs` layer.
pub const RESERVED_SLOTS: usize = 20;

/// The remaining budget available for proxied backend tools.
pub const BACKEND_TOOL_BUDGET: usize = TOOL_BUDGET_TOTAL - RESERVED_SLOTS;

/// Tracks tool-count usage across backend servers and enforces the budget cap.
#[derive(Debug, Default)]
pub struct ToolBudget {
    /// Total proxied tools currently loaded (after truncation).
    used: usize,
    /// Names of backend servers whose tools were truncated due to budget.
    truncated_servers: Vec<String>,
}

impl ToolBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of proxied tool slots still available.
    pub fn remaining(&self) -> usize {
        BACKEND_TOOL_BUDGET.saturating_sub(self.used)
    }

    /// Returns true if there is budget for `n` more tools.
    pub fn has_room_for(&self, n: usize) -> bool {
        self.used + n <= BACKEND_TOOL_BUDGET
    }

    /// Consume `n` slots for a backend.
    pub fn consume(&mut self, n: usize) {
        self.used += n;
    }

    /// Record that a server's tools were partially or fully truncated.
    pub fn record_truncation(&mut self, server_id: &str) {
        if !self.truncated_servers.contains(&server_id.to_string()) {
            self.truncated_servers.push(server_id.to_string());
        }
    }

    /// Servers that had at least one tool truncated from the budget.
    pub fn truncated_servers(&self) -> &[String] {
        &self.truncated_servers
    }

    /// Reset for a full re-sync.
    pub fn reset(&mut self) {
        self.used = 0;
        self.truncated_servers.clear();
    }
}

// ── Backend connection ────────────────────────────────────────────────────────

/// The upstream transport variant for a backend MCP server.
#[derive(Debug, Clone)]
pub enum BackendConnection {
    /// HTTP-based MCP server (streamable HTTP transport).
    Http(HttpBackend),
    /// Stdio-based local MCP server (child process).
    Stdio(StdioBackend),
}

/// An HTTP MCP backend — connects to a remote MCP server or gateway route.
#[derive(Debug, Clone)]
pub struct HttpBackend {
    /// Stable ID (used for tool namespacing).
    pub server_id: String,
    /// Human-readable name.
    pub name: String,
    /// Base URL of the backend's MCP endpoint.
    pub url: String,
    /// Tools fetched from this backend (original names, not namespaced).
    pub tools: Vec<ToolDefinition>,
    /// Tool visibility policy from the registry catalog.
    pub tool_visibility: ToolVisibility,
    /// Priority for budget allocation.
    pub priority: u8,
    /// Optional Bearer token for gateway-authenticated backends.
    pub auth_token: Option<String>,
}

/// A stdio MCP backend — managed as a child process.
#[derive(Debug, Clone)]
pub struct StdioBackend {
    /// Stable ID (used for tool namespacing).
    pub server_id: String,
    /// Human-readable name.
    pub name: String,
    /// Command to run.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Tools fetched from this backend (original names, not namespaced).
    pub tools: Vec<ToolDefinition>,
    /// Tool visibility policy from the registry catalog.
    pub tool_visibility: ToolVisibility,
    /// Priority for budget allocation.
    pub priority: u8,
}

/// A single MCP tool definition as returned by a backend's `tools/list`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<Value>,
}

/// Tool visibility policy from the registry catalog.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolVisibility {
    /// Surface all tools from this backend.
    All,
    /// Surface only the listed tools.
    Curated(Vec<String>),
    /// Do not surface tools by default; available via catalog browse.
    OnDemand,
}

impl ToolVisibility {
    fn from_entry(entry: &McpServerEntry) -> Self {
        match entry.tool_visibility.as_deref() {
            Some("curated") => {
                let list = entry.visible_tools.clone().unwrap_or_default();
                ToolVisibility::Curated(list)
            }
            Some("on_demand") => ToolVisibility::OnDemand,
            _ => ToolVisibility::All,
        }
    }

    /// Filter a list of tool definitions according to this visibility policy.
    pub fn filter<'a>(&self, tools: &'a [ToolDefinition]) -> Vec<&'a ToolDefinition> {
        match self {
            ToolVisibility::All => tools.iter().collect(),
            ToolVisibility::Curated(allowed) => {
                tools.iter().filter(|t| allowed.contains(&t.name)).collect()
            }
            ToolVisibility::OnDemand => vec![],
        }
    }
}

impl BackendConnection {
    pub fn server_id(&self) -> &str {
        match self {
            BackendConnection::Http(b) => &b.server_id,
            BackendConnection::Stdio(b) => &b.server_id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            BackendConnection::Http(b) => &b.name,
            BackendConnection::Stdio(b) => &b.name,
        }
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        match self {
            BackendConnection::Http(b) => &b.tools,
            BackendConnection::Stdio(b) => &b.tools,
        }
    }

    pub fn tool_visibility(&self) -> &ToolVisibility {
        match self {
            BackendConnection::Http(b) => &b.tool_visibility,
            BackendConnection::Stdio(b) => &b.tool_visibility,
        }
    }

    pub fn priority(&self) -> u8 {
        match self {
            BackendConnection::Http(b) => b.priority,
            BackendConnection::Stdio(b) => b.priority,
        }
    }
}

// ── Backend registry ──────────────────────────────────────────────────────────

/// Inner state of the registry, protected by a Mutex for thread safety.
pub(crate) struct RegistryInner {
    /// Active backend connections keyed by server_id.
    pub(crate) backends: HashMap<String, BackendConnection>,
    /// Tool budget tracker (reset on each sync).
    pub(crate) budget: ToolBudget,
    /// Time of the last successful sync.
    pub(crate) last_synced: Option<Instant>,
    /// Cached approved server list from the most recent successful registry fetch.
    pub(crate) last_response: Option<McpServersResponse>,
}

/// The central MCP aggregator. Manages all backend connections and exposes a
/// merged, namespaced tool surface to the MCP server loop.
#[derive(Clone)]
pub struct BackendRegistry {
    pub(crate) inner: Arc<Mutex<RegistryInner>>,
    http: reqwest::blocking::Client,
}

impl BackendRegistry {
    /// Create a new, empty registry. Call `sync()` to populate it.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                backends: HashMap::new(),
                budget: ToolBudget::new(),
                last_synced: None,
                last_response: None,
            })),
            http: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new()),
        }
    }

    /// Sync with the registry: fetch the approved server list and update
    /// backend connections.
    ///
    /// - New servers are added.
    /// - Servers no longer in the approved list are removed.
    /// - Existing servers are refreshed if their config changed.
    ///
    /// Returns the number of backends now active.
    pub fn sync(&self, state: &AppState, registry_client: &RegistryClient) -> Result<usize> {
        let response = fetch_approved_servers(state, registry_client)?;

        // Load portal auth token for gateway-routed backends
        let gateway_token = skillrunner_core::auth::load_tokens(state, &registry_client.base_url)
            .ok()
            .flatten()
            .map(|t| t.access_token);

        let mut inner = self.inner.lock().unwrap();
        inner.budget.reset();

        // Build a set of server IDs in the new approved list.
        let approved_ids: std::collections::HashSet<String> = response
            .servers
            .iter()
            .filter(|s| s.status != "blocked")
            .map(effective_server_id)
            .collect();

        // Remove backends no longer approved.
        let removed: Vec<String> = inner
            .backends
            .keys()
            .filter(|id| !approved_ids.contains(*id))
            .cloned()
            .collect();
        for id in &removed {
            info!(server_id = %id, "removing backend — no longer approved");
            inner.backends.remove(id);
        }

        // Sort servers by priority descending for budget allocation.
        let mut servers: Vec<&McpServerEntry> = response
            .servers
            .iter()
            .filter(|s| s.status != "blocked")
            .collect();
        servers.sort_by_key(|s| std::cmp::Reverse(s.priority.unwrap_or(50)));

        // Add or refresh backends.
        for entry in &servers {
            let server_id = effective_server_id(entry);
            let transport = entry.transport_type.as_deref().unwrap_or("http");
            let visibility = ToolVisibility::from_entry(entry);
            let priority = entry.priority.unwrap_or(50);

            let conn = if transport == "stdio" {
                // Build from server_config's command/args
                let (command, args) = extract_stdio_command(entry);
                BackendConnection::Stdio(StdioBackend {
                    server_id: server_id.clone(),
                    name: entry.name.clone(),
                    command,
                    args,
                    tools: vec![], // populated by fetch_tools_from_backend
                    tool_visibility: visibility,
                    priority,
                })
            } else {
                // HTTP backend — URL from gateway_url or server_config URL.
                // Skip backends that only have a package_source (npm name) but no
                // actual reachable URL — they are catalog entries awaiting gateway
                // configuration or local stdio setup.
                // For gateway-routed backends, rewrite the gateway_url to use
                // the local registry base URL (the one the user configured via
                // --registry-url / SKILLCLUB_REGISTRY_URL). The API may return a
                // public URL that isn't reachable from this machine.
                let url = if transport == "gateway" {
                    entry.gateway_url.as_ref().and_then(|gw_url| {
                        // Extract the path portion (e.g. /gateway/guinness-finder/mcp)
                        gw_url.find("/gateway/").map(|idx| {
                            let path = &gw_url[idx..];
                            let base = registry_client.base_url.trim_end_matches('/');
                            format!("{base}{path}")
                        })
                    })
                } else {
                    entry
                        .gateway_url
                        .clone()
                        .or_else(|| {
                            entry
                                .server_config
                                .as_ref()
                                .and_then(|c| c.get("url").and_then(|u| u.as_str()).map(String::from))
                        })
                };

                match url {
                    Some(url) => {
                        // Gateway-routed backends get the portal JWT
                        let token = if transport == "gateway" {
                            gateway_token.clone()
                        } else {
                            None
                        };
                        BackendConnection::Http(HttpBackend {
                            server_id: server_id.clone(),
                            name: entry.name.clone(),
                            url,
                            tools: vec![],
                            tool_visibility: visibility,
                            priority,
                            auth_token: token,
                        })
                    }
                    None => {
                        debug!(
                            server_id = %server_id,
                            package_source = %entry.package_source,
                            "skipping backend — no gateway_url or server_config url configured"
                        );
                        continue;
                    }
                }
            };

            // Fetch tool list from the backend (best-effort; empty tools on failure).
            let tools = self
                .fetch_tools_from_backend(&conn)
                .unwrap_or_else(|e| {
                    let err_str = e.to_string();
                    if err_str.contains("401") || err_str.contains("Unauthorized") {
                        debug!(server_id = %server_id, "backend requires credentials — skipping tool fetch (authorize via portal to connect)");
                    } else {
                        warn!(server_id = %server_id, error = %e, "failed to fetch tools from backend");
                    }
                    vec![]
                });

            // Apply visibility filter and budget.
            let visible: Vec<ToolDefinition> = conn
                .tool_visibility()
                .filter(&tools)
                .into_iter()
                .cloned()
                .collect();

            let budget_slots = if inner.budget.has_room_for(visible.len()) {
                inner.budget.consume(visible.len());
                visible.len()
            } else {
                let remaining = inner.budget.remaining();
                inner.budget.record_truncation(&server_id);
                inner.budget.consume(remaining);
                remaining
            };

            // Rebuild the connection with the (possibly truncated) tool list.
            let final_tools = tools.into_iter().take(budget_slots).collect();
            let conn_with_tools = rebuild_with_tools(conn, final_tools);

            if !inner.backends.contains_key(&server_id) {
                info!(server_id = %server_id, tools = budget_slots, "registered new backend");
            } else {
                debug!(server_id = %server_id, tools = budget_slots, "refreshed backend");
            }
            inner.backends.insert(server_id, conn_with_tools);
        }

        inner.last_synced = Some(Instant::now());
        inner.last_response = Some(response);

        let count = inner.backends.len();
        info!(backends = count, "sync complete");
        Ok(count)
    }

    /// Sync backends from the local `backends.yaml` config file.
    ///
    /// This is the standalone (no-registry) path. Local backends are loaded
    /// and merged into the registry. When called alongside `sync()`, local
    /// backends are additive — they do not remove registry-sourced backends.
    ///
    /// Returns the number of local backends loaded.
    pub fn sync_local(&self, state: &AppState) -> Result<usize> {
        let local_backends = crate::backends_config::load_local_backends(state)?;

        if local_backends.is_empty() {
            debug!("no local backends to sync");
            return Ok(0);
        }

        let mut inner = self.inner.lock().unwrap();

        let mut loaded = 0;
        for conn in local_backends {
            let server_id = conn.server_id().to_string();

            // Fetch tools from the backend (best-effort).
            let tools = self
                .fetch_tools_from_backend(&conn)
                .unwrap_or_else(|e| {
                    warn!(
                        server_id = %server_id,
                        error = %e,
                        "failed to fetch tools from local backend"
                    );
                    vec![]
                });

            // Apply budget.
            let budget_slots = if inner.budget.has_room_for(tools.len()) {
                inner.budget.consume(tools.len());
                tools.len()
            } else {
                let remaining = inner.budget.remaining();
                inner.budget.record_truncation(&server_id);
                inner.budget.consume(remaining);
                remaining
            };

            let final_tools = tools.into_iter().take(budget_slots).collect();
            let conn_with_tools = rebuild_with_tools(conn, final_tools);

            if !inner.backends.contains_key(&server_id) {
                info!(
                    server_id = %server_id,
                    tools = budget_slots,
                    "registered local backend"
                );
            } else {
                debug!(
                    server_id = %server_id,
                    tools = budget_slots,
                    "refreshed local backend"
                );
            }
            inner.backends.insert(server_id, conn_with_tools);
            loaded += 1;
        }

        inner.last_synced = Some(Instant::now());
        info!(backends = loaded, "local backend sync complete");
        Ok(loaded)
    }

    /// Returns all namespaced tool definitions from all active backends,
    /// respecting tool budget limits.
    pub fn all_tools(&self) -> Vec<Value> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::new();

        for backend in inner.backends.values() {
            let server_id = backend.server_id();
            for tool in backend.tools() {
                let namespaced_name = namespace_tool(server_id, &tool.name);
                let mut tool_json = serde_json::json!({
                    "name": namespaced_name,
                });
                if let Some(desc) = &tool.description {
                    tool_json["description"] = Value::String(format!(
                        "[{}] {}",
                        backend.name(),
                        desc
                    ));
                }
                if let Some(schema) = &tool.input_schema {
                    tool_json["inputSchema"] = schema.clone();
                }
                out.push(tool_json);
            }
        }

        out
    }

    /// Dispatch a namespaced tool call to the appropriate backend.
    ///
    /// Returns `None` if the tool name does not match any registered backend
    /// (i.e. it should be handled by the skill/governance layer instead).
    pub fn dispatch(&self, namespaced_tool: &str, args: &Value) -> Option<Result<Value>> {
        let (server_id, original_tool) = parse_tool_name(namespaced_tool)?;

        let inner = self.inner.lock().unwrap();
        let backend = inner.backends.get(server_id)?;

        // Verify the tool exists on this backend.
        let tool_exists = backend.tools().iter().any(|t| t.name == original_tool);
        if !tool_exists {
            return Some(Err(anyhow::anyhow!(
                "tool '{}' not found on backend '{}'",
                original_tool,
                server_id
            )));
        }

        let result = match backend {
            BackendConnection::Http(http) => {
                self.call_http_tool(&http.url, original_tool, args, http.auth_token.as_deref())
            }
            BackendConnection::Stdio(_stdio) => {
                // Stdio dispatch is stubbed — full child-process management
                // is out of scope for the initial aggregator implementation.
                Err(anyhow::anyhow!(
                    "stdio backend dispatch not yet implemented for '{}'",
                    server_id
                ))
            }
        };

        Some(result)
    }

    /// Shut down all backend connections gracefully.
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        let count = inner.backends.len();
        inner.backends.clear();
        inner.budget.reset();
        info!(backends = count, "aggregator shut down");
    }

    /// How long since the last successful sync, or `None` if never synced.
    pub fn time_since_sync(&self) -> Option<Duration> {
        self.inner
            .lock()
            .unwrap()
            .last_synced
            .map(|t| t.elapsed())
    }

    /// Number of currently active backends.
    pub fn backend_count(&self) -> usize {
        self.inner.lock().unwrap().backends.len()
    }

    /// Check whether a backend with the given ID is active.
    pub fn has_backend(&self, server_id: &str) -> bool {
        self.inner.lock().unwrap().backends.contains_key(server_id)
    }

    /// Remove a single backend by ID. Returns true if a backend was removed.
    pub fn remove_backend(&self, server_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let removed = inner.backends.remove(server_id).is_some();
        if removed {
            info!(server_id = %server_id, "removed backend from aggregator");
        }
        removed
    }

    /// Get tool names for a specific backend.
    pub fn backend_tools(&self, server_id: &str) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        match inner.backends.get(server_id) {
            Some(conn) => conn
                .tools()
                .iter()
                .map(|t| format!("{}__{}", server_id, t.name))
                .collect(),
            None => vec![],
        }
    }

    /// IDs of backends that had tools truncated due to the tool budget.
    pub fn truncated_backends(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .budget
            .truncated_servers()
            .to_vec()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Attempt to fetch the tool list from a backend's MCP endpoint.
    /// Returns an empty vec on any transport error (graceful degradation).
    fn fetch_tools_from_backend(&self, conn: &BackendConnection) -> Result<Vec<ToolDefinition>> {
        match conn {
            BackendConnection::Http(http) => {
                // MCP Streamable HTTP: POST JSON-RPC to the endpoint URL directly.
                // The method is in the request body, not appended to the path.
                let tools_url = http.url.trim_end_matches('/').to_string();
                debug!(url = %tools_url, "fetching tools from HTTP backend");

                let mut req = self
                    .http
                    .post(&tools_url)
                    .json(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/list",
                        "params": {}
                    }));

                if let Some(token) = &http.auth_token {
                    req = req.header("authorization", format!("Bearer {token}"));
                }

                let resp = req
                    .send()
                    .with_context(|| format!("failed to reach backend at {tools_url}"))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    anyhow::bail!("backend returned HTTP {status}");
                }

                let body: Value = resp
                    .json()
                    .context("failed to parse tools/list response")?;

                let tools = body
                    .get("result")
                    .and_then(|r| r.get("tools"))
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| serde_json::from_value(v.clone()).ok())
                            .collect()
                    })
                    .unwrap_or_default();

                Ok(tools)
            }
            BackendConnection::Stdio(_) => {
                // Stdio tool listing requires spawning the process, which is
                // deferred to a future implementation phase.
                Ok(vec![])
            }
        }
    }

    /// Call a tool on an HTTP backend and return the result.
    fn call_http_tool(&self, base_url: &str, tool_name: &str, args: &Value, auth_token: Option<&str>) -> Result<Value> {
        // MCP Streamable HTTP: POST JSON-RPC to the endpoint URL directly.
        let call_url = base_url.trim_end_matches('/').to_string();
        debug!(url = %call_url, tool = %tool_name, "dispatching tool call to HTTP backend");

        let mut req = self
            .http
            .post(&call_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": args,
                }
            }));

        if let Some(token) = auth_token {
            req = req.header("authorization", format!("Bearer {token}"));
        }

        let resp = req
            .send()
            .with_context(|| format!("failed to reach backend at {call_url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("backend tool call failed (HTTP {status}): {body}");
        }

        let body: Value = resp
            .json()
            .context("failed to parse tools/call response")?;

        // Unwrap the JSON-RPC result layer.
        if let Some(err) = body.get("error") {
            anyhow::bail!("backend returned JSON-RPC error: {err}");
        }

        Ok(body
            .get("result")
            .cloned()
            .unwrap_or(Value::Null))
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Namespace a tool name: `{server_id}__{tool_name}`.
///
/// The double-underscore separator is chosen because it cannot appear in
/// standard MCP tool names (which follow identifier conventions).
pub fn namespace_tool(server_id: &str, tool_name: &str) -> String {
    format!("{server_id}__{tool_name}")
}

/// Parse a namespaced tool name back into `(server_id, original_tool_name)`.
///
/// Returns `None` if the name does not contain the `__` separator (i.e. it
/// is a skill or governance tool, not a proxied backend tool).
pub fn parse_tool_name(namespaced: &str) -> Option<(&str, &str)> {
    let pos = namespaced.find("__")?;
    let server_id = &namespaced[..pos];
    let tool_name = &namespaced[pos + 2..];
    if server_id.is_empty() || tool_name.is_empty() {
        None
    } else {
        Some((server_id, tool_name))
    }
}

/// Derive the effective server_id for an entry: use `server_id` field if
/// present, otherwise fall back to a sanitised version of `name`.
fn effective_server_id(entry: &McpServerEntry) -> String {
    entry
        .server_id
        .clone()
        .unwrap_or_else(|| sanitize_id(&entry.name))
}

/// Convert a display name into a valid identifier (lowercase, dashes only).
pub fn sanitize_id(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Extract command and args from a stdio McpServerEntry's server_config.
fn extract_stdio_command(entry: &McpServerEntry) -> (String, Vec<String>) {
    if let Some(config) = &entry.server_config {
        let command = config
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("npx")
            .to_string();
        let args = config
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        return (command, args);
    }
    // Default: run via npx
    (
        "npx".to_string(),
        vec!["-y".to_string(), entry.package_source.clone()],
    )
}

/// Rebuild a `BackendConnection` with a new tool list (other fields unchanged).
fn rebuild_with_tools(conn: BackendConnection, tools: Vec<ToolDefinition>) -> BackendConnection {
    match conn {
        BackendConnection::Http(mut b) => {
            b.tools = tools;
            BackendConnection::Http(b)
        }
        BackendConnection::Stdio(mut b) => {
            b.tools = tools;
            BackendConnection::Stdio(b)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_tool_name ───────────────────────────────────────────────────────

    #[test]
    fn parse_tool_name_splits_on_double_underscore() {
        let (server, tool) = parse_tool_name("github__create_issue").unwrap();
        assert_eq!(server, "github");
        assert_eq!(tool, "create_issue");
    }

    #[test]
    fn parse_tool_name_returns_none_without_separator() {
        assert!(parse_tool_name("skillclub_search").is_none());
        assert!(parse_tool_name("create_issue").is_none());
        assert!(parse_tool_name("nodoubleunderscore").is_none());
    }

    #[test]
    fn parse_tool_name_returns_none_for_empty_parts() {
        assert!(parse_tool_name("__tool").is_none());
        assert!(parse_tool_name("server__").is_none());
        assert!(parse_tool_name("__").is_none());
    }

    // ── namespace_tool ────────────────────────────────────────────────────────

    #[test]
    fn namespace_tool_produces_double_underscore_prefix() {
        assert_eq!(namespace_tool("sentry", "search_issues"), "sentry__search_issues");
        assert_eq!(namespace_tool("github", "create_pr"), "github__create_pr");
    }

    // ── ToolBudget ────────────────────────────────────────────────────────────

    #[test]
    fn tool_budget_tracks_remaining_correctly() {
        let mut budget = ToolBudget::new();
        assert_eq!(budget.remaining(), BACKEND_TOOL_BUDGET);
        budget.consume(10);
        assert_eq!(budget.remaining(), BACKEND_TOOL_BUDGET - 10);
    }

    #[test]
    fn tool_budget_has_room_for_works() {
        let mut budget = ToolBudget::new();
        assert!(budget.has_room_for(BACKEND_TOOL_BUDGET));
        budget.consume(BACKEND_TOOL_BUDGET);
        assert!(!budget.has_room_for(1));
    }

    #[test]
    fn tool_budget_records_truncation() {
        let mut budget = ToolBudget::new();
        budget.record_truncation("github");
        budget.record_truncation("sentry");
        budget.record_truncation("github"); // duplicate — should not double-count
        assert_eq!(budget.truncated_servers().len(), 2);
        assert!(budget.truncated_servers().contains(&"github".to_string()));
    }

    #[test]
    fn tool_budget_resets_cleanly() {
        let mut budget = ToolBudget::new();
        budget.consume(50);
        budget.record_truncation("github");
        budget.reset();
        assert_eq!(budget.remaining(), BACKEND_TOOL_BUDGET);
        assert!(budget.truncated_servers().is_empty());
    }

    // ── ToolVisibility ────────────────────────────────────────────────────────

    #[test]
    fn tool_visibility_all_passes_all_tools() {
        let tools = vec![
            ToolDefinition { name: "a".to_string(), description: None, input_schema: None },
            ToolDefinition { name: "b".to_string(), description: None, input_schema: None },
        ];
        let visible = ToolVisibility::All.filter(&tools);
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn tool_visibility_curated_filters_to_allowed_list() {
        let tools = vec![
            ToolDefinition { name: "create_issue".to_string(), description: None, input_schema: None },
            ToolDefinition { name: "delete_repo".to_string(), description: None, input_schema: None },
            ToolDefinition { name: "list_prs".to_string(), description: None, input_schema: None },
        ];
        let visible = ToolVisibility::Curated(vec![
            "create_issue".to_string(),
            "list_prs".to_string(),
        ])
        .filter(&tools);
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().any(|t| t.name == "create_issue"));
        assert!(visible.iter().any(|t| t.name == "list_prs"));
        assert!(!visible.iter().any(|t| t.name == "delete_repo"));
    }

    #[test]
    fn tool_visibility_on_demand_returns_empty() {
        let tools = vec![
            ToolDefinition { name: "a".to_string(), description: None, input_schema: None },
        ];
        assert!(ToolVisibility::OnDemand.filter(&tools).is_empty());
    }

    // ── sanitize_id ───────────────────────────────────────────────────────────

    #[test]
    fn sanitize_id_lowercases_and_replaces_spaces() {
        assert_eq!(sanitize_id("GitHub MCP"), "github-mcp");
        assert_eq!(sanitize_id("Sentry.io"), "sentry-io");
        assert_eq!(sanitize_id("plain"), "plain");
    }

    // ── BackendRegistry (unit, no network) ───────────────────────────────────

    #[test]
    fn all_tools_namespaces_correctly() {
        let registry = BackendRegistry::new();
        {
            let mut inner = registry.inner.lock().unwrap();
            inner.backends.insert(
                "github".to_string(),
                BackendConnection::Http(HttpBackend {
                    server_id: "github".to_string(),
                    name: "GitHub".to_string(),
                    url: "http://unused".to_string(),
                    tools: vec![
                        ToolDefinition {
                            name: "create_issue".to_string(),
                            description: Some("Create an issue".to_string()),
                            input_schema: None,
                        },
                    ],
                    tool_visibility: ToolVisibility::All,
                    priority: 50,
                    auth_token: None,
                }),
            );
        }

        let tools = registry.all_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "github__create_issue");
        assert!(tools[0]["description"]
            .as_str()
            .unwrap_or("")
            .contains("GitHub"));
    }

    #[test]
    fn dispatch_returns_none_for_non_namespaced_tool() {
        let registry = BackendRegistry::new();
        let result = registry.dispatch("skillclub_search", &Value::Null);
        assert!(result.is_none(), "non-namespaced tool should not be dispatched by aggregator");
    }

    #[test]
    fn dispatch_returns_none_for_unknown_server() {
        let registry = BackendRegistry::new();
        // No backends registered
        let result = registry.dispatch("unknown__some_tool", &Value::Null);
        assert!(result.is_none());
    }

    #[test]
    fn shutdown_clears_all_backends() {
        let registry = BackendRegistry::new();
        {
            let mut inner = registry.inner.lock().unwrap();
            inner.backends.insert(
                "test".to_string(),
                BackendConnection::Http(HttpBackend {
                    server_id: "test".to_string(),
                    name: "Test".to_string(),
                    url: "http://unused".to_string(),
                    tools: vec![],
                    tool_visibility: ToolVisibility::All,
                    priority: 50,
                    auth_token: None,
                }),
            );
        }
        assert_eq!(registry.backend_count(), 1);
        registry.shutdown();
        assert_eq!(registry.backend_count(), 0);
    }

    #[test]
    fn has_backend_returns_false_when_empty() {
        let registry = BackendRegistry::new();
        assert!(!registry.has_backend("test"));
    }

    #[test]
    fn has_backend_returns_true_for_existing() {
        let registry = BackendRegistry::new();
        {
            let mut inner = registry.inner.lock().unwrap();
            inner.backends.insert(
                "playwright".to_string(),
                BackendConnection::Http(HttpBackend {
                    server_id: "playwright".to_string(),
                    name: "Playwright".to_string(),
                    url: "http://unused".to_string(),
                    tools: vec![],
                    tool_visibility: ToolVisibility::All,
                    priority: 50,
                    auth_token: None,
                }),
            );
        }
        assert!(registry.has_backend("playwright"));
        assert!(!registry.has_backend("github"));
    }

    #[test]
    fn remove_backend_returns_false_when_not_found() {
        let registry = BackendRegistry::new();
        assert!(!registry.remove_backend("nonexistent"));
    }

    #[test]
    fn remove_backend_removes_and_returns_true() {
        let registry = BackendRegistry::new();
        {
            let mut inner = registry.inner.lock().unwrap();
            inner.backends.insert(
                "sentry".to_string(),
                BackendConnection::Http(HttpBackend {
                    server_id: "sentry".to_string(),
                    name: "Sentry".to_string(),
                    url: "http://unused".to_string(),
                    tools: vec![],
                    tool_visibility: ToolVisibility::All,
                    priority: 50,
                    auth_token: None,
                }),
            );
        }
        assert_eq!(registry.backend_count(), 1);
        assert!(registry.remove_backend("sentry"));
        assert_eq!(registry.backend_count(), 0);
        assert!(!registry.has_backend("sentry"));
    }

    #[test]
    fn backend_tools_returns_namespaced_names() {
        let registry = BackendRegistry::new();
        {
            let mut inner = registry.inner.lock().unwrap();
            inner.backends.insert(
                "github".to_string(),
                BackendConnection::Http(HttpBackend {
                    server_id: "github".to_string(),
                    name: "GitHub".to_string(),
                    url: "http://unused".to_string(),
                    tools: vec![
                        ToolDefinition {
                            name: "create_issue".to_string(),
                            description: Some("Create issue".to_string()),
                            input_schema: None,
                        },
                        ToolDefinition {
                            name: "list_repos".to_string(),
                            description: None,
                            input_schema: None,
                        },
                    ],
                    tool_visibility: ToolVisibility::All,
                    priority: 50,
                    auth_token: None,
                }),
            );
        }
        let tools = registry.backend_tools("github");
        assert_eq!(tools.len(), 2);
        assert!(tools.contains(&"github__create_issue".to_string()));
        assert!(tools.contains(&"github__list_repos".to_string()));
    }

    #[test]
    fn backend_tools_returns_empty_for_unknown() {
        let registry = BackendRegistry::new();
        assert!(registry.backend_tools("unknown").is_empty());
    }
}
