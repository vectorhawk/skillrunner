//! Tests for `StdioProcess` — uses a small Python/shell script to act as
//! a minimal MCP echo server so we can exercise the full request/response
//! cycle without spawning real MCP backends.
//!
//! The echo server script reads JSON-RPC lines and replies:
//! - `initialize`  → a valid MCP InitializeResult
//! - `tools/list`  → a tools/list result with one fake tool
//! - `tools/call`  → a tools/call result echoing the arguments back as text
//! - anything else → a JSON-RPC method-not-found error
//!
//! We use Python (universally available on macOS/Linux) instead of a shell
//! script so we can handle line-buffered JSON reliably.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod stdio_process_tests {
    use crate::stdio_process::StdioProcess;
    use std::collections::HashMap;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Inline Python MCP echo server. Reads JSON-RPC lines on stdin, writes
    /// JSON-RPC responses to stdout, each terminated by `\n`.
    const ECHO_SERVER_SCRIPT: &str = r#"
import sys, json

def respond(req_id, result):
    msg = {"jsonrpc": "2.0", "id": req_id, "result": result}
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

def error_resp(req_id, code, msg):
    obj = {"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": msg}}
    sys.stdout.write(json.dumps(obj) + "\n")
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
        pass  # no response for notifications
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
        error_resp(req_id, -32601, f"Method not found: {method}")
"#;

    /// Spawn a Python echo server and return a `StdioProcess` connected to it.
    fn spawn_echo_server() -> StdioProcess {
        let mut env = HashMap::new();
        // Ensure Python output is unbuffered.
        env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
        StdioProcess::spawn("python3", &["-c", ECHO_SERVER_SCRIPT], &env)
            .expect("failed to spawn echo server — is python3 available?")
    }

    // ── StdioProcess::spawn ───────────────────────────────────────────────────

    #[test]
    fn spawn_fails_for_nonexistent_command() {
        let result =
            StdioProcess::spawn("__nonexistent_binary_xyz__", &[] as &[&str], &HashMap::new());
        assert!(result.is_err(), "spawning a missing binary should return an error");
    }

    #[test]
    fn spawn_succeeds_for_valid_command() {
        let mut proc = spawn_echo_server();
        assert!(proc.is_alive(), "process should be alive after spawn");
    }

    // ── StdioProcess::initialize ──────────────────────────────────────────────

    #[test]
    fn initialize_succeeds_with_valid_server() {
        let mut proc = spawn_echo_server();
        proc.initialize().expect("initialize should succeed");
    }

    // ── StdioProcess::list_tools ──────────────────────────────────────────────

    #[test]
    fn list_tools_returns_expected_tool() {
        let mut proc = spawn_echo_server();
        proc.initialize().expect("initialize should succeed");
        let tools = proc.list_tools().expect("list_tools should succeed");
        assert_eq!(tools.len(), 1, "echo server exposes exactly one tool");
        assert_eq!(tools[0].name, "echo_tool");
        assert!(tools[0].description.is_some());
    }

    #[test]
    fn list_tools_without_initialize_also_works() {
        // StdioProcess should send initialize automatically if not yet done,
        // or the server may respond regardless — test that we handle it.
        let mut proc = spawn_echo_server();
        let tools = proc.list_tools().expect("list_tools should succeed even without explicit initialize");
        assert!(!tools.is_empty());
    }

    // ── StdioProcess::call_tool ───────────────────────────────────────────────

    #[test]
    fn call_tool_returns_echoed_args() {
        let mut proc = spawn_echo_server();
        proc.initialize().expect("initialize");
        let args = serde_json::json!({"text": "hello world"});
        let result = proc.call_tool("echo_tool", &args).expect("call_tool should succeed");
        // The echo server returns a tools/call result with a `content` array.
        // `call_tool` should extract and return the result value as-is.
        let content = result.get("content").expect("result should have content");
        let text = content[0]["text"].as_str().expect("first content should have text");
        let echoed: serde_json::Value = serde_json::from_str(text).expect("text should be valid JSON");
        assert_eq!(echoed["text"], "hello world");
    }

    #[test]
    fn call_tool_with_empty_args_succeeds() {
        let mut proc = spawn_echo_server();
        proc.initialize().expect("initialize");
        let result = proc.call_tool("echo_tool", &serde_json::Value::Null).expect("call_tool");
        assert!(result.get("content").is_some());
    }

    // ── StdioProcess::is_alive ────────────────────────────────────────────────

    #[test]
    fn is_alive_returns_false_after_shutdown() {
        let mut proc = spawn_echo_server();
        assert!(proc.is_alive());
        proc.shutdown().expect("shutdown should succeed");
        assert!(!proc.is_alive(), "process should be dead after shutdown");
    }

    // ── StdioProcess::shutdown ────────────────────────────────────────────────

    #[test]
    fn shutdown_is_idempotent() {
        let mut proc = spawn_echo_server();
        proc.shutdown().expect("first shutdown");
        // Second shutdown on a dead process should not panic or return an error
        // that causes a test failure — it may return Ok or a benign error.
        let _ = proc.shutdown();
    }

    // ── Environment variable passing ──────────────────────────────────────────

    #[test]
    fn env_vars_are_passed_to_child_process() {
        // Spawn a Python process that reads an env var and echoes it.
        let script = r#"
import sys, json, os
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    method = req.get("method", "")
    req_id = req.get("id")
    if method == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"e","version":"0"}
        }}) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        val = os.environ.get("MY_SECRET_TOKEN", "NOT_SET")
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "content":[{"type":"text","text":val}]
        }}) + "\n")
        sys.stdout.flush()
"#;
        let mut env = HashMap::new();
        env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
        env.insert("MY_SECRET_TOKEN".to_string(), "abc123".to_string());
        let mut proc = StdioProcess::spawn("python3", &["-c", script], &env)
            .expect("spawn env-echo server");
        proc.initialize().expect("initialize");
        let result = proc.call_tool("env_echo", &serde_json::Value::Null).expect("call_tool");
        let text = result["content"][0]["text"].as_str().expect("text");
        assert_eq!(text, "abc123", "env var should reach child process");
    }

    // ── Error response from backend ───────────────────────────────────────────

    #[test]
    fn call_tool_propagates_jsonrpc_error_from_backend() {
        // Spawn a server that always returns a JSON-RPC error on tools/call.
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    method = req.get("method", "")
    req_id = req.get("id")
    if method == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"result":{
            "protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"e","version":"0"}
        }}) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req_id,"error":{
            "code":-32000,"message":"internal backend error"
        }}) + "\n")
        sys.stdout.flush()
"#;
        let mut env = HashMap::new();
        env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
        let mut proc = StdioProcess::spawn("python3", &["-c", script], &env)
            .expect("spawn error server");
        proc.initialize().expect("initialize");
        let result = proc.call_tool("any_tool", &serde_json::Value::Null);
        assert!(result.is_err(), "JSON-RPC error from backend should propagate as Err");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("internal backend error") || msg.contains("JSON-RPC error"),
            "error message should describe the backend error, got: {msg}");
    }
}
