use crate::{
    aggregator::BackendRegistry,
    protocol::{
        InitializeResult, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ServerCapabilities,
        ServerInfo, ToolCallParams, ToolsCapability, ToolsListResult, INVALID_PARAMS,
        METHOD_NOT_FOUND,
    },
    sampling::{HybridModelClient, McpSamplingClient, SharedIo},
    tools::{build_tool_list, handle_tool_call},
};
use anyhow::Result;
use skillrunner_core::{
    mcp_governance::{buffer_audit_event, flush_audit_buffer, AuditEvent},
    model::ModelClient,
    ollama::OllamaClient,
    policy::MockPolicyClient,
    registry::{HttpPolicyClient, RegistryClient},
    state::AppState,
    updater::check_skill_updates,
};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Minimum interval between automatic aggregator syncs triggered by tools/list.
const AGGREGATOR_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Configuration for the MCP server.
pub struct McpServerConfig {
    pub registry_url: Option<String>,
    pub ollama_url: String,
    pub model: String,
}

/// Mutable state shared across request dispatches within a single server session.
struct ServerState {
    /// The backend aggregator — manages all proxied backend connections.
    aggregator: BackendRegistry,
    /// Registry client (present when a registry URL is configured).
    registry_client: Option<RegistryClient>,
    /// Time of the last successful aggregator sync, for throttling.
    last_aggregator_sync: Option<Instant>,
    /// Number of aggregated tools from the last sync (used to detect changes).
    last_aggregated_tool_count: usize,
    /// Time of the last skill version sync, for throttling.
    last_skill_sync: Option<Instant>,
    /// Time of the last audit buffer flush.
    last_audit_flush: Option<Instant>,
    /// Time of the last unmanaged server scan.
    last_unmanaged_scan: Option<Instant>,
    /// Cached machine hostname for audit events.
    machine_id: String,
}

impl ServerState {
    fn new(registry_client: Option<RegistryClient>) -> Self {
        Self {
            aggregator: BackendRegistry::new(),
            registry_client,
            last_aggregator_sync: None,
            last_aggregated_tool_count: 0,
            last_skill_sync: None,
            last_audit_flush: None,
            last_unmanaged_scan: None,
            machine_id: crate::setup::get_machine_id(),
        }
    }

    /// Check all installed skills for available updates, respecting the 300 s throttle.
    ///
    /// Returns `true` if any skills were updated (caller should fire
    /// `notifications/tools/list_changed`).
    fn maybe_sync_skills(&mut self, app_state: &AppState) -> bool {
        let should_sync = match self.last_skill_sync {
            None => true,
            Some(t) => t.elapsed() >= AGGREGATOR_REFRESH_INTERVAL,
        };

        if !should_sync {
            return false;
        }

        let Some(registry) = &self.registry_client else {
            return false;
        };

        let policy_client =
            HttpPolicyClient::new(RegistryClient::new(&registry.base_url), app_state);

        self.last_skill_sync = Some(Instant::now());

        match check_skill_updates(app_state, registry, &policy_client) {
            Ok(0) => false,
            Ok(count) => {
                info!(
                    count,
                    "background skill sync updated skills — will fire list_changed"
                );
                true
            }
            Err(e) => {
                warn!(error = %e, "background skill sync failed");
                false
            }
        }
    }

    /// Flush buffered audit events to the registry on the same 300 s throttle.
    fn maybe_flush_audit(&mut self, app_state: &AppState) {
        let should_flush = match self.last_audit_flush {
            None => true,
            Some(t) => t.elapsed() >= AGGREGATOR_REFRESH_INTERVAL,
        };

        if !should_flush {
            return;
        }

        let Some(registry) = &self.registry_client else {
            return;
        };

        self.last_audit_flush = Some(Instant::now());

        match flush_audit_buffer(app_state, registry) {
            Ok(0) => {}
            Ok(n) => info!(count = n, "flushed audit events to registry"),
            Err(e) => warn!(error = %e, "audit flush failed"),
        }
    }

    /// Scan AI client configs for unmanaged MCP servers and emit audit events.
    fn maybe_scan_unmanaged(&mut self, app_state: &AppState) {
        let should_scan = match self.last_unmanaged_scan {
            None => true,
            Some(t) => t.elapsed() >= AGGREGATOR_REFRESH_INTERVAL,
        };

        if !should_scan {
            return;
        }

        // Only emit events if we have a registry to upload to
        if self.registry_client.is_none() {
            return;
        }

        self.last_unmanaged_scan = Some(Instant::now());

        let unmanaged = crate::setup::detect_unmanaged_servers();
        for server in &unmanaged {
            let event = AuditEvent {
                server_name: Some(server.server_name.clone()),
                user_id: None,
                user_email: None,
                machine_id: Some(self.machine_id.clone()),
                event_type: "unmanaged_server_detected".to_string(),
                tool_name: None,
                metadata: Some(serde_json::json!({
                    "config_path": server.config_path,
                    "client_name": server.client_name,
                })),
                org_id: "default".to_string(),
            };
            if let Err(e) = buffer_audit_event(app_state, &event) {
                warn!(error = %e, server = %server.server_name, "failed to buffer unmanaged server event");
            }
        }

        if !unmanaged.is_empty() {
            info!(
                count = unmanaged.len(),
                "detected unmanaged MCP servers — audit events buffered"
            );
        }
    }

    /// Attempt a throttled sync of the aggregator with the registry.
    ///
    /// Returns `true` if the sync changed the tool count (caller should fire
    /// `notifications/tools/list_changed`).
    fn maybe_sync_aggregator(&mut self, app_state: &AppState) -> bool {
        let should_sync = match self.last_aggregator_sync {
            None => true,
            Some(t) => t.elapsed() >= AGGREGATOR_REFRESH_INTERVAL,
        };

        if !should_sync {
            return false;
        }

        let Some(registry) = &self.registry_client else {
            // No registry configured — sync from local backends.yaml only
            match self.aggregator.sync_local(app_state) {
                Ok(_count) => {
                    self.last_aggregator_sync = Some(Instant::now());
                    let new_count = self.aggregator.all_tools().len();
                    let changed = new_count != self.last_aggregated_tool_count;
                    self.last_aggregated_tool_count = new_count;
                    if changed {
                        info!(
                            tools = new_count,
                            "local backend tool count changed — will fire list_changed"
                        );
                    }
                    return changed;
                }
                Err(e) => {
                    warn!(error = %e, "local backend sync failed");
                    return false;
                }
            }
        };

        match self.aggregator.sync(app_state, registry) {
            Ok(_count) => {
                // Also merge local backends alongside registry ones
                if let Err(e) = self.aggregator.sync_local(app_state) {
                    warn!(error = %e, "local backend merge after registry sync failed");
                }
                self.last_aggregator_sync = Some(Instant::now());
                let new_count = self.aggregator.all_tools().len();
                let changed = new_count != self.last_aggregated_tool_count;
                self.last_aggregated_tool_count = new_count;
                if changed {
                    info!(
                        tools = new_count,
                        "aggregator tool count changed — will fire list_changed"
                    );
                }
                changed
            }
            Err(e) => {
                warn!(error = %e, "aggregator sync failed");
                false
            }
        }
    }
}

/// Run the MCP server over stdio.
///
/// Reads JSON-RPC messages from stdin (one per line) and writes responses to stdout.
/// When a skill's LLM step needs a model, the server uses a hybrid approach:
/// try local Ollama first, fall back to MCP `sampling/createMessage` delegation.
/// Both the server loop and the sampling client share the same stdin/stdout handles.
pub fn run_server(state: AppState, config: McpServerConfig) -> Result<()> {
    // Create shared IO for both the server loop and sampling client
    let shared_io = Arc::new(Mutex::new(SharedIo::new(
        Box::new(io::stdout()),
        Box::new(io::BufReader::new(io::stdin())),
    )));

    let registry_client = config.registry_url.as_ref().map(RegistryClient::new);

    // Auto-detect model from Ollama if not explicitly specified
    let model_name = if config.model == "auto" {
        let probe = OllamaClient::new(&config.ollama_url, "");
        if probe.health_check().reachable {
            probe
                .list_models()
                .ok()
                .and_then(|m| m.into_iter().next().map(|m| m.name))
                .unwrap_or_else(|| "llama3.2".to_string())
        } else {
            "llama3.2".to_string()
        }
    } else {
        config.model.clone()
    };
    let ollama = OllamaClient::new(&config.ollama_url, &model_name);
    let ollama_available = ollama.health_check().reachable;

    // Create the sampling client (delegates LLM calls to the AI client)
    let sampling_client = McpSamplingClient::from_shared(Arc::clone(&shared_io));

    // Create the hybrid model client: tries Ollama first, falls back to sampling
    let hybrid_client = HybridModelClient::new(
        if ollama_available {
            Some(&ollama as &dyn ModelClient)
        } else {
            None
        },
        &sampling_client,
        ollama_available,
    );

    info!(
        "MCP server starting (ollama={}, registry={})",
        if ollama_available {
            "available"
        } else {
            "unavailable"
        },
        config.registry_url.as_deref().unwrap_or("none"),
    );

    let mut server_state = ServerState::new(registry_client);

    // Initial syncs (best-effort — don't block startup on failure)
    let _ = server_state.maybe_sync_aggregator(&state);
    server_state.maybe_scan_unmanaged(&state);

    loop {
        // Read next line from stdin (release lock before dispatching)
        let line = {
            let mut io = shared_io.lock().unwrap();
            match io.read_line() {
                Ok(l) => l,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    error!("Failed to read stdin: {e}");
                    break;
                }
            }
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::error(
                    None,
                    crate::protocol::PARSE_ERROR,
                    format!("Invalid JSON: {e}"),
                );
                let mut io = shared_io.lock().unwrap();
                io.write_message(&serde_json::to_string(&resp)?)?;
                continue;
            }
        };

        debug!("Received: {} (id={:?})", request.method, request.id);

        // Notifications (no id) don't get responses
        if request.id.is_none() {
            continue;
        }

        // On tools/list, attempt throttled aggregator + skill-version refreshes.
        // Track whether either sync caused a change that should fire list_changed.
        let (aggregator_changed, skills_changed) = if request.method == "tools/list" {
            let agg = server_state.maybe_sync_aggregator(&state);
            let skills = server_state.maybe_sync_skills(&state);
            server_state.maybe_scan_unmanaged(&state);
            server_state.maybe_flush_audit(&state);
            (agg, skills)
        } else {
            (false, false)
        };

        // Dispatch the request — during tools/call, this may trigger
        // sampling requests through the shared_io (the lock is not held here)
        let response = dispatch_request(
            &request,
            &state,
            &config,
            Some(&hybrid_client),
            server_state.registry_client.as_ref(),
            &server_state.aggregator,
            &server_state.machine_id,
        );

        // Write response (lock shared_io)
        {
            let mut io = shared_io.lock().unwrap();
            io.write_message(&serde_json::to_string(&response)?)?;
        }

        // Fire list_changed notifications:
        // 1. After a skill install (existing behaviour)
        // 2. After an aggregator sync that changed the tool count
        // 3. After background skill-version sync updated one or more skills
        let fire_list_changed = aggregator_changed || skills_changed || {
            request.method == "tools/call"
                && serde_json::from_value::<ToolCallParams>(request.params.clone())
                    .map(|p| {
                        matches!(
                            p.name.as_str(),
                            "skillclub_install"
                                | "skillclub_uninstall"
                                | "skillclub_mcp_install"
                                | "skillclub_mcp_uninstall"
                        )
                    })
                    .unwrap_or(false)
        };

        if fire_list_changed {
            let notification = JsonRpcNotification::new("notifications/tools/list_changed");
            let mut io = shared_io.lock().unwrap();
            io.write_message(&serde_json::to_string(&notification)?)?;
        }
    }

    info!("MCP server shutting down");
    server_state.aggregator.shutdown();
    Ok(())
}

/// Emit an audit event for a proxied backend tool call.
fn emit_tool_called_event(state: &AppState, tool_name: &str, machine_id: &str) {
    // Parse server_id from the namespaced tool name (e.g. "github__create_issue" → "github")
    let server_name = tool_name.split("__").next().map(String::from);

    let event = AuditEvent {
        server_name,
        user_id: None,
        user_email: None,
        machine_id: Some(machine_id.to_string()),
        event_type: "tool_called".to_string(),
        tool_name: Some(tool_name.to_string()),
        metadata: None,
        org_id: "default".to_string(),
    };
    if let Err(e) = buffer_audit_event(state, &event) {
        warn!(error = %e, tool = %tool_name, "failed to buffer tool_called audit event");
    }
}

fn dispatch_request(
    request: &JsonRpcRequest,
    state: &AppState,
    config: &McpServerConfig,
    model_client: Option<&dyn ModelClient>,
    registry_client: Option<&RegistryClient>,
    aggregator: &BackendRegistry,
    machine_id: &str,
) -> JsonRpcResponse {
    match request.method.as_str() {
        "initialize" => handle_initialize(request, state),
        "tools/list" => handle_tools_list(request, state, &config.registry_url, aggregator),
        "tools/call" => handle_tools_call(
            request,
            state,
            config,
            model_client,
            registry_client,
            aggregator,
            machine_id,
        ),
        _ => JsonRpcResponse::error(
            request.id.clone(),
            METHOD_NOT_FOUND,
            format!("Unknown method: {}", request.method),
        ),
    }
}

/// Build MCP server instructions dynamically based on managed.json.
///
/// When a managed config exists with an org name, instructions are stronger
/// and org-specific. Custom governance messages from IT override the default.
fn build_instructions(state: &AppState) -> String {
    use skillrunner_core::managed::load_managed_config;

    let managed = load_managed_config(state);

    // If IT disabled governance messaging, return minimal instructions
    if let Some(ref config) = managed {
        if !config.governance_message_enabled {
            return "SkillClub SkillRunner — governed AI platform. \
                    Use skillclub_list and skillclub_search for capabilities."
                .to_string();
        }
    }

    // If IT provided a custom message, use it
    if let Some(ref config) = managed {
        if let Some(ref custom) = config.governance_message {
            return custom.clone();
        }
    }

    // Build org-aware instructions
    let org_line = match managed.as_ref().and_then(|c| c.org.as_deref()) {
        Some(org) => format!("This SkillRunner instance is managed by {org}. "),
        None => String::new(),
    };

    format!(
        "{org_line}\
         SkillClub SkillRunner is the user's governed AI platform. \
         ALL MCP servers, plugins, and AI tools MUST be installed exclusively through \
         SkillRunner's governance system. NEVER install MCP servers directly to config files \
         (such as ~/.claude/settings.json, ~/.claude.json, ~/.cursor/mcp.json, or any \
         managed-mcp.json). NEVER use update-config, enabledPlugins, or any native plugin \
         installation mechanism to install MCP servers. \
         \
         To install an MCP server: use skillclub_mcp_request to request access, then \
         skillclub_mcp_install to activate approved servers. Servers installed through \
         SkillRunner appear as namespaced tools (e.g., github__create_issue) and are \
         subject to enterprise policy, audit, and revocation. Direct installation \
         bypasses governance and is not permitted. \
         \
         For plugins: use skillclub_plugin_search to find plugins and \
         skillclub_plugin_install to add them. Plugins bundle skills, MCP servers, \
         and commands into governed packages. \
         \
         For AI skills: use skillclub_search to find skills and skillclub_install to add \
         them. Use skillclub_list to show installed skills. Installed skills can be run \
         directly as tools by their skill ID. \
         \
         When the user asks about 'skills', 'what skills are available', 'what can you do', \
         or similar queries about capabilities, use skillclub_list and skillclub_search."
    )
}

fn handle_initialize(request: &JsonRpcRequest, state: &AppState) -> JsonRpcResponse {
    let instructions = build_instructions(state);

    let result = InitializeResult {
        protocol_version: "2024-11-05".to_string(),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability { list_changed: true }),
        },
        server_info: ServerInfo {
            name: "skillrunner".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        instructions: Some(instructions),
    };

    JsonRpcResponse::success(
        request.id.clone(),
        serde_json::to_value(result).unwrap_or_default(),
    )
}

fn handle_tools_list(
    request: &JsonRpcRequest,
    state: &AppState,
    registry_url: &Option<String>,
    aggregator: &BackendRegistry,
) -> JsonRpcResponse {
    // Skill + governance tools from the existing layer
    let mut tools = build_tool_list(state, registry_url);

    // Merge in proxied backend tools from the aggregator
    let backend_tools = aggregator.all_tools();
    for bt in backend_tools {
        // Convert the aggregator's serde_json::Value into a protocol ToolDefinition
        let name = bt["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let description = bt["description"].as_str().unwrap_or("").to_string();
        let input_schema = bt
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));

        tools.push(crate::protocol::ToolDefinition {
            name,
            description,
            input_schema,
        });
    }

    let result = ToolsListResult { tools };
    JsonRpcResponse::success(
        request.id.clone(),
        serde_json::to_value(result).unwrap_or_default(),
    )
}

fn handle_tools_call(
    request: &JsonRpcRequest,
    state: &AppState,
    config: &McpServerConfig,
    model_client: Option<&dyn ModelClient>,
    registry_client: Option<&RegistryClient>,
    aggregator: &BackendRegistry,
    machine_id: &str,
) -> JsonRpcResponse {
    let params: ToolCallParams = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::error(
                request.id.clone(),
                INVALID_PARAMS,
                format!("Invalid tool call params: {e}"),
            );
        }
    };

    // Handle MCP install/uninstall — these need the aggregator directly
    if params.name == "skillclub_mcp_install" {
        let result = crate::tools::handle_mcp_install(
            &params.arguments,
            state,
            &config.registry_url,
            aggregator,
        );
        return JsonRpcResponse::success(
            request.id.clone(),
            serde_json::to_value(result).unwrap_or_default(),
        );
    }
    if params.name == "skillclub_mcp_uninstall" {
        let result =
            crate::tools::handle_mcp_uninstall(&params.arguments, &config.registry_url, aggregator);
        return JsonRpcResponse::success(
            request.id.clone(),
            serde_json::to_value(result).unwrap_or_default(),
        );
    }

    // Try aggregator first for namespaced backend tools
    if let Some(dispatch_result) = aggregator.dispatch(&params.name, &params.arguments) {
        // Emit tool_called audit event for proxied backend tools
        emit_tool_called_event(state, &params.name, machine_id);

        return match dispatch_result {
            Ok(value) => {
                let call_result = crate::protocol::ToolCallResult::success(
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
                );
                JsonRpcResponse::success(
                    request.id.clone(),
                    serde_json::to_value(call_result).unwrap_or_default(),
                )
            }
            Err(e) => {
                let call_result =
                    crate::protocol::ToolCallResult::error(format!("Backend tool error: {e}"));
                JsonRpcResponse::success(
                    request.id.clone(),
                    serde_json::to_value(call_result).unwrap_or_default(),
                )
            }
        };
    }

    // Fall through to skill / governance tool layer
    let registry_url = &config.registry_url;
    let result = if let Some(url) = registry_url {
        let http_policy = HttpPolicyClient::new(RegistryClient::new(url), state);
        handle_tool_call(
            &params.name,
            &params.arguments,
            state,
            &http_policy,
            model_client,
            registry_client,
            registry_url,
        )
    } else {
        let mock_policy = MockPolicyClient::new();
        handle_tool_call(
            &params.name,
            &params.arguments,
            state,
            &mock_policy,
            model_client,
            None,
            registry_url,
        )
    };

    JsonRpcResponse::success(
        request.id.clone(),
        serde_json::to_value(result).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::{
        BackendConnection, HttpBackend, ToolDefinition as AggToolDef, ToolVisibility,
    };

    fn temp_state(label: &str) -> (AppState, camino::Utf8PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = camino::Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("mcp-server-test-{label}-{nanos}")),
        )
        .unwrap();
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        (state, root)
    }

    fn make_request(id: u64, method: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(id)),
            method: method.to_string(),
            params: serde_json::json!({}),
        }
    }

    #[test]
    fn handle_initialize_returns_capabilities() {
        let (state, root) = temp_state("init-caps");
        let req = make_request(1, "initialize");
        let resp = handle_initialize(&req, &state);
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "skillrunner");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
        let _ = std::fs::remove_dir_all(root.as_str());
    }

    #[test]
    fn handle_initialize_includes_instructions() {
        let (state, root) = temp_state("init-instructions");
        let req = make_request(1, "initialize");
        let resp = handle_initialize(&req, &state);
        let result = resp.result.unwrap();
        let instructions = result["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("skillclub_list"),
            "instructions should mention skillclub_list, got: {instructions}"
        );
        assert!(
            instructions.contains("skillclub_search"),
            "instructions should mention skillclub_search, got: {instructions}"
        );
        assert!(
            instructions.contains("skillclub_install"),
            "instructions should mention skillclub_install, got: {instructions}"
        );
        let _ = std::fs::remove_dir_all(root.as_str());
    }

    #[test]
    fn handle_initialize_instructions_enforce_governance() {
        let (state, root) = temp_state("init-governance");
        let req = make_request(1, "initialize");
        let resp = handle_initialize(&req, &state);
        let result = resp.result.unwrap();
        let instructions = result["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("NEVER install MCP servers directly"),
            "instructions must prohibit direct MCP installation, got: {instructions}"
        );
        assert!(
            instructions.contains("skillclub_mcp_request"),
            "instructions must mention skillclub_mcp_request, got: {instructions}"
        );
        assert!(
            instructions.contains("skillclub_mcp_install"),
            "instructions must mention skillclub_mcp_install, got: {instructions}"
        );
        assert!(
            instructions.contains("NEVER use update-config"),
            "instructions must prohibit update-config, got: {instructions}"
        );
        let _ = std::fs::remove_dir_all(root.as_str());
    }

    #[test]
    fn handle_initialize_includes_org_name_from_managed_config() {
        let (state, root) = temp_state("init-managed");
        // Write a managed.json with org name
        std::fs::write(
            state.root_dir.join("managed.json"),
            r#"{"managed": true, "org": "Acme Corp"}"#,
        )
        .unwrap();

        let req = make_request(1, "initialize");
        let resp = handle_initialize(&req, &state);
        let result = resp.result.unwrap();
        let instructions = result["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("managed by Acme Corp"),
            "instructions should include org name, got: {instructions}"
        );
        let _ = std::fs::remove_dir_all(root.as_str());
    }

    #[test]
    fn handle_tools_list_returns_skill_tools() {
        let (state, root) = temp_state("tools-list");

        let req = make_request(2, "tools/list");
        let aggregator = BackendRegistry::new();

        let resp = handle_tools_list(
            &req,
            &state,
            &Some("http://localhost:8000".to_string()),
            &aggregator,
        );
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();

        // Without auth tokens, only local tools and login should appear
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"skillclub_list"));
        assert!(names.contains(&"skillclub_login"));
        assert!(
            !names.contains(&"skillclub_search"),
            "search requires login"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn handle_tools_list_includes_aggregator_backends() {
        let (state, root) = temp_state("tools-list-agg");

        // Inject a backend with tools directly into the registry
        let aggregator = BackendRegistry::new();
        {
            let mut inner = aggregator.inner.lock().unwrap();
            inner.backends.insert(
                "github".to_string(),
                BackendConnection::Http(HttpBackend {
                    server_id: "github".to_string(),
                    name: "GitHub".to_string(),
                    url: "http://unused".to_string(),
                    tools: vec![AggToolDef {
                        name: "create_issue".to_string(),
                        description: Some("Create a GitHub issue".to_string()),
                        input_schema: None,
                    }],
                    tool_visibility: ToolVisibility::All,
                    priority: 50,
                    auth_token: None,
                }),
            );
        }

        let req = make_request(3, "tools/list");
        let resp = handle_tools_list(&req, &state, &None, &aggregator);
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Namespaced aggregator tool should appear
        assert!(
            names.contains(&"github__create_issue"),
            "expected github__create_issue in tool list: {names:?}"
        );
        // Existing skill tool should still appear
        assert!(names.contains(&"skillclub_list"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dispatch_routes_namespaced_tool_to_aggregator() {
        let (_state, root) = temp_state("dispatch-agg");

        // Inject a backend — dispatch will find no tool (no real server) and
        // return an error result, not None. The key is it doesn't fall through
        // to the skill layer.
        let aggregator = BackendRegistry::new();
        {
            let mut inner = aggregator.inner.lock().unwrap();
            inner.backends.insert(
                "github".to_string(),
                BackendConnection::Http(HttpBackend {
                    server_id: "github".to_string(),
                    name: "GitHub".to_string(),
                    url: "http://127.0.0.1:1".to_string(), // unreachable
                    tools: vec![AggToolDef {
                        name: "create_issue".to_string(),
                        description: None,
                        input_schema: None,
                    }],
                    tool_visibility: ToolVisibility::All,
                    priority: 50,
                    auth_token: None,
                }),
            );
        }

        // Dispatch a namespaced tool call
        let dispatch_result = aggregator.dispatch(
            "github__create_issue",
            &serde_json::json!({"title": "test"}),
        );
        // Should get Some(Err(...)) because the backend is unreachable, not None
        assert!(
            dispatch_result.is_some(),
            "aggregator should intercept namespaced tool, not return None"
        );
        assert!(
            dispatch_result.unwrap().is_err(),
            "unreachable backend should return an error"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dispatch_unknown_method_returns_error() {
        let (state, root) = temp_state("unknown-method");
        let config = McpServerConfig {
            registry_url: None,
            ollama_url: "http://localhost:11434".to_string(),
            model: "llama3.2".to_string(),
        };
        let aggregator = BackendRegistry::new();

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(3)),
            method: "unknown/method".to_string(),
            params: serde_json::json!({}),
        };

        let resp = dispatch_request(
            &req,
            &state,
            &config,
            None,
            None,
            &aggregator,
            "test-machine",
        );
        assert!(resp.error.is_some());
        assert_eq!(resp.error.as_ref().unwrap().code, METHOD_NOT_FOUND);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maybe_sync_skills_throttled() {
        let (state, root) = temp_state("sync-skills-throttle");

        // No registry URL → should always return false without touching last_skill_sync.
        let mut server_state = ServerState::new(None);
        let result = server_state.maybe_sync_skills(&state);
        assert!(!result, "no registry URL → should return false");
        assert!(
            server_state.last_skill_sync.is_none(),
            "last_skill_sync should remain None when no registry is configured"
        );

        // With an unreachable registry, first call should stamp last_skill_sync.
        let mut server_state2 = ServerState::new(Some(RegistryClient::new("http://localhost:0")));
        // No skills installed → check_skill_updates returns Ok(0), which is Ok(false).
        // But the stamp still gets set before the check.
        let _first = server_state2.maybe_sync_skills(&state);
        assert!(
            server_state2.last_skill_sync.is_some(),
            "last_skill_sync should be stamped after first attempt"
        );

        // Second call immediately after: elapsed << 300 s → should be throttled.
        // Manually set last_skill_sync to just now so the throttle fires reliably.
        server_state2.last_skill_sync = Some(Instant::now());
        let result_throttled = server_state2.maybe_sync_skills(&state);
        assert!(
            !result_throttled,
            "call within throttle window should return false"
        );

        // Force the timestamp to look like it was 301 s ago → should attempt again.
        server_state2.last_skill_sync = Some(Instant::now() - Duration::from_secs(301));
        // No skills installed so nothing updates, but the call should proceed (not throttle).
        // Result doesn't matter — we just verify no panic and the stamp is refreshed.
        let _result_unthrottled = server_state2.maybe_sync_skills(&state);
        assert!(
            server_state2
                .last_skill_sync
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(999)
                < 2,
            "last_skill_sync should be refreshed after an unthrottled call"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
