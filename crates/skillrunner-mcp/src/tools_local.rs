//! Minimal tool surface for standalone (no-registry) MCP server.
//!
//! Exposes installed skills as tools and basic management tools (list, validate,
//! import, run). Registry-dependent tools (auth, publish, governance) are omitted.

use crate::protocol::{ToolCallResult, ToolDefinition};
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rusqlite::Connection;
use skillrunner_core::{
    executor::run_skill,
    import::import_skill_md,
    install::install_unpacked_skill,
    model::ModelClient,
    policy::MockPolicyClient,
    state::AppState,
    validator::validate_bundle,
};
use skillrunner_manifest::SkillPackage;

/// Build the list of MCP tool definitions for standalone mode.
pub fn build_tool_list(state: &AppState, _registry_url: &Option<String>) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    // Installed skills as tools
    if let Ok(skill_tools) = skill_tools_from_db(state) {
        tools.extend(skill_tools);
    }

    // Management tools
    tools.push(ToolDefinition {
        name: "skillclub_list".to_string(),
        description: "List all installed skills".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_validate".to_string(),
        description: "Validate a skill bundle at a given path".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to skill bundle directory" }
            },
            "required": ["path"]
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_import".to_string(),
        description: "Import a SKILL.md file to create a skill bundle".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to SKILL.md file" }
            },
            "required": ["path"]
        }),
    });

    tools
}

/// Handle a tool call in standalone mode.
pub fn handle_tool_call(
    tool_name: &str,
    args: &serde_json::Value,
    state: &AppState,
    model_client: Option<&dyn ModelClient>,
    _registry_client: Option<&()>,
    _machine_id: &str,
) -> ToolCallResult {
    match tool_name {
        "skillclub_list" => handle_list(state),
        "skillclub_validate" => handle_validate(args),
        "skillclub_import" => handle_import(args, state),
        // Try installed skill execution
        other => handle_skill_run(other, args, state, model_client),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn skill_tools_from_db(state: &AppState) -> Result<Vec<ToolDefinition>> {
    let conn = Connection::open(&state.db_path)
        .context("failed to open state database")?;
    let mut stmt = conn.prepare(
        "SELECT skill_id, active_version, install_root FROM installed_skills WHERE current_status = 'active'"
    ).context("failed to query installed skills")?;

    let mut tools = Vec::new();
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    }).context("failed to iterate installed skills")?;

    for row in rows {
        let (skill_id, _version, install_root) = row.context("failed to read skill row")?;
        let active_path = Utf8PathBuf::from(&install_root).join("active");
        if let Ok(pkg) = SkillPackage::load_from_dir(&active_path) {
            let description = pkg.manifest.description.clone()
                .unwrap_or_else(|| format!("Run the {skill_id} skill"));

            tools.push(ToolDefinition {
                name: skill_id,
                description,
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            });
        }
    }
    Ok(tools)
}

fn handle_list(state: &AppState) -> ToolCallResult {
    let conn = match Connection::open(&state.db_path) {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Failed to open database: {e}")),
    };

    let mut stmt = match conn.prepare(
        "SELECT skill_id, active_version, current_status FROM installed_skills ORDER BY skill_id"
    ) {
        Ok(s) => s,
        Err(e) => return ToolCallResult::error(format!("Failed to query skills: {e}")),
    };

    let mut lines = Vec::new();
    let rows = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    }) {
        Ok(r) => r,
        Err(e) => return ToolCallResult::error(format!("Failed to iterate skills: {e}")),
    };

    for row in rows {
        match row {
            Ok((id, version, status)) => {
                lines.push(format!("  {id} v{version} [{status}]"));
            }
            Err(e) => {
                lines.push(format!("  (error reading row: {e})"));
            }
        }
    }

    if lines.is_empty() {
        ToolCallResult::success("No skills installed.")
    } else {
        ToolCallResult::success(format!("Installed skills:\n{}", lines.join("\n")))
    }
}

fn handle_validate(args: &serde_json::Value) -> ToolCallResult {
    let path = match args.get("path").and_then(|p| p.as_str()) {
        Some(p) => p,
        None => return ToolCallResult::error("Missing required parameter: path"),
    };
    let bundle_path = Utf8PathBuf::from(path);
    let report = validate_bundle(&bundle_path);
    ToolCallResult::success(format!("{report:#?}"))
}

fn handle_import(args: &serde_json::Value, state: &AppState) -> ToolCallResult {
    let path = match args.get("path").and_then(|p| p.as_str()) {
        Some(p) => p,
        None => return ToolCallResult::error("Missing required parameter: path"),
    };
    let md_path = Utf8PathBuf::from(path);
    match import_skill_md(&md_path) {
        Ok(scaffolded) => {
            // Load the scaffolded bundle and install it
            let bundle_path = scaffolded.output_dir.clone();
            match SkillPackage::load_from_dir(&bundle_path) {
                Ok(pkg) => match install_unpacked_skill(state, &pkg) {
                    Ok(()) => ToolCallResult::success(
                        format!("Imported and installed skill '{}' from {path}", pkg.manifest.id)
                    ),
                    Err(e) => ToolCallResult::success(
                        format!("Imported to {bundle_path} but install failed: {e}")
                    ),
                },
                Err(e) => ToolCallResult::error(format!("Failed to load imported bundle: {e}")),
            }
        }
        Err(e) => ToolCallResult::error(format!("Import failed: {e}")),
    }
}

fn handle_skill_run(
    skill_id: &str,
    args: &serde_json::Value,
    state: &AppState,
    model_client: Option<&dyn ModelClient>,
) -> ToolCallResult {
    let policy_client = MockPolicyClient::new();
    match run_skill(state, &policy_client, skill_id, args, model_client, None) {
        Ok(result) => {
            let output = result.steps.last()
                .and_then(|s| s.output.as_ref())
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()))
                .unwrap_or_else(|| "Skill completed (no output)".to_string());
            ToolCallResult::success(output)
        }
        Err(e) => ToolCallResult::error(format!("Skill execution failed: {e}")),
    }
}
