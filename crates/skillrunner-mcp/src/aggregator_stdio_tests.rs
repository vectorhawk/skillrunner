//! Integration tests for stdio backend wiring in the aggregator.
//!
//! These tests configure a `BackendRegistry` with a stdio backend (the same
//! Python echo server used in `stdio_process_tests`) and exercise the full
//! sync → list_tools → dispatch pipeline without a real MCP server binary.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod aggregator_stdio_tests {
    use crate::aggregator::{BackendConnection, BackendRegistry, StdioBackend, ToolVisibility};
    use camino::Utf8PathBuf;
    use skillrunner_core::state::AppState;
    use std::time::{SystemTime, UNIX_EPOCH};

    // ── Test helpers ──────────────────────────────────────────────────────────

    const ECHO_SERVER_SCRIPT: &str = r#"
import sys, json

def respond(req_id, result):
    msg = {"jsonrpc": "2.0", "id": req_id, "result": result}
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue

    req_id = req.get("id")
    method = req.get("method", "")

    if method == "initialize":
        respond(req_id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "echo-mcp", "version": "0.0.1"}
        })
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(req_id, {
            "tools": [{
                "name": "echo_tool",
                "description": "Echoes back the input",
                "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
            }]
        })
    elif method == "tools/call":
        args = req.get("params", {}).get("arguments", {})
        respond(req_id, {
            "content": [{"type": "text", "text": json.dumps(args)}],
            "isError": False
        })
    else:
        error_resp_obj = {"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": f"not found: {method}"}}
        sys.stdout.write(json.dumps(error_resp_obj) + "\n")
        sys.stdout.flush()
"#;

    fn temp_state(name: &str) -> AppState {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock ok")
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("sr-aggregator-stdio-test-{name}-{nanos}"));
        let root = Utf8PathBuf::from_path_buf(path).expect("utf-8 path");
        AppState::bootstrap_in(root).expect("bootstrap")
    }

    fn cleanup(state: &AppState) {
        let _ = std::fs::remove_dir_all(&state.root_dir);
    }

    /// Build a `StdioBackend` pointing at the Python echo server.
    fn echo_backend() -> StdioBackend {
        StdioBackend {
            server_id: "echo".to_string(),
            name: "Echo MCP".to_string(),
            command: "python3".to_string(),
            args: vec!["-c".to_string(), ECHO_SERVER_SCRIPT.to_string()],
            env: {
                let mut m = std::collections::HashMap::new();
                m.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
                m
            },
            tools: vec![],
            tool_visibility: ToolVisibility::All,
            priority: 50,
            process: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    // ── fetch_tools via registry internal path ────────────────────────────────

    #[test]
    fn fetch_tools_from_stdio_backend_returns_tool_list() {
        let registry = BackendRegistry::new();
        let backend = echo_backend();
        let conn = BackendConnection::Stdio(backend);
        let tools = registry
            .fetch_tools_from_backend_stdio(&conn)
            .expect("fetch_tools should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo_tool");
    }

    // ── sync_local with a backends.yaml ──────────────────────────────────────

    #[test]
    fn sync_local_with_stdio_backend_populates_tools() {
        let state = temp_state("sync-local");

        // Write a backends.yaml that uses the echo server via Python.
        // We use the inline script via -c to avoid needing a real binary.
        // Escape the script for YAML single-line embedding is tricky, so we
        // write it to a temp file and reference it.
        let script_path = state.root_dir.join("echo_server.py");
        std::fs::write(&script_path, ECHO_SERVER_SCRIPT).expect("write script");

        let yaml = format!(
            r#"
backends:
  - name: Echo
    server_id: echo
    transport: stdio
    command: python3
    args: ["{script}"]
    env:
      PYTHONUNBUFFERED: "1"
"#,
            script = script_path.as_str()
        );
        std::fs::write(state.root_dir.join("backends.yaml"), yaml).expect("write yaml");

        let registry = BackendRegistry::new();
        let count = registry.sync_local(&state).expect("sync_local should succeed");
        assert_eq!(count, 1, "one backend should be loaded");
        assert_eq!(registry.backend_count(), 1);

        let tools = registry.all_tools();
        assert_eq!(tools.len(), 1, "echo backend should expose one namespaced tool");
        assert_eq!(
            tools[0]["name"].as_str().unwrap_or(""),
            "echo__echo_tool"
        );

        registry.shutdown();
        cleanup(&state);
    }

    // ── dispatch ──────────────────────────────────────────────────────────────

    #[test]
    fn dispatch_routes_tool_call_to_stdio_backend() {
        let state = temp_state("dispatch");
        let script_path = state.root_dir.join("echo_server.py");
        std::fs::write(&script_path, ECHO_SERVER_SCRIPT).expect("write script");

        let yaml = format!(
            r#"
backends:
  - name: Echo
    server_id: echo
    transport: stdio
    command: python3
    args: ["{script}"]
    env:
      PYTHONUNBUFFERED: "1"
"#,
            script = script_path.as_str()
        );
        std::fs::write(state.root_dir.join("backends.yaml"), yaml).expect("write yaml");

        let registry = BackendRegistry::new();
        registry.sync_local(&state).expect("sync_local");

        let args = serde_json::json!({"text": "round-trip"});
        let result = registry
            .dispatch("echo__echo_tool", &args)
            .expect("tool should be recognized by aggregator");
        let value = result.expect("dispatch should succeed");

        // The result is the raw JSON-RPC result from the backend.
        // Echo server wraps args in content[].text.
        let text = value["content"][0]["text"].as_str().expect("text field");
        let echoed: serde_json::Value =
            serde_json::from_str(text).expect("text is JSON");
        assert_eq!(echoed["text"], "round-trip");

        registry.shutdown();
        cleanup(&state);
    }

    // ── dispatch returns None for non-namespaced tool ─────────────────────────

    #[test]
    fn dispatch_returns_none_for_skill_tool() {
        let registry = BackendRegistry::new();
        // This tool belongs to the skill layer, not an aggregator backend.
        assert!(registry.dispatch("skillclub_search", &serde_json::Value::Null).is_none());
    }

    // ── shutdown closes child processes ──────────────────────────────────────

    #[test]
    fn shutdown_terminates_stdio_processes() {
        let state = temp_state("shutdown");
        let script_path = state.root_dir.join("echo_server.py");
        std::fs::write(&script_path, ECHO_SERVER_SCRIPT).expect("write script");

        let yaml = format!(
            r#"
backends:
  - name: Echo
    server_id: echo
    transport: stdio
    command: python3
    args: ["{script}"]
    env:
      PYTHONUNBUFFERED: "1"
"#,
            script = script_path.as_str()
        );
        std::fs::write(state.root_dir.join("backends.yaml"), yaml).expect("write yaml");

        let registry = BackendRegistry::new();
        registry.sync_local(&state).expect("sync_local");

        // Force the process to spawn by doing a dispatch.
        let _ = registry.dispatch("echo__echo_tool", &serde_json::json!({}));

        // Now shut down — should not hang.
        registry.shutdown();
        assert_eq!(registry.backend_count(), 0);
        cleanup(&state);
    }

    // ── process death detection and restart ───────────────────────────────────

    #[test]
    fn dispatch_restarts_dead_process_and_succeeds() {
        // Server that exits after the first tools/call, then a second spawn
        // (restart) should work fine for the aggregator's one-restart policy.
        let script = r#"
import sys, json, os, signal

call_count = 0

for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    method = req.get("method", "")
    req_id = req.get("id")

    if method == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"flaky","version":"0"}
        }}) + "\n")
        sys.stdout.flush()
    elif method == "tools/list":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "tools":[{"name":"flaky_tool","description":"flaky","inputSchema":{"type":"object","properties":{}}}]
        }}) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        call_count += 1
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "content":[{"type":"text","text":"ok"}]
        }}) + "\n")
        sys.stdout.flush()
        if call_count == 1:
            sys.exit(0)  # die after first call
"#;
        let state = temp_state("restart");
        let script_path = state.root_dir.join("flaky_server.py");
        std::fs::write(&script_path, script).expect("write script");

        let yaml = format!(
            r#"
backends:
  - name: Flaky
    server_id: flaky
    transport: stdio
    command: python3
    args: ["{script}"]
    env:
      PYTHONUNBUFFERED: "1"
"#,
            script = script_path.as_str()
        );
        std::fs::write(state.root_dir.join("backends.yaml"), yaml).expect("write yaml");

        let registry = BackendRegistry::new();
        registry.sync_local(&state).expect("sync_local");

        // First call succeeds.
        let r1 = registry
            .dispatch("flaky__flaky_tool", &serde_json::json!({}))
            .expect("tool known")
            .expect("first call ok");
        assert_eq!(r1["content"][0]["text"].as_str(), Some("ok"));

        // Give the server a moment to exit.
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Second call triggers a restart. Should also succeed.
        let r2 = registry
            .dispatch("flaky__flaky_tool", &serde_json::json!({}))
            .expect("tool known")
            .expect("second call after restart ok");
        assert_eq!(r2["content"][0]["text"].as_str(), Some("ok"));

        registry.shutdown();
        cleanup(&state);
    }
}
