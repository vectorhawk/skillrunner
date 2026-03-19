use crate::protocol::{ToolCallResult, ToolDefinition};
use anyhow::Result;
use camino::Utf8PathBuf;
use rusqlite::Connection;
use skillrunner_core::{
    auth::{self, AuthClient},
    executor::run_skill,
    import::import_skill_md,
    install::install_unpacked_skill,
    mcp_governance,
    model::ModelClient,
    policy::PolicyClient,
    registry::RegistryClient,
    state::AppState,
    updater::{install_from_registry, package_skill},
    validator::validate_bundle,
};
use skillrunner_manifest::SkillPackage;
use std::fs;
use tracing::debug;

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

    // Authoring tools (always available)
    tools.push(ToolDefinition {
        name: "skillclub_author".to_string(),
        description: "Create a new SkillClub skill from a name and system prompt. Scaffolds a complete skill bundle directory ready for validation and publishing.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Human-readable name for the skill (e.g., 'Contract Compare')"
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of what the skill does (auto-generated if omitted)"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "The system prompt that defines the skill's behavior"
                },
                "triggers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Trigger phrases that help AI clients decide when to invoke this skill (e.g., ['compare contracts', 'diff legal docs'])"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Directory to create the skill bundle in (default: current directory)"
                }
            },
            "required": ["name", "system_prompt"]
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_validate".to_string(),
        description: "Validate a SkillClub skill bundle directory. Checks manifest, workflow, schemas, and file references.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the skill bundle directory to validate"
                }
            },
            "required": ["path"]
        }),
    });

    // Install tool is always available (supports both local paths and registry)
    tools.push(ToolDefinition {
        name: "skillclub_install".to_string(),
        description: "Install a skill from a local path or from the SkillClub registry by its ID.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "The ID of the skill to install from the registry (use this OR path, not both)"
                },
                "path": {
                    "type": "string",
                    "description": "Local path to a skill bundle directory to install (use this OR skill_id, not both)"
                },
                "version": {
                    "type": "string",
                    "description": "Optional specific version to install from registry (default: latest)"
                }
            },
            "required": []
        }),
    });

    // MCP Governance tools (always available when registry is configured)
    if registry_url.is_some() {
        tools.push(ToolDefinition {
            name: "skillclub_mcp_catalog".to_string(),
            description: "Browse approved MCP servers in your organisation's catalog. Shows available servers with their status, version pins, and credential notes.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        });

        tools.push(ToolDefinition {
            name: "skillclub_mcp_request".to_string(),
            description: "Request access to a new MCP server. In trust mode, the request is auto-approved. In catalog-only mode, known servers are auto-approved. In strict mode, the request goes to IT for review.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "Name of the MCP server to request (e.g., 'Slack MCP')"
                    },
                    "package_source": {
                        "type": "string",
                        "description": "Optional package source (e.g., '@modelcontextprotocol/server-slack')"
                    }
                },
                "required": ["server_name"]
            }),
        });

        tools.push(ToolDefinition {
            name: "skillclub_mcp_status".to_string(),
            description: "Check the status of your MCP server access requests.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        });
    }

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
            name: "skillclub_publish".to_string(),
            description: "Package and publish a skill bundle to the SkillClub registry. Requires authentication.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the skill bundle directory to publish"
                    }
                },
                "required": ["path"]
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

    let base_desc = pkg
        .manifest
        .description
        .clone()
        .unwrap_or_else(|| format!("SkillClub skill: {}", pkg.manifest.name));

    // Enrich description with trigger phrases if present
    let description = if pkg.manifest.triggers.is_empty() {
        base_desc
    } else {
        format!(
            "{}\n\nUse this tool when the user asks to: {}",
            base_desc,
            pkg.manifest.triggers.join(", ")
        )
    };

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
    let result = match name {
        "skillclub_list" => handle_list(state),
        "skillclub_search" => handle_search(arguments, registry_url),
        "skillclub_install" => handle_install(arguments, state, registry_url),
        "skillclub_info" => handle_info(arguments, state),
        "skillclub_author" => handle_author(arguments),
        "skillclub_validate" => handle_validate(arguments),
        "skillclub_publish" => handle_publish(arguments, state, registry_url),
        "skillclub_mcp_catalog" => handle_mcp_catalog(state, registry_url),
        "skillclub_mcp_request" => handle_mcp_request(arguments, state, registry_url),
        "skillclub_mcp_status" => handle_mcp_status(state, registry_url),
        _ => handle_skill_run(name, arguments, state, policy_client, model_client, registry_client),
    };

    // Buffer audit event for tool calls (best-effort, don't fail the call)
    if !name.starts_with("skillclub_list") && !name.starts_with("skillclub_info") {
        let event = mcp_governance::AuditEvent {
            server_name: None,
            user_id: None,
            user_email: None,
            machine_id: None,
            event_type: "tool_called".to_string(),
            tool_name: Some(name.to_string()),
            metadata: None,
            org_id: "default".to_string(),
        };
        let _ = mcp_governance::buffer_audit_event(state, &event);
    }

    result
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
    let path = arguments.get("path").and_then(|v| v.as_str());
    let skill_id = arguments.get("skill_id").and_then(|v| v.as_str());

    match (path, skill_id) {
        // Local path install
        (Some(local_path), _) => {
            let utf8_path = camino::Utf8Path::new(local_path);
            let pkg = match SkillPackage::load_from_dir(utf8_path) {
                Ok(p) => p,
                Err(e) => return ToolCallResult::error(format!("Failed to load skill bundle at {local_path}: {e}")),
            };
            let id = pkg.manifest.id.clone();
            let ver = pkg.manifest.version.to_string();
            match install_unpacked_skill(state, &pkg) {
                Ok(_) => ToolCallResult::success(format!(
                    "Successfully installed {id}@{ver} from local path."
                )),
                Err(e) => ToolCallResult::error(format!("Failed to install {id}: {e}")),
            }
        }
        // Registry install
        (None, Some(id)) => {
            let url = match registry_url {
                Some(u) => u,
                None => return ToolCallResult::error("No registry URL configured. Provide a local 'path' instead."),
            };
            let version = arguments.get("version").and_then(|v| v.as_str());
            let registry = RegistryClient::new(url);
            match install_from_registry(state, &registry, id, version) {
                Ok(installed_ver) => ToolCallResult::success(format!(
                    "Successfully installed {id}@{installed_ver} from registry."
                )),
                Err(e) => ToolCallResult::error(format!("Failed to install {id}: {e}")),
            }
        }
        // Neither provided
        (None, None) => ToolCallResult::error("Provide either 'path' (local install) or 'skill_id' (registry install)"),
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

// ── Authoring tool handlers ──────────────────────────────────────────────────

fn handle_author(arguments: &serde_json::Value) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name"),
    };

    // Description is now optional — auto-generate from name if omitted
    let description = arguments
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("A skill that helps with {}", name.to_lowercase()));

    let system_prompt = match arguments.get("system_prompt").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("Missing required parameter: system_prompt"),
    };

    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    // Optional triggers for tool registration
    let triggers: Vec<String> = arguments
        .get("triggers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Build SKILL.md content with optional fields
    let mut frontmatter = format!("---\nname: {name}\ndescription: {description}\n");
    if !triggers.is_empty() {
        frontmatter.push_str(&format!(
            "triggers:\n{}\n",
            triggers
                .iter()
                .map(|t| format!("  - {t}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    frontmatter.push_str("---\n");

    let skill_md = format!("{frontmatter}\n{system_prompt}\n");

    // Derive skill ID for the subdirectory name
    let skill_id: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    let skill_dir = Utf8PathBuf::from(output_dir).join(&skill_id);

    // Create directory and write SKILL.md
    if let Err(e) = fs::create_dir_all(&skill_dir) {
        return ToolCallResult::error(format!("Failed to create directory {skill_dir}: {e}"));
    }

    let skill_md_path = skill_dir.join("SKILL.md");
    if let Err(e) = fs::write(&skill_md_path, &skill_md) {
        return ToolCallResult::error(format!("Failed to write SKILL.md: {e}"));
    }

    // Import SKILL.md to scaffold the full bundle
    match import_skill_md(&skill_md_path) {
        Ok(bundle) => {
            let files: Vec<&str> = bundle.files.iter().map(|f| f.as_str()).collect();
            let result = serde_json::json!({
                "skill_id": bundle.id,
                "output_dir": bundle.output_dir.to_string(),
                "files": files,
                "message": format!(
                    "Created skill '{}' at {}. You can test it with: skillrunner skill validate {}",
                    bundle.id, bundle.output_dir, bundle.output_dir
                ),
            });
            match serde_json::to_string_pretty(&result) {
                Ok(text) => ToolCallResult::success(text),
                Err(e) => ToolCallResult::error(format!("Failed to serialize result: {e}")),
            }
        }
        Err(e) => ToolCallResult::error(format!("Failed to scaffold skill bundle: {e}")),
    }
}

fn handle_validate(arguments: &serde_json::Value) -> ToolCallResult {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolCallResult::error("Missing required parameter: path"),
    };

    let utf8_path = camino::Utf8Path::new(path);
    let report = validate_bundle(utf8_path);

    let checks: Vec<serde_json::Value> = report
        .checks
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "passed": c.passed,
                "detail": c.detail,
            })
        })
        .collect();

    let result = serde_json::json!({
        "all_passed": report.all_passed(),
        "checks": checks,
    });

    match serde_json::to_string_pretty(&result) {
        Ok(text) => ToolCallResult::success(text),
        Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
    }
}

fn handle_publish(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolCallResult::error("Missing required parameter: path"),
    };

    // Check auth
    let tokens = match auth::load_tokens(state, url) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return ToolCallResult::error(
                "Not logged in. Run `skillrunner auth login` first.",
            )
        }
        Err(e) => return ToolCallResult::error(format!("Failed to load auth tokens: {e}")),
    };

    // Package the skill
    let utf8_path = camino::Utf8Path::new(path);
    let (archive_path, _sha) = match package_skill(utf8_path) {
        Ok(r) => r,
        Err(e) => return ToolCallResult::error(format!("Failed to package skill: {e}")),
    };

    // Publish to registry
    let registry = RegistryClient::new(url).with_auth(&tokens.access_token);
    let result = match registry.publish_skill(&archive_path) {
        Ok(resp) => {
            // Clean up archive
            let _ = fs::remove_file(&archive_path);

            let skill_id = resp.get("skill_id").and_then(|v| v.as_str()).unwrap_or("unknown");
            let version = resp.get("version").and_then(|v| v.as_str()).unwrap_or("unknown");
            format!("Published {skill_id}@{version} to registry successfully.")
        }
        Err(e) => {
            let _ = fs::remove_file(&archive_path);
            return ToolCallResult::error(format!("Failed to publish: {e}"));
        }
    };

    ToolCallResult::success(result)
}

// ── Auth helper with refresh + elicitation fallback ──────────────────────────

/// Attempt to get a valid access token. On failure, returns an elicitation-style
/// error message that prompts the user to authenticate.
///
/// Flow:
/// 1. Load stored tokens
/// 2. If tokens exist, return the access token (caller will handle 401 retry)
/// 3. If no tokens, return an elicitation prompt
fn ensure_auth(state: &AppState, registry_url: &str) -> std::result::Result<String, ToolCallResult> {
    match auth::load_tokens(state, registry_url) {
        Ok(Some(tokens)) => Ok(tokens.access_token),
        Ok(None) => Err(auth_elicitation_prompt(registry_url)),
        Err(e) => Err(ToolCallResult::error(format!("Failed to load auth tokens: {e}"))),
    }
}

/// When a 401 is encountered, attempt a token refresh. If refresh succeeds,
/// save the new tokens and return the new access token. If refresh fails,
/// return an elicitation prompt.
fn try_refresh_auth(
    state: &AppState,
    registry_url: &str,
    refresh_token: &str,
) -> std::result::Result<String, ToolCallResult> {
    debug!("access token expired, attempting refresh");

    let auth_client = AuthClient::new(registry_url);
    match auth_client.refresh(refresh_token) {
        Ok(new_tokens) => {
            // Save refreshed tokens
            if let Err(e) = auth::save_tokens(
                state,
                registry_url,
                &new_tokens.access_token,
                &new_tokens.refresh_token,
            ) {
                debug!("failed to save refreshed tokens: {e}");
            }
            Ok(new_tokens.access_token)
        }
        Err(_) => {
            // Refresh failed — clear stale tokens and prompt re-auth
            let _ = auth::clear_tokens(state, registry_url);
            Err(auth_elicitation_prompt(registry_url))
        }
    }
}

/// Build an elicitation-style prompt that asks the user to authenticate.
/// Includes both the MCP elicitation format (for clients that support it)
/// and a CLI fallback instruction.
fn auth_elicitation_prompt(registry_url: &str) -> ToolCallResult {
    ToolCallResult::error(format!(
        "Authentication required.\n\n\
        To authenticate, please run:\n\
        ```\n\
        skillrunner auth login --registry-url {registry_url}\n\
        ```\n\n\
        After logging in, retry this command."
    ))
}

// ── MCP Governance tool handlers ──────────────────────────────────────────────

fn handle_mcp_catalog(state: &AppState, registry_url: &Option<String>) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    let registry = RegistryClient::new(url);
    match registry.fetch_mcp_servers() {
        Ok(resp) => {
            // Cache for offline use
            let _ = mcp_governance::sync_mcp_config(
                state,
                &registry,
                "skillrunner",
                url,
                true, // dry run — just cache, don't write file
            );

            let formatted: Vec<serde_json::Value> = resp
                .servers
                .iter()
                .filter(|s| s.status == "approved")
                .map(|s| {
                    let mut entry = serde_json::json!({
                        "name": s.name,
                        "package_source": s.package_source,
                        "status": s.status,
                    });
                    if let Some(pin) = &s.version_pin {
                        entry["version_pin"] = serde_json::json!(pin);
                    }
                    if let Some(note) = &s.credential_note {
                        entry["credential_note"] = serde_json::json!(note);
                    }
                    entry
                })
                .collect();

            if formatted.is_empty() {
                ToolCallResult::success(format!(
                    "No approved MCP servers in catalog (approval mode: {}).\nAsk your IT admin to add servers via the SkillClub admin portal.",
                    resp.approval_mode
                ))
            } else {
                let mut output = format!(
                    "Org approval mode: {}\n\nApproved MCP servers ({}):\n",
                    resp.approval_mode,
                    formatted.len()
                );
                match serde_json::to_string_pretty(&formatted) {
                    Ok(text) => {
                        output.push_str(&text);
                        ToolCallResult::success(output)
                    }
                    Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
                }
            }
        }
        Err(e) => ToolCallResult::error(format!("Failed to fetch MCP catalog: {e}")),
    }
}

fn handle_mcp_request(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    let server_name = match arguments.get("server_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: server_name"),
    };

    let package_source = arguments.get("package_source").and_then(|v| v.as_str());

    // Ensure auth with refresh fallback
    let access_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    // Submit request to registry
    let registry = RegistryClient::new(url);
    let result = match registry.submit_mcp_request(server_name, package_source, &access_token) {
        Ok(v) => v,
        Err(e) => {
            // On auth failure, try refresh
            let err_str = e.to_string();
            if err_str.contains("401") || err_str.contains("Unauthorized") {
                let refresh_token = match auth::load_tokens(state, url) {
                    Ok(Some(t)) => t.refresh_token,
                    _ => return auth_elicitation_prompt(url),
                };
                let new_token = match try_refresh_auth(state, url, &refresh_token) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                // Retry with refreshed token
                match registry.submit_mcp_request(server_name, package_source, &new_token) {
                    Ok(v) => v,
                    Err(e) => return ToolCallResult::error(format!("Failed to submit request: {e}")),
                }
            } else {
                return ToolCallResult::error(format!("Failed to submit request: {e}"));
            }
        }
    };

    let req_status = result
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match req_status {
        "approved" => {
            // Auto-approved — trigger immediate sync
            let skillrunner_path = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "skillrunner".to_string());

            let _ = mcp_governance::sync_mcp_config(
                state,
                &registry,
                &skillrunner_path,
                url,
                false,
            );

            ToolCallResult::success(format!(
                "Request for '{}' was auto-approved! The server has been added to your managed MCP config.\n\nPlease restart Claude Code to activate the new MCP server.",
                server_name
            ))
        }
        "pending" => ToolCallResult::success(format!(
            "Request for '{}' has been submitted and is pending IT review.\n\nYour admin will review it in the SkillClub portal. Run `skillclub_mcp_status` to check on it later.",
            server_name
        )),
        _ => ToolCallResult::success(format!(
            "Request submitted with status: {}",
            req_status
        )),
    }
}

fn handle_mcp_status(state: &AppState, registry_url: &Option<String>) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    // Ensure auth with refresh fallback
    let access_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let registry = RegistryClient::new(url);
    let result = match registry.list_mcp_requests(&access_token) {
        Ok(v) => v,
        Err(e) => {
            // On auth failure, try refresh
            let err_str = e.to_string();
            if err_str.contains("401") || err_str.contains("Unauthorized") {
                let refresh_token = match auth::load_tokens(state, url) {
                    Ok(Some(t)) => t.refresh_token,
                    _ => return auth_elicitation_prompt(url),
                };
                let new_token = match try_refresh_auth(state, url, &refresh_token) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                match registry.list_mcp_requests(&new_token) {
                    Ok(v) => v,
                    Err(e) => return ToolCallResult::error(format!("Failed to fetch requests: {e}")),
                }
            } else {
                return ToolCallResult::error(format!("Failed to fetch requests: {e}"));
            }
        }
    };

    let items = result
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if items.is_empty() {
        ToolCallResult::success("No MCP server access requests found.")
    } else {
        let formatted: Vec<serde_json::Value> = items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "server_name": item.get("server_name").and_then(|v| v.as_str()).unwrap_or("?"),
                    "status": item.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                    "admin_notes": item.get("admin_notes").and_then(|v| v.as_str()),
                    "created_at": item.get("created_at").and_then(|v| v.as_str()),
                })
            })
            .collect();

        match serde_json::to_string_pretty(&formatted) {
            Ok(text) => ToolCallResult::success(text),
            Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
        }
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
        assert!(names.contains(&"skillclub_author"));
        assert!(names.contains(&"skillclub_validate"));
        assert!(names.contains(&"skillclub_publish"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_without_registry_omits_registry_tools() {
        let state_root = temp_root("tool-list-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let tools = build_tool_list(&state, &None);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"skillclub_list"));
        assert!(names.contains(&"skillclub_author"));
        assert!(names.contains(&"skillclub_validate"));
        assert!(names.contains(&"skillclub_install")); // install always available (supports local paths)
        assert!(!names.contains(&"skillclub_search"));
        assert!(!names.contains(&"skillclub_publish"));

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
    fn handle_install_requires_path_or_skill_id() {
        let state_root = temp_root("handle-install-no-id");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_install(
            &serde_json::json!({}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("path") || result.content[0].text.contains("skill_id"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_install_local_path() {
        let state_root = temp_root("handle-install-local");
        let skill_root = temp_root("handle-install-local-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);

        let result = handle_install(
            &serde_json::json!({"path": skill_root.as_str()}),
            &state,
            &None, // no registry needed for local install
        );
        assert!(result.is_error.is_none(), "got: {:?}", result.content[0].text);
        assert!(result.content[0].text.contains("test-skill"));
        assert!(result.content[0].text.contains("0.1.0"));

        // Verify the skill appears in the list
        let list_result = handle_list(&state);
        assert!(list_result.content[0].text.contains("test-skill"));

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_install_registry_requires_url() {
        let state_root = temp_root("handle-install-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_install(
            &serde_json::json!({"skill_id": "some-skill"}),
            &state,
            &None,
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));

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

    // ── Authoring tool tests ──────────────────────────────────────────────

    #[test]
    fn handle_author_creates_skill_bundle() {
        let out_dir = temp_root("author-ok");
        fs::create_dir_all(&out_dir).unwrap();

        let result = handle_author(&serde_json::json!({
            "name": "My Test Skill",
            "description": "Does something useful",
            "system_prompt": "You are a helpful assistant.",
            "output_dir": out_dir.as_str(),
        }));

        assert!(result.is_error.is_none(), "got: {:?}", result.content[0].text);
        let text = &result.content[0].text;
        assert!(text.contains("my-test-skill"), "got: {text}");

        // Verify bundle files were created
        let skill_dir = out_dir.join("my-test-skill");
        assert!(skill_dir.join("manifest.json").exists());
        assert!(skill_dir.join("workflow.yaml").exists());
        assert!(skill_dir.join("prompts/system.txt").exists());
        assert!(skill_dir.join("schemas/input.schema.json").exists());

        // Verify system prompt content
        let prompt = fs::read_to_string(skill_dir.join("prompts/system.txt")).unwrap();
        assert!(prompt.contains("You are a helpful assistant."));

        let _ = fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn handle_author_requires_name() {
        let result = handle_author(&serde_json::json!({
            "description": "test",
            "system_prompt": "test",
        }));
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("name"));
    }

    #[test]
    fn handle_author_requires_system_prompt() {
        let result = handle_author(&serde_json::json!({
            "name": "test",
            "description": "test",
        }));
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("system_prompt"));
    }

    #[test]
    fn handle_validate_passes_valid_bundle() {
        let skill_root = temp_root("validate-ok");
        write_test_skill(&skill_root);

        let result = handle_validate(&serde_json::json!({
            "path": skill_root.as_str(),
        }));
        assert!(result.is_error.is_none());
        let text = &result.content[0].text;
        assert!(text.contains("\"all_passed\": true"), "got: {text}");

        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_validate_fails_invalid_bundle() {
        let skill_root = temp_root("validate-bad");
        fs::create_dir_all(&skill_root).unwrap();
        // Empty directory — no manifest.json
        fs::write(skill_root.join("something.txt"), "not a skill").unwrap();

        let result = handle_validate(&serde_json::json!({
            "path": skill_root.as_str(),
        }));
        assert!(result.is_error.is_none()); // Returns validation report, not error
        let text = &result.content[0].text;
        assert!(text.contains("\"all_passed\": false"), "got: {text}");

        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_validate_requires_path() {
        let result = handle_validate(&serde_json::json!({}));
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("path"));
    }

    #[test]
    fn handle_publish_requires_registry() {
        let state_root = temp_root("publish-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_publish(
            &serde_json::json!({"path": "/tmp/fake"}),
            &state,
            &None,
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_publish_requires_path() {
        let state_root = temp_root("publish-no-path");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_publish(
            &serde_json::json!({}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("path"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_publish_requires_auth() {
        let state_root = temp_root("publish-no-auth");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_publish(
            &serde_json::json!({"path": "/tmp/fake"}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("Not logged in"));

        let _ = fs::remove_dir_all(&state_root);
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
