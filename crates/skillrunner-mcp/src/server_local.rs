//! Minimal MCP server for standalone (no-registry) builds.
//!
//! Supports local backends via `backends.yaml`, installed skill execution,
//! and basic management tools. No registry auth, governance, or auto-update.

use crate::{
    aggregator::BackendRegistry,
    protocol::{
        InitializeResult, JsonRpcRequest, JsonRpcResponse, ServerCapabilities, ServerInfo,
        ToolCallParams, ToolDefinition, ToolsCapability, ToolsListResult, INVALID_PARAMS,
        METHOD_NOT_FOUND,
    },
    sampling::{HybridModelClient, McpSamplingClient, SharedIo},
    tools::{build_tool_list, handle_tool_call},
};
use anyhow::Result;
use skillrunner_core::{model::ModelClient, ollama::OllamaClient, state::AppState};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

const AGGREGATOR_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Configuration for the MCP server.
pub struct McpServerConfig {
    pub registry_url: Option<String>,
    pub ollama_url: String,
    pub model: String,
}

struct ServerState {
    aggregator: BackendRegistry,
    last_aggregator_sync: Option<Instant>,
    last_aggregated_tool_count: usize,
}

impl ServerState {
    fn new() -> Self {
        Self {
            aggregator: BackendRegistry::new(),
            last_aggregator_sync: None,
            last_aggregated_tool_count: 0,
        }
    }

    fn maybe_sync_aggregator(&mut self, app_state: &AppState) -> bool {
        let should_sync = match self.last_aggregator_sync {
            None => true,
            Some(t) => t.elapsed() >= AGGREGATOR_REFRESH_INTERVAL,
        };

        if !should_sync {
            return false;
        }

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
                changed
            }
            Err(e) => {
                warn!(error = %e, "local backend sync failed");
                false
            }
        }
    }
}

/// Run the MCP server over stdio (standalone, no registry).
pub fn run_server(state: AppState, config: McpServerConfig) -> Result<()> {
    let shared_io = Arc::new(Mutex::new(SharedIo::new(
        Box::new(io::stdout()),
        Box::new(io::BufReader::new(io::stdin())),
    )));

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

    let sampling_client = McpSamplingClient::from_shared(Arc::clone(&shared_io));

    let hybrid_client = HybridModelClient::new(
        if ollama_available {
            Some(&ollama as &dyn ModelClient)
        } else {
            None
        },
        &sampling_client,
    );

    info!(
        "MCP server starting in standalone mode (ollama={}, registry=none)",
        if ollama_available {
            "available"
        } else {
            "unavailable"
        },
    );

    let mut server_state = ServerState::new();
    let _ = server_state.maybe_sync_aggregator(&state);

    loop {
        let line = {
            let mut io_guard = shared_io.lock().unwrap();
            match io_guard.read_line() {
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
                debug!(error = %e, "ignoring unparseable input");
                continue;
            }
        };

        // Skip notifications
        if request.id.is_none() {
            debug!(method = %request.method, "received notification — ignoring");
            continue;
        }

        let aggregator_changed = if request.method == "tools/list" {
            server_state.maybe_sync_aggregator(&state)
        } else {
            false
        };

        let response = dispatch_request(
            &request,
            &state,
            &config,
            Some(&hybrid_client),
            &server_state.aggregator,
        );

        {
            let mut io_guard = shared_io.lock().unwrap();
            io_guard.write_message(&serde_json::to_string(&response)?)?;
        }

        if aggregator_changed {
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed",
                "params": {}
            });
            let mut io_guard = shared_io.lock().unwrap();
            io_guard.write_message(&serde_json::to_string(&notification)?)?;
        }
    }

    server_state.aggregator.shutdown();
    Ok(())
}

fn dispatch_request(
    request: &JsonRpcRequest,
    state: &AppState,
    _config: &McpServerConfig,
    model_client: Option<&dyn ModelClient>,
    aggregator: &BackendRegistry,
) -> JsonRpcResponse {
    match request.method.as_str() {
        "initialize" => handle_initialize(request, state),
        "tools/list" => handle_tools_list(request, state, &_config.registry_url, aggregator),
        "tools/call" => handle_tools_call(request, state, model_client, aggregator),
        _ => JsonRpcResponse::error(
            request.id.clone(),
            METHOD_NOT_FOUND,
            format!("Method not found: {}", request.method),
        ),
    }
}

fn handle_initialize(request: &JsonRpcRequest, _state: &AppState) -> JsonRpcResponse {
    let result = InitializeResult {
        protocol_version: "2024-11-05".to_string(),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability { list_changed: true }),
        },
        server_info: ServerInfo {
            name: "skillrunner".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        instructions: None,
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
    let mut tools = build_tool_list(state, registry_url);

    // Convert aggregated backend tools to ToolDefinitions
    for agg_tool in aggregator.all_tools() {
        let name = agg_tool
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string();
        let description = agg_tool
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let input_schema = agg_tool
            .get("inputSchema")
            .cloned()
            .unwrap_or(serde_json::json!({"type": "object"}));
        tools.push(ToolDefinition {
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
    model_client: Option<&dyn ModelClient>,
    aggregator: &BackendRegistry,
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

    let args = if params.arguments.is_null() {
        serde_json::json!({})
    } else {
        params.arguments.clone()
    };

    // Check if this is an aggregated backend tool
    if let Some(result) = aggregator.dispatch(&params.name, &args) {
        return match result {
            Ok(value) => JsonRpcResponse::success(request.id.clone(), value),
            Err(e) => JsonRpcResponse::success(
                request.id.clone(),
                serde_json::json!({
                    "content": [{"type": "text", "text": format!("Error: {e}")}],
                    "isError": true
                }),
            ),
        };
    }

    let result = handle_tool_call(&params.name, &args, state, model_client, None::<&()>, "");

    let response_value = serde_json::to_value(result).unwrap_or_else(|e| {
        serde_json::json!({
            "content": [{"type": "text", "text": format!("Serialization error: {e}")}],
            "isError": true
        })
    });

    JsonRpcResponse::success(request.id.clone(), response_value)
}
