use crate::{
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
    model::ModelClient,
    ollama::OllamaClient,
    policy::MockPolicyClient,
    registry::{HttpPolicyClient, RegistryClient},
    state::AppState,
};
use std::io;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info};

/// Configuration for the MCP server.
pub struct McpServerConfig {
    pub registry_url: Option<String>,
    pub ollama_url: String,
    pub model: String,
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

    let ollama = OllamaClient::new(&config.ollama_url, &config.model);
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

        // Dispatch the request — during tools/call, this may trigger
        // sampling requests through the shared_io (the lock is not held here)
        let response = dispatch_request(
            &request,
            &state,
            &config,
            Some(&hybrid_client),
            registry_client.as_ref(),
        );

        // Write response (lock shared_io)
        {
            let mut io = shared_io.lock().unwrap();
            io.write_message(&serde_json::to_string(&response)?)?;
        }

        // After install, notify client that tools list changed
        if request.method == "tools/call" {
            if let Ok(params) = serde_json::from_value::<ToolCallParams>(request.params.clone()) {
                if params.name == "skillclub_install" {
                    let notification =
                        JsonRpcNotification::new("notifications/tools/list_changed");
                    let mut io = shared_io.lock().unwrap();
                    io.write_message(&serde_json::to_string(&notification)?)?;
                }
            }
        }
    }

    info!("MCP server shutting down");
    Ok(())
}

fn dispatch_request(
    request: &JsonRpcRequest,
    state: &AppState,
    config: &McpServerConfig,
    model_client: Option<&dyn ModelClient>,
    registry_client: Option<&RegistryClient>,
) -> JsonRpcResponse {
    match request.method.as_str() {
        "initialize" => handle_initialize(request),
        "tools/list" => handle_tools_list(request, state, &config.registry_url),
        "tools/call" => handle_tools_call(
            request,
            state,
            config,
            model_client,
            registry_client,
        ),
        _ => JsonRpcResponse::error(
            request.id.clone(),
            METHOD_NOT_FOUND,
            format!("Unknown method: {}", request.method),
        ),
    }
}

fn handle_initialize(request: &JsonRpcRequest) -> JsonRpcResponse {
    let result = InitializeResult {
        protocol_version: "2024-11-05".to_string(),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability { list_changed: true }),
        },
        server_info: ServerInfo {
            name: "skillrunner".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
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
) -> JsonRpcResponse {
    let tools = build_tool_list(state, registry_url);
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

    // Build policy client based on registry availability
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

    #[test]
    fn handle_initialize_returns_capabilities() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "initialize".to_string(),
            params: serde_json::json!({}),
        };

        let resp = handle_initialize(&req);
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "skillrunner");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
    }

    #[test]
    fn handle_tools_list_returns_tools() {
        let state_root = camino::Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!(
                "mcp-server-test-tools-list-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        )
        .unwrap();
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(2)),
            method: "tools/list".to_string(),
            params: serde_json::json!({}),
        };

        let resp = handle_tools_list(
            &req,
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"skillclub_list"));
        assert!(names.contains(&"skillclub_search"));

        let _ = std::fs::remove_dir_all(&state_root);
    }

    #[test]
    fn dispatch_unknown_method_returns_error() {
        let state_root = camino::Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!(
                "mcp-server-test-unknown-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        )
        .unwrap();
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let config = McpServerConfig {
            registry_url: None,
            ollama_url: "http://localhost:11434".to_string(),
            model: "llama3.2".to_string(),
        };

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(3)),
            method: "unknown/method".to_string(),
            params: serde_json::json!({}),
        };

        let resp = dispatch_request(&req, &state, &config, None, None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.as_ref().unwrap().code, METHOD_NOT_FOUND);

        let _ = std::fs::remove_dir_all(&state_root);
    }
}
