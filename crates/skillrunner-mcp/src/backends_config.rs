//! Local `backends.yaml` configuration for declaring MCP backend servers
//! without a registry connection.
//!
//! The file lives at `{state_dir}/backends.yaml` and is loaded by the
//! aggregator's `sync_local()` method. When both a registry and local config
//! exist, the backends are merged (local entries added alongside registry ones).
//!
//! # Example `backends.yaml`
//!
//! ```yaml
//! backends:
//!   - name: GitHub
//!     transport: stdio
//!     command: npx
//!     args: ["-y", "@modelcontextprotocol/server-github"]
//!     env:
//!       GITHUB_TOKEN: "ghp_xxxx"
//!
//!   - name: Sentry
//!     server_id: sentry
//!     transport: http
//!     url: http://localhost:3001/mcp
//!     priority: 60
//! ```

use crate::aggregator::{
    sanitize_id, BackendConnection, HttpBackend, StdioBackend, ToolVisibility,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use skillrunner_core::state::AppState;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Top-level structure of `backends.yaml`.
#[derive(Debug, Deserialize)]
pub struct BackendsConfig {
    /// List of backend server declarations.
    #[serde(default)]
    pub backends: Vec<BackendEntry>,
}

/// A single backend server entry in the local config.
#[derive(Debug, Deserialize)]
pub struct BackendEntry {
    /// Human-readable display name (required).
    pub name: String,

    /// Stable identifier for tool namespacing. Defaults to a sanitized form
    /// of `name` if omitted.
    #[serde(default)]
    pub server_id: Option<String>,

    /// Transport type: `"stdio"` or `"http"`.
    pub transport: String,

    /// Command to run (stdio transport only).
    #[serde(default)]
    pub command: Option<String>,

    /// Arguments for the command (stdio transport only).
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables to set for the child process (stdio only).
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// URL of the MCP server (http transport only).
    #[serde(default)]
    pub url: Option<String>,

    /// Priority for tool budget allocation (higher = more important). Default: 50.
    #[serde(default = "default_priority")]
    pub priority: u8,
}

fn default_priority() -> u8 {
    50
}

/// Resolve the effective server_id for a config entry.
fn effective_id(entry: &BackendEntry) -> String {
    entry
        .server_id
        .clone()
        .unwrap_or_else(|| sanitize_id(&entry.name))
}

/// Load the local `backends.yaml` from the state directory.
///
/// Returns an empty vec if the file does not exist. Returns an error only if
/// the file exists but cannot be parsed.
pub fn load_local_backends(state: &AppState) -> Result<Vec<BackendConnection>> {
    let config_path = state.root_dir.join("backends.yaml");

    if !config_path.exists() {
        debug!(path = %config_path, "no local backends.yaml found — skipping");
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {config_path}"))?;

    // Handle empty files gracefully (serde_yaml parses "" as null).
    if content.trim().is_empty() {
        debug!(path = %config_path, "backends.yaml is empty — no backends to load");
        return Ok(vec![]);
    }

    let config: BackendsConfig =
        serde_yaml::from_str(&content).with_context(|| format!("failed to parse {config_path}"))?;

    let mut connections = Vec::new();

    for entry in &config.backends {
        let server_id = effective_id(entry);

        match entry.transport.as_str() {
            "stdio" => {
                let Some(command) = &entry.command else {
                    warn!(
                        server_id = %server_id,
                        "skipping stdio backend — missing 'command' field"
                    );
                    continue;
                };
                connections.push(BackendConnection::Stdio(StdioBackend {
                    server_id,
                    name: entry.name.clone(),
                    command: command.clone(),
                    args: entry.args.clone(),
                    env: entry.env.clone(),
                    tools: vec![],
                    tool_visibility: ToolVisibility::All,
                    priority: entry.priority,
                    process: Arc::new(Mutex::new(None)),
                }));
            }
            "http" => {
                let Some(url) = &entry.url else {
                    warn!(
                        server_id = %server_id,
                        "skipping http backend — missing 'url' field"
                    );
                    continue;
                };
                connections.push(BackendConnection::Http(HttpBackend {
                    server_id,
                    name: entry.name.clone(),
                    url: url.clone(),
                    tools: vec![],
                    tool_visibility: ToolVisibility::All,
                    priority: entry.priority,
                    auth_token: None,
                }));
            }
            other => {
                warn!(
                    server_id = %server_id,
                    transport = %other,
                    "skipping backend — unknown transport type (expected 'stdio' or 'http')"
                );
            }
        }
    }

    info!(
        count = connections.len(),
        path = %config_path,
        "loaded local backends from config"
    );
    Ok(connections)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use skillrunner_core::state::AppState;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_state(test_name: &str) -> AppState {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sr-backends-test-{test_name}-{nanos}"));
        let root = Utf8PathBuf::from_path_buf(path).expect("temp path should be utf-8");
        AppState::bootstrap_in(root).expect("bootstrap should succeed")
    }

    fn cleanup(state: &AppState) {
        let _ = std::fs::remove_dir_all(&state.root_dir);
    }

    // ── Deserialization ──────────────────────────────────────────────────────

    #[test]
    fn parse_empty_backends_list() {
        let yaml = "backends: []\n";
        let config: BackendsConfig =
            serde_yaml::from_str(yaml).expect("empty backends should parse");
        assert!(config.backends.is_empty());
    }

    #[test]
    fn parse_stdio_backend() {
        let yaml = r#"
backends:
  - name: GitHub
    transport: stdio
    command: npx
    args: ["-y", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_TOKEN: "ghp_xxxx"
"#;
        let config: BackendsConfig = serde_yaml::from_str(yaml).expect("should parse");
        assert_eq!(config.backends.len(), 1);
        let entry = &config.backends[0];
        assert_eq!(entry.name, "GitHub");
        assert_eq!(entry.transport, "stdio");
        assert_eq!(entry.command.as_deref(), Some("npx"));
        assert_eq!(
            entry.args,
            vec!["-y", "@modelcontextprotocol/server-github"]
        );
        assert_eq!(
            entry.env.get("GITHUB_TOKEN").map(String::as_str),
            Some("ghp_xxxx")
        );
        assert_eq!(entry.priority, 50); // default
    }

    #[test]
    fn parse_http_backend_with_custom_priority() {
        let yaml = r#"
backends:
  - name: Sentry
    server_id: sentry
    transport: http
    url: http://localhost:3001/mcp
    priority: 60
"#;
        let config: BackendsConfig = serde_yaml::from_str(yaml).expect("should parse");
        assert_eq!(config.backends.len(), 1);
        let entry = &config.backends[0];
        assert_eq!(entry.name, "Sentry");
        assert_eq!(entry.server_id.as_deref(), Some("sentry"));
        assert_eq!(entry.transport, "http");
        assert_eq!(entry.url.as_deref(), Some("http://localhost:3001/mcp"));
        assert_eq!(entry.priority, 60);
    }

    #[test]
    fn parse_multiple_backends() {
        let yaml = r#"
backends:
  - name: GitHub
    transport: stdio
    command: npx
    args: ["-y", "@modelcontextprotocol/server-github"]
  - name: Sentry
    transport: http
    url: http://localhost:3001/mcp
"#;
        let config: BackendsConfig = serde_yaml::from_str(yaml).expect("should parse");
        assert_eq!(config.backends.len(), 2);
    }

    // ── effective_id ─────────────────────────────────────────────────────────

    #[test]
    fn effective_id_uses_server_id_when_present() {
        let entry = BackendEntry {
            name: "GitHub MCP".to_string(),
            server_id: Some("github".to_string()),
            transport: "http".to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some("http://x".to_string()),
            priority: 50,
        };
        assert_eq!(effective_id(&entry), "github");
    }

    #[test]
    fn effective_id_falls_back_to_sanitized_name() {
        let entry = BackendEntry {
            name: "GitHub MCP".to_string(),
            server_id: None,
            transport: "http".to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some("http://x".to_string()),
            priority: 50,
        };
        assert_eq!(effective_id(&entry), "github-mcp");
    }

    // ── load_local_backends ──────────────────────────────────────────────────

    #[test]
    fn load_returns_empty_when_file_missing() {
        let state = temp_state("no-file");
        let backends = load_local_backends(&state).expect("should succeed");
        assert!(backends.is_empty());
        cleanup(&state);
    }

    #[test]
    fn load_returns_backends_from_valid_yaml() {
        let state = temp_state("valid-yaml");
        let config_path = state.root_dir.join("backends.yaml");
        std::fs::write(
            &config_path,
            r#"
backends:
  - name: GitHub
    transport: stdio
    command: npx
    args: ["-y", "@modelcontextprotocol/server-github"]
  - name: Sentry
    server_id: sentry
    transport: http
    url: http://localhost:3001/mcp
    priority: 70
"#,
        )
        .expect("write config");

        let backends = load_local_backends(&state).expect("should succeed");
        assert_eq!(backends.len(), 2);

        // First backend: stdio
        assert_eq!(backends[0].server_id(), "github");
        assert_eq!(backends[0].name(), "GitHub");
        assert_eq!(backends[0].priority(), 50);
        match &backends[0] {
            BackendConnection::Stdio(s) => {
                assert_eq!(s.command, "npx");
                assert_eq!(s.args, vec!["-y", "@modelcontextprotocol/server-github"]);
            }
            BackendConnection::Http(_) => panic!("expected stdio backend"),
        }

        // Second backend: http
        assert_eq!(backends[1].server_id(), "sentry");
        assert_eq!(backends[1].name(), "Sentry");
        assert_eq!(backends[1].priority(), 70);
        match &backends[1] {
            BackendConnection::Http(h) => {
                assert_eq!(h.url, "http://localhost:3001/mcp");
            }
            BackendConnection::Stdio(_) => panic!("expected http backend"),
        }

        cleanup(&state);
    }

    #[test]
    fn load_skips_stdio_without_command() {
        let state = temp_state("no-command");
        let config_path = state.root_dir.join("backends.yaml");
        std::fs::write(
            &config_path,
            r#"
backends:
  - name: Bad Stdio
    transport: stdio
"#,
        )
        .expect("write config");

        let backends = load_local_backends(&state).expect("should succeed");
        assert!(
            backends.is_empty(),
            "backend without command should be skipped"
        );
        cleanup(&state);
    }

    #[test]
    fn load_skips_http_without_url() {
        let state = temp_state("no-url");
        let config_path = state.root_dir.join("backends.yaml");
        std::fs::write(
            &config_path,
            r#"
backends:
  - name: Bad Http
    transport: http
"#,
        )
        .expect("write config");

        let backends = load_local_backends(&state).expect("should succeed");
        assert!(backends.is_empty(), "backend without url should be skipped");
        cleanup(&state);
    }

    #[test]
    fn load_skips_unknown_transport() {
        let state = temp_state("unknown-transport");
        let config_path = state.root_dir.join("backends.yaml");
        std::fs::write(
            &config_path,
            r#"
backends:
  - name: Weird
    transport: websocket
    url: ws://localhost:9999
"#,
        )
        .expect("write config");

        let backends = load_local_backends(&state).expect("should succeed");
        assert!(backends.is_empty(), "unknown transport should be skipped");
        cleanup(&state);
    }

    #[test]
    fn load_errors_on_invalid_yaml() {
        let state = temp_state("bad-yaml");
        let config_path = state.root_dir.join("backends.yaml");
        std::fs::write(&config_path, "{{not valid yaml").expect("write config");

        let result = load_local_backends(&state);
        assert!(result.is_err(), "invalid YAML should return an error");
        cleanup(&state);
    }

    #[test]
    fn load_handles_empty_file_gracefully() {
        let state = temp_state("empty-file");
        let config_path = state.root_dir.join("backends.yaml");
        std::fs::write(&config_path, "").expect("write config");

        // Empty YAML deserializes as null — we should handle this gracefully
        // and return an empty list rather than erroring.
        let backends = load_local_backends(&state).expect("empty file should succeed");
        assert!(backends.is_empty());
        cleanup(&state);
    }
}
