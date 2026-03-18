use crate::protocol::{ToolCallResult, ToolDefinition};
use anyhow::Result;
use rusqlite::Connection;
use skillrunner_core::{
    executor::run_skill,
    model::ModelClient,
    policy::PolicyClient,
    registry::RegistryClient,
    state::AppState,
    updater::install_from_registry,
};
use skillrunner_manifest::SkillPackage;
use std::fs;

// ── Tool registry ────────────────────────────────────────────────────────────

/// Builds the list of MCP tool definitions from installed skills + management tools.
pub fn build_tool_list(state: &AppState, registry_url: &Option<String>) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    // Add installed skills as tools
    if let Ok(skill_tools) = skill_tools_from_db(state) {
        tools.extend(skill_tools);
    }

    // Add management tools
    tools.push(ToolDefinition {
        name: "skillclub_list".to_string(),
        description: "List all installed SkillClub skills with their versions and status."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    });

    if registry_url.is_some() {
        tools.push(ToolDefinition {
            name: "skillclub_search".to_string(),
            description: "Search the SkillClub registry for available skills.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query to find skills (e.g., 'contract', 'analysis')"
                    }
                },
                "required": ["query"]
            }),
        });

        tools.push(ToolDefinition {
            name: "skillclub_install".to_string(),
            description: "Install a skill from the SkillClub registry by its ID.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_id": {
                        "type": "string",
                        "description": "The ID of the skill to install from the registry"
                    },
                    "version": {
                        "type": "string",
                        "description": "Optional specific version to install (default: latest)"
                    }
                },
                "required": ["skill_id"]
            }),
        });

        tools.push(ToolDefinition {
            name: "skillclub_info".to_string(),
            description: "Show detailed information about an installed SkillClub skill.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_id": {
                        "type": "string",
                        "description": "The ID of the installed skill to get info about"
                    }
                },
                "required": ["skill_id"]
            }),
        });
    }

    tools
}

/// Load installed skills from SQLite and convert to MCP tool definitions.
fn skill_tools_from_db(state: &AppState) -> Result<Vec<ToolDefinition>> {
    let conn = Connection::open(&state.db_path)?;
    let mut stmt = conn.prepare(
        "SELECT skill_id, install_root FROM installed_skills WHERE current_status = 'active'",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut tools = Vec::new();
    for row in rows {
        let (skill_id, install_root) = row?;
        let active_path = format!("{}/active", install_root);
        if let Ok(tool) = skill_to_tool(&skill_id, &active_path) {
            tools.push(tool);
        }
    }

    Ok(tools)
}

/// Convert a single installed skill into an MCP tool definition.
fn skill_to_tool(skill_id: &str, active_path: &str) -> Result<ToolDefinition> {
    let pkg = SkillPackage::load_from_dir(active_path)?;

    let description = pkg
        .manifest
        .description
        .clone()
        .unwrap_or_else(|| format!("SkillClub skill: {}", pkg.manifest.name));

    // Read the input schema file to use as the tool's inputSchema
    let schema_path = pkg.root.join(&pkg.manifest.inputs_schema);
    let schema_text = fs::read_to_string(&schema_path)?;
    let input_schema: serde_json::Value = serde_json::from_str(&schema_text)?;

    Ok(ToolDefinition {
        name: skill_id.to_string(),
        description,
        input_schema,
    })
}

// ── Tool dispatch ────────────────────────────────────────────────────────────

/// Execute a tool call and return the MCP result.
pub fn handle_tool_call(
    name: &str,
    arguments: &serde_json::Value,
    state: &AppState,
    policy_client: &dyn PolicyClient,
    model_client: Option<&dyn ModelClient>,
    registry_client: Option<&RegistryClient>,
    registry_url: &Option<String>,
) -> ToolCallResult {
    match name {
        "skillclub_list" => handle_list(state),
        "skillclub_search" => handle_search(arguments, registry_url),
        "skillclub_install" => handle_install(arguments, state, registry_url),
        "skillclub_info" => handle_info(arguments, state),
        _ => handle_skill_run(name, arguments, state, policy_client, model_client, registry_client),
    }
}

// ── Management tool handlers ─────────────────────────────────────────────────

fn handle_list(state: &AppState) -> ToolCallResult {
    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Failed to open state DB: {e}")),
    };

    let mut stmt = match conn.prepare(
        "SELECT skill_id, active_version, current_status FROM installed_skills ORDER BY skill_id",
    ) {
        Ok(s) => s,
        Err(e) => return ToolCallResult::error(format!("Failed to query skills: {e}")),
    };

    let rows = match stmt.query_map([], |row| {
        Ok(serde_json::json!({
            "skill_id": row.get::<_, String>(0)?,
            "version": row.get::<_, String>(1)?,
            "status": row.get::<_, String>(2)?,
        }))
    }) {
        Ok(r) => r,
        Err(e) => return ToolCallResult::error(format!("Failed to read skills: {e}")),
    };

    let skills: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();

    if skills.is_empty() {
        ToolCallResult::success("No skills installed.")
    } else {
        match serde_json::to_string_pretty(&skills) {
            Ok(text) => ToolCallResult::success(text),
            Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
        }
    }
}

fn handle_search(arguments: &serde_json::Value, registry_url: &Option<String>) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    let query = match arguments.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return ToolCallResult::error("Missing required parameter: query"),
    };

    let registry = RegistryClient::new(url);
    match registry.search_skills(query) {
        Ok(results) => {
            if results.is_empty() {
                ToolCallResult::success(format!("No skills found matching '{query}'."))
            } else {
                let formatted: Vec<serde_json::Value> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "skill_id": r.skill_id,
                            "name": r.name,
                            "version": r.latest_version,
                            "publisher": r.publisher_name,
                            "description": r.description,
                        })
                    })
                    .collect();
                match serde_json::to_string_pretty(&formatted) {
                    Ok(text) => ToolCallResult::success(text),
                    Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
                }
            }
        }
        Err(e) => ToolCallResult::error(format!("Search failed: {e}")),
    }
}

fn handle_install(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return ToolCallResult::error("Missing required parameter: skill_id"),
    };

    let version = arguments
        .get("version")
        .and_then(|v| v.as_str());

    let registry = RegistryClient::new(url);
    match install_from_registry(state, &registry, skill_id, version) {
        Ok(installed_ver) => ToolCallResult::success(format!(
            "Successfully installed {skill_id}@{installed_ver} from registry."
        )),
        Err(e) => ToolCallResult::error(format!("Failed to install {skill_id}: {e}")),
    }
}

fn handle_info(arguments: &serde_json::Value, state: &AppState) -> ToolCallResult {
    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return ToolCallResult::error("Missing required parameter: skill_id"),
    };

    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Failed to open state DB: {e}")),
    };

    let row: Option<(String, String, String)> = match conn.query_row(
        "SELECT skill_id, active_version, install_root FROM installed_skills WHERE skill_id = ?1",
        [skill_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ) {
        Ok(r) => Some(r),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return ToolCallResult::error(format!("Failed to query skill: {e}")),
    };

    let (_, version, install_root) = match row {
        Some(r) => r,
        None => return ToolCallResult::error(format!("Skill '{skill_id}' is not installed")),
    };

    let active_path = format!("{}/active", install_root);
    match SkillPackage::load_from_dir(&active_path) {
        Ok(pkg) => {
            let info = serde_json::json!({
                "skill_id": pkg.manifest.id,
                "name": pkg.manifest.name,
                "version": version,
                "publisher": pkg.manifest.publisher,
                "description": pkg.manifest.description,
                "steps": pkg.workflow.steps.len(),
                "permissions": {
                    "filesystem": pkg.manifest.permissions.filesystem,
                    "network": pkg.manifest.permissions.network,
                    "clipboard": pkg.manifest.permissions.clipboard,
                },
                "model_requirements": pkg.manifest.model_requirements.as_ref().map(|r| serde_json::json!({
                    "min_context_tokens": r.min_context_tokens,
                    "supports_structured_output": r.supports_structured_output,
                    "supports_tool_calling": r.supports_tool_calling,
                    "preferred_execution": r.preferred_execution,
                })),
            });
            match serde_json::to_string_pretty(&info) {
                Ok(text) => ToolCallResult::success(text),
                Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
            }
        }
        Err(e) => ToolCallResult::error(format!("Failed to load skill package: {e}")),
    }
}

// ── Skill execution handler ──────────────────────────────────────────────────

fn handle_skill_run(
    skill_id: &str,
    arguments: &serde_json::Value,
    state: &AppState,
    policy_client: &dyn PolicyClient,
    model_client: Option<&dyn ModelClient>,
    registry_client: Option<&RegistryClient>,
) -> ToolCallResult {
    match run_skill(state, policy_client, skill_id, arguments, model_client, registry_client) {
        Ok(result) => {
            // Return the last step's output, or a summary if no output
            let output = result
                .steps
                .iter()
                .rev()
                .find_map(|s| s.output.as_ref())
                .cloned()
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "status": "completed",
                        "skill_id": result.skill_id,
                        "version": result.version,
                        "steps_completed": result.steps.len(),
                    })
                });

            let text = match &output {
                serde_json::Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };

            ToolCallResult::success(text)
        }
        Err(e) => ToolCallResult::error(format!("Skill execution failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use skillrunner_core::{install::install_unpacked_skill, policy::MockPolicyClient};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("mcp-tests-{label}-{nanos}")),
        )
        .unwrap()
    }

    fn write_test_skill(root: &Utf8PathBuf) {
        fs::create_dir_all(root.join("schemas")).unwrap();
        fs::create_dir_all(root.join("prompts")).unwrap();
        fs::write(
            root.join("manifest.json"),
            r#"{
  "schema_version": "1.0",
  "id": "test-skill",
  "name": "Test Skill",
  "version": "0.1.0",
  "publisher": "skillclub",
  "description": "A test skill for MCP testing",
  "entrypoint": "workflow.yaml",
  "inputs_schema": "schemas/input.schema.json",
  "outputs_schema": "schemas/output.schema.json",
  "permissions": { "filesystem": "none", "network": "none", "clipboard": false },
  "execution": { "sandbox_profile": "strict", "timeout_seconds": 30, "memory_mb": 256 }
}"#,
        )
        .unwrap();
        fs::write(
            root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .unwrap();
        fs::write(root.join("prompts/system.txt"), "Do the thing.").unwrap();
        fs::write(
            root.join("schemas/input.schema.json"),
            r#"{"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}"#,
        )
        .unwrap();
        fs::write(root.join("schemas/output.schema.json"), "{}").unwrap();
    }

    #[test]
    fn build_tool_list_includes_management_tools() {
        let state_root = temp_root("tool-list");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let tools = build_tool_list(&state, &Some("http://localhost:8000".to_string()));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"skillclub_list"));
        assert!(names.contains(&"skillclub_search"));
        assert!(names.contains(&"skillclub_install"));
        assert!(names.contains(&"skillclub_info"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_without_registry_omits_registry_tools() {
        let state_root = temp_root("tool-list-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let tools = build_tool_list(&state, &None);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"skillclub_list"));
        assert!(!names.contains(&"skillclub_search"));
        assert!(!names.contains(&"skillclub_install"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_includes_installed_skill() {
        let state_root = temp_root("tool-list-skill");
        let skill_root = temp_root("tool-list-skill-bundle");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let tools = build_tool_list(&state, &None);
        let skill_tool = tools.iter().find(|t| t.name == "test-skill");

        assert!(skill_tool.is_some(), "installed skill should appear as tool");
        let tool = skill_tool.unwrap();
        assert_eq!(tool.description, "A test skill for MCP testing");
        // The input schema should match the skill's input.schema.json
        assert_eq!(tool.input_schema["type"], "object");
        assert!(tool.input_schema["properties"]["query"].is_object());

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_list_returns_installed_skills() {
        let state_root = temp_root("handle-list");
        let skill_root = temp_root("handle-list-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let result = handle_list(&state);
        assert!(result.is_error.is_none());
        let text = &result.content[0].text;
        assert!(text.contains("test-skill"), "should list test-skill, got: {text}");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_list_empty() {
        let state_root = temp_root("handle-list-empty");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_list(&state);
        assert!(result.is_error.is_none());
        assert!(result.content[0].text.contains("No skills installed"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_search_requires_query() {
        let result = handle_search(
            &serde_json::json!({}),
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("query"));
    }

    #[test]
    fn handle_search_requires_registry() {
        let result = handle_search(&serde_json::json!({"query": "test"}), &None);
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));
    }

    #[test]
    fn handle_install_requires_skill_id() {
        let state_root = temp_root("handle-install-no-id");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_install(
            &serde_json::json!({}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("skill_id"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_info_not_installed() {
        let state_root = temp_root("handle-info-missing");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_info(&serde_json::json!({"skill_id": "ghost"}), &state);
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("not installed"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_info_returns_skill_details() {
        let state_root = temp_root("handle-info-ok");
        let skill_root = temp_root("handle-info-ok-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let result = handle_info(&serde_json::json!({"skill_id": "test-skill"}), &state);
        assert!(result.is_error.is_none());
        let text = &result.content[0].text;
        assert!(text.contains("test-skill"), "got: {text}");
        assert!(text.contains("Test Skill"), "got: {text}");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_skill_run_not_installed() {
        let state_root = temp_root("handle-run-missing");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let policy = MockPolicyClient::new();

        let result = handle_skill_run(
            "ghost-skill",
            &serde_json::json!({}),
            &state,
            &policy,
            None,
            None,
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("not installed"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_skill_run_executes_installed_skill() {
        let state_root = temp_root("handle-run-ok");
        let skill_root = temp_root("handle-run-ok-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let policy = MockPolicyClient::new();
        let result = handle_skill_run(
            "test-skill",
            &serde_json::json!({"query": "hello"}),
            &state,
            &policy,
            None, // stub mode
            None,
        );
        // Stub mode returns no output, so we get the summary
        assert!(result.is_error.is_none());

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }
}
