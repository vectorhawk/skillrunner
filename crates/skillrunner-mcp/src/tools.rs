use crate::protocol::{ToolCallResult, ToolDefinition};
use anyhow::Result;
use camino::Utf8PathBuf;
use rusqlite::Connection;
use semver::Version;
use skillrunner_core::{
    auth::{self, AuthClient},
    executor::{run_skill, RunResult},
    import::import_local_skill_md,
    install::{install_unpacked_skill, uninstall_skill, InstallMode},
    mcp_governance,
    model::{ModelClient, ModelSource},
    policy::PolicyClient,
    registry::RegistryClient,
    state::AppState,
    updater::{
        check_for_update, install_from_registry, install_plugin_from_registry, package_plugin,
        tar_gz_skill_source,
    },
    validator::validate_bundle,
};
use skillrunner_manifest::SkillPackage;
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::debug;

/// How long a cached update-check result is considered fresh before a re-check.
const UPDATE_CHECK_TTL: Duration = Duration::from_secs(600); // 10 minutes

/// A single cached result from a registry update check for one skill.
pub struct UpdateCheckEntry {
    /// When this entry was populated.
    pub checked_at: Instant,
    /// The latest version from the registry, if it is newer than installed.
    /// `None` means the skill is already up-to-date, or the check failed.
    pub latest_version: Option<Version>,
}

/// Shared update-check cache passed from `ServerState` to the tools layer.
///
/// Using `Arc<Mutex<…>>` so `ServerState` can hold it and hand a reference
/// to `handle_tool_call` without moving or cloning the whole map.
pub type UpdateCheckCache = Arc<Mutex<HashMap<String, UpdateCheckEntry>>>;

const GOVERNANCE_FOOTER: &str = "\n\n---\nTo add new MCP servers, use /mcp-request. To add plugins, use /plugin-install. Direct installation via /mcp bypasses governance.";

fn is_managed(state: &AppState) -> bool {
    skillrunner_core::managed::load_managed_config(state).is_some()
}

// ── Tool registry ────────────────────────────────────────────────────────────

/// Builds the list of MCP tool definitions from installed skills + management tools.
pub fn build_tool_list(state: &AppState, registry_url: &Option<String>) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    // Check if user has auth tokens (logged in to registry)
    let logged_in = registry_url
        .as_ref()
        .and_then(|url| {
            skillrunner_core::auth::load_tokens(state, url)
                .ok()
                .flatten()
        })
        .is_some();

    // Add installed skills as tools
    if let Ok(skill_tools) = skill_tools_from_db(state) {
        tools.extend(skill_tools);
    }

    // Add management tools (always available — these work locally)
    tools.push(ToolDefinition {
        name: "skillclub_list".to_string(),
        description: "List all installed skills available to the user. Use this when the user asks 'what skills do I have', 'what tools are available', or 'what can you do'. Shows skill IDs, versions, and descriptions."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_uninstall".to_string(),
        description: "Uninstall an installed SkillClub skill by its ID. Removes the skill files and database records.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "The ID of the installed skill to uninstall"
                }
            },
            "required": ["skill_id"]
        }),
    });

    // Authoring tools (always available)
    tools.push(ToolDefinition {
        name: "skillclub_author".to_string(),
        description: "Create a new SkillClub skill from a name and system prompt. Analyzes the prompt and returns smart recommendations for triggers, permissions, model, and execution settings. Use skillclub_author_confirm to finalize.".to_string(),
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
                "mode": {
                    "type": "string",
                    "enum": ["interactive", "accept_suggestions", "skip_metadata"],
                    "description": "Authoring mode. 'interactive' (default): returns recommendations for review. 'accept_suggestions': auto-accepts all recommendations and scaffolds immediately. 'skip_metadata': scaffolds with bare defaults, no analysis."
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
        name: "skillclub_author_confirm".to_string(),
        description: "Finalize skill creation after reviewing recommendations from skillclub_author. Scaffolds the skill bundle with the confirmed metadata values.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name (same as passed to skillclub_author)"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "System prompt (same as passed to skillclub_author)"
                },
                "description": {
                    "type": "string",
                    "description": "Skill description"
                },
                "vh_triggers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Confirmed trigger phrases"
                },
                "vh_permissions": {
                    "type": "object",
                    "properties": {
                        "network": { "type": "string" },
                        "filesystem": { "type": "string" },
                        "clipboard": { "type": "string" }
                    },
                    "description": "Confirmed permission settings"
                },
                "vh_model": {
                    "type": "object",
                    "properties": {
                        "min_params_b": { "type": "number" },
                        "recommended": { "type": "array", "items": { "type": "string" } },
                        "fallback": { "type": "string" }
                    },
                    "description": "Confirmed model settings"
                },
                "vh_execution": {
                    "type": "object",
                    "properties": {
                        "timeout_ms": { "type": "integer" },
                        "memory_mb": { "type": "integer" },
                        "sandbox": { "type": "string" }
                    },
                    "description": "Confirmed execution settings"
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

    // MCP Governance tools (only shown when logged in to registry)
    if logged_in {
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

        tools.push(ToolDefinition {
            name: "skillclub_mcp_install".to_string(),
            description: "Activate an approved MCP server through SkillRunner's governance system. \
                This forces an immediate sync with the registry and makes the server's tools \
                available right away. The server must already be approved via skillclub_mcp_request."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "Name of the approved MCP server to activate"
                    }
                },
                "required": ["server_name"]
            }),
        });

        tools.push(ToolDefinition {
            name: "skillclub_mcp_uninstall".to_string(),
            description: "Remove a governed MCP server from SkillRunner. \
                This deactivates the server and removes its tools immediately."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server_name": {
                        "type": "string",
                        "description": "Name of the MCP server to deactivate"
                    }
                },
                "required": ["server_name"]
            }),
        });
    }

    // Login is always available when a registry URL exists (so users can log in)
    if registry_url.is_some() && !logged_in {
        tools.push(ToolDefinition {
            name: "skillclub_login".to_string(),
            description: "Log in to the VectorHawk registry to unlock publishing, searching, and governance features.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "email": {
                        "type": "string",
                        "description": "Your VectorHawk account email address"
                    },
                    "password": {
                        "type": "string",
                        "description": "Your VectorHawk account password"
                    },
                    "registry_url": {
                        "type": "string",
                        "description": "Optional registry URL override (defaults to the server's configured registry URL)"
                    }
                },
                "required": ["email", "password"]
            }),
        });
    }

    // Registry tools that require authentication
    if logged_in {
        tools.push(ToolDefinition {
            name: "skillclub_logout".to_string(),
            description: "Log out of the VectorHawk registry. Clears stored authentication tokens."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        });

        tools.push(ToolDefinition {
            name: "skillclub_search".to_string(),
            description: "Search the SkillClub skill registry for skills that can be installed. Use this when the user asks 'what skills are available', 'find skills for X', or wants to discover new capabilities. Use an empty query to list all available skills.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query to find skills (e.g., 'contract', 'analysis'). Omit or leave empty to list all available skills."
                    }
                },
                "required": []
            }),
        });

        tools.push(ToolDefinition {
            name: "skillclub_publish".to_string(),
            description: "Package and publish a skill bundle to the SkillClub registry. Requires authentication. If the version already exists, bump the version in manifest.json and retry.".to_string(),
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
    } // end logged_in block for search/publish/logout

    // Update tool — available whenever a registry URL is configured.
    if registry_url.is_some() {
        tools.push(ToolDefinition {
            name: "skillclub_update".to_string(),
            description: "Update an installed skill to the latest version from the registry. \
                Call this after skillclub_update notifies you that a newer version is available."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_id": {
                        "type": "string",
                        "description": "The skill ID to update"
                    }
                },
                "required": ["skill_id"]
            }),
        });
    }

    // Info and import are always available (local operations)
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

    tools.push(ToolDefinition {
        name: "skillclub_import".to_string(),
            description: "Import an external skill or MCP server into SkillClub. Paste an npm package name (e.g. @modelcontextprotocol/server-github), npx command, or GitHub URL. The system detects whether it's a skill or MCP server and routes to the appropriate approval workflow.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "The npm package name, npx command, or GitHub URL to import"
                    },
                    "confirm": {
                        "type": "boolean",
                        "description": "If true, submit the import after preview. If false (default), only preview."
                    }
                },
                "required": ["input"]
            }),
        });

    // Plugin tools that require registry auth
    if logged_in {
        tools.push(ToolDefinition {
            name: "skillclub_plugin_search".to_string(),
            description: "Search the SkillClub registry for plugins. Plugins are composite bundles that include skills, MCP servers, and slash commands packaged together for a complete workflow.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query (empty to list all plugins)"
                    }
                },
                "required": []
            }),
        });
    }

    // Plugin tools (always available — local operations)
    tools.push(ToolDefinition {
        name: "skillclub_plugin_info".to_string(),
        description: "Get detailed information about a plugin including its skills, MCP servers, commands, and configuration requirements.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "plugin_id": {
                    "type": "string",
                    "description": "The ID of the plugin to get info about"
                }
            },
            "required": ["plugin_id"]
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_plugin_install".to_string(),
        description: "Install a plugin from a local directory or the registry.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path_or_id": {
                    "type": "string",
                    "description": "Local directory path to a plugin bundle, or a registry plugin ID"
                }
            },
            "required": ["path_or_id"]
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_plugin_uninstall".to_string(),
        description: "Uninstall a plugin. Removes all its skills, disconnects MCP servers, and deletes slash commands.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "plugin_id": {
                    "type": "string",
                    "description": "The ID of the installed plugin to uninstall"
                }
            },
            "required": ["plugin_id"]
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_plugin_import".to_string(),
        description: "Import a Claude Code plugin or .mcpb Desktop Extension into SkillClub plugin format. Auto-detects the external format and converts it.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the Claude Code plugin directory or .mcpb file"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Output directory for the converted plugin (default: current directory)"
                }
            },
            "required": ["path"]
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_plugin_list".to_string(),
        description: "List all installed plugins with their status and component breakdown (skills, MCP servers, commands).".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    });

    // Rating tool — always available once any skill is installed
    tools.push(ToolDefinition {
        name: "skillclub_rate".to_string(),
        description: "Rate a skill after using it. Call this when the user gives a thumbs up or thumbs down on a skill result. Records the rating locally and syncs it to the registry.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "skill_id": {
                    "type": "string",
                    "description": "The skill ID to rate"
                },
                "rating": {
                    "type": "string",
                    "enum": ["up", "down"],
                    "description": "thumbs up or thumbs down"
                }
            },
            "required": ["skill_id", "rating"]
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_plugin_export".to_string(),
        description: "Export a SkillClub plugin to Claude Code plugin or .mcpb Desktop Extension \
            format for distribution to non-SkillClub channels."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the SkillClub plugin directory"
                },
                "format": {
                    "type": "string",
                    "enum": ["claude-code", "mcpb"],
                    "description": "Export format: 'claude-code' for a Claude Code plugin directory, 'mcpb' for a Desktop Extension archive"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Output directory where the exported artifact will be written (default: current directory)"
                }
            },
            "required": ["path", "format"]
        }),
    });

    tools.push(ToolDefinition {
        name: "skillclub_plugin_author".to_string(),
        description: "Create a new SkillClub plugin scaffold with plugin.json and directory structure.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Human-readable name for the plugin (e.g., 'My Workflow Plugin')"
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of what the plugin does"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Directory to create the plugin bundle in (default: current directory)"
                }
            },
            "required": ["name"]
        }),
    });

    if logged_in {
        tools.push(ToolDefinition {
            name: "skillclub_plugin_publish".to_string(),
            description: "Package and publish a plugin bundle to the SkillClub registry. Requires authentication.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the plugin bundle directory to publish"
                    }
                },
                "required": ["path"]
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

    // Append version to description so updates are visible in the tool listing.
    let versioned_desc = format!("{} (v{})", base_desc, pkg.manifest.version);

    // Enrich description with trigger phrases when the author declared any.
    // AUTH2a: trigger phrases are now declared via vh_triggers in SKILL.md
    // frontmatter. They are lowercased and deduplicated at load time.
    // Skills without vh_triggers have an empty list and fall through unmodified.
    let triggers = pkg.manifest.triggers.clone();

    let description = if triggers.is_empty() {
        versioned_desc
    } else {
        format!(
            "{}\n\nUse this tool when the user asks to: {}",
            versioned_desc,
            triggers.join(", ")
        )
    };

    // Use the in-memory input schema (already parsed at load time).
    let input_schema = pkg.manifest.inputs_schema_or_default();

    Ok(ToolDefinition {
        name: skill_id.to_string(),
        description,
        input_schema,
    })
}

// ── Update check helpers ─────────────────────────────────────────────────────

/// Consult the update-check cache for `skill_id`.
///
/// Returns `Some(ToolCallResult)` (an error-style prompt asking the user to
/// update or skip) when a newer version is available in the registry.
/// Returns `None` when the skill is up-to-date, the check failed, or no
/// registry client is configured — in all of these cases the caller should
/// proceed with execution as normal.
fn maybe_build_update_prompt(
    skill_id: &str,
    state: &AppState,
    registry_client: Option<&RegistryClient>,
    cache: &UpdateCheckCache,
) -> Option<ToolCallResult> {
    let registry = registry_client?;

    // Resolve latest version: use a cached result if it is still fresh.
    let latest_version = {
        let cached = cache.lock().ok().and_then(|guard| {
            guard.get(skill_id).and_then(|entry| {
                if entry.checked_at.elapsed() < UPDATE_CHECK_TTL {
                    Some(entry.latest_version.clone())
                } else {
                    None // stale
                }
            })
        });

        if let Some(from_cache) = cached {
            from_cache
        } else {
            // Cache miss or stale — query the registry (best-effort).
            let check_result = check_for_update(state, registry, skill_id);

            // If the registry call succeeded, we have fresh network access.
            // Invalidate the cached policy so the next resolve_skill call
            // re-fetches the live policy state — which will now show "blocked"
            // if the skill has been unpublished.
            if check_result.is_ok() {
                if let Ok(conn) = Connection::open(&state.db_path) {
                    let _ = conn.execute(
                        "DELETE FROM policy_cache WHERE skill_id = ?1",
                        rusqlite::params![skill_id],
                    );
                }
            }

            let result = check_result.unwrap_or(None);

            // Store the result so the next call within the TTL skips the
            // network round-trip regardless of the outcome.
            if let Ok(mut guard) = cache.lock() {
                guard.insert(
                    skill_id.to_string(),
                    UpdateCheckEntry {
                        checked_at: Instant::now(),
                        latest_version: result.clone(),
                    },
                );
            }

            result
        }
    };

    let latest = latest_version?;

    // There is a newer version — look up the installed version string for the
    // human-readable prompt.  If we cannot read it, skip the prompt rather
    // than blocking execution.
    let installed_str = {
        let conn = Connection::open(&state.db_path).ok()?;
        conn.query_row(
            "SELECT active_version FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| row.get::<_, String>(0),
        )
        .ok()?
    };

    let message = format!(
        "Update available: {skill_id} v{installed_str} \u{2192} v{latest}\n\n\
         Would you like to update before running? You have two options:\n\
         1. Call skillclub_update(skill_id=\"{skill_id}\") to install v{latest}, then retry.\n\
         2. Call this skill again with skip_update_check=true to run v{installed_str}."
    );

    Some(ToolCallResult::error(message))
}

// ── Tool dispatch ────────────────────────────────────────────────────────────

/// Execute a tool call and return the MCP result.
#[allow(clippy::too_many_arguments)]
pub fn handle_tool_call(
    name: &str,
    arguments: &serde_json::Value,
    state: &AppState,
    policy_client: &dyn PolicyClient,
    model_client: Option<&dyn ModelClient>,
    registry_client: Option<&RegistryClient>,
    registry_url: &Option<String>,
    update_check_cache: &UpdateCheckCache,
) -> ToolCallResult {
    let result = match name {
        "skillclub_list" => handle_list(state, registry_url, update_check_cache),
        "skillclub_search" => handle_search(arguments, registry_url),
        "skillclub_install" => handle_install(arguments, state, registry_url),
        "skillclub_uninstall" => handle_uninstall(arguments, state),
        "skillclub_info" => handle_info(arguments, state),
        "skillclub_author" => handle_author(arguments),
        "skillclub_author_confirm" => handle_author_confirm(arguments),
        "skillclub_validate" => handle_validate(arguments),
        "skillclub_publish" => handle_publish(arguments, state, registry_url),
        "skillclub_login" => handle_login(arguments, state, registry_url),
        "skillclub_logout" => handle_logout(state, registry_url),
        "skillclub_mcp_catalog" => handle_mcp_catalog(state, registry_url),
        "skillclub_mcp_request" => handle_mcp_request(arguments, state, registry_url),
        "skillclub_mcp_status" => handle_mcp_status(state, registry_url),
        "skillclub_import" => handle_import(arguments, state, registry_url),
        "skillclub_plugin_search" => handle_plugin_search(arguments, registry_url),
        "skillclub_plugin_info" => handle_plugin_info(arguments, state),
        "skillclub_plugin_install" => handle_plugin_install(arguments, state, registry_url),
        "skillclub_plugin_uninstall" => handle_plugin_uninstall(arguments, state),
        "skillclub_plugin_list" => handle_plugin_list(state),
        "skillclub_plugin_author" => handle_plugin_author(arguments),
        "skillclub_plugin_publish" => handle_plugin_publish(arguments, state, registry_url),
        "skillclub_plugin_export" => handle_plugin_export(arguments),
        "skillclub_plugin_import" => handle_plugin_import(arguments),
        "skillclub_rate" => handle_rate(arguments, state),
        "skillclub_update" => handle_update(arguments, state, registry_url),
        _ => handle_skill_run(
            name,
            arguments,
            state,
            policy_client,
            model_client,
            registry_client,
            update_check_cache,
        ),
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

fn handle_list(
    state: &AppState,
    registry_url: &Option<String>,
    update_check_cache: &UpdateCheckCache,
) -> ToolCallResult {
    // Force a fresh sync with the registry on every list call. This picks up
    // lifecycle changes (unpublish → deactivate, reactivation) and version
    // upgrades. Best-effort: if the registry is unreachable, we fall through
    // to the local DB view.
    let mut sync_msg: Option<String> = None;
    if let Some(url) = registry_url.as_ref() {
        let registry = skillrunner_core::registry::RegistryClient::new(url);
        let policy_client = skillrunner_core::registry::HttpPolicyClient::new(
            skillrunner_core::registry::RegistryClient::new(url),
            state,
        );
        match skillrunner_core::updater::check_skill_updates(state, &registry, &policy_client) {
            Ok(0) => {}
            Ok(n) => {
                sync_msg = Some(format!(
                    "🔄 Synced with registry: {n} skill(s) updated.\n\n"
                ));
                // Any in-memory update-check results may now be stale (we just
                // pulled changes). Clear the whole cache — next skill call
                // repopulates on demand.
                if let Ok(mut cache) = update_check_cache.lock() {
                    cache.clear();
                }
            }
            Err(_) => {
                // Registry unreachable — proceed with local view.
            }
        }
    }

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

    let footer = if is_managed(state) {
        GOVERNANCE_FOOTER
    } else {
        ""
    };

    if skills.is_empty() {
        // Check if user is logged in to give appropriate next steps
        let logged_in = registry_url
            .as_ref()
            .and_then(|url| {
                skillrunner_core::auth::load_tokens(state, url)
                    .ok()
                    .flatten()
            })
            .is_some();

        let next_steps = if logged_in {
            "\n\nTo get started, you can:\n\
             - Use skillclub_search to browse the registry\n\
             - Use skillclub_author to create a new skill from a prompt\n\
             - Use skillclub_import to import an existing SKILL.md file"
        } else {
            "\n\nTo get started, you can:\n\
             - Use skillclub_author to create a new skill from a prompt\n\
             - Use skillclub_import to import an existing SKILL.md file\n\
             - Use skillclub_login to sign in and access the skill registry"
        };

        let prefix = sync_msg.unwrap_or_default();
        ToolCallResult::success(format!("{prefix}No skills installed.{next_steps}{footer}"))
    } else {
        match serde_json::to_string_pretty(&skills) {
            Ok(text) => {
                let prefix = sync_msg.unwrap_or_default();
                ToolCallResult::success(format!("{prefix}{text}{footer}"))
            }
            Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
        }
    }
}

fn handle_search(arguments: &serde_json::Value, registry_url: &Option<String>) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    let query = arguments
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("");

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
                Err(e) => {
                    return ToolCallResult::error(format!(
                        "Failed to load skill bundle at {local_path}: {e}"
                    ))
                }
            };
            let id = pkg.manifest.id.clone();
            let ver = pkg.manifest.version.to_string();
            match install_unpacked_skill(state, &pkg, InstallMode::Copy) {
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
                None => {
                    return ToolCallResult::error(
                        "No registry URL configured. Provide a local 'path' instead.",
                    )
                }
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
        (None, None) => ToolCallResult::error(
            "Provide either 'path' (local install) or 'skill_id' (registry install)",
        ),
    }
}

fn handle_uninstall(arguments: &serde_json::Value, state: &AppState) -> ToolCallResult {
    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return ToolCallResult::error("Missing required parameter: skill_id"),
    };

    match uninstall_skill(state, skill_id) {
        Ok(Some(version)) => {
            ToolCallResult::success(format!("Successfully uninstalled {skill_id}@{version}."))
        }
        Ok(None) => ToolCallResult::error(format!("Skill '{skill_id}' is not installed.")),
        Err(e) => ToolCallResult::error(format!("Failed to uninstall '{skill_id}': {e}")),
    }
}

fn handle_update(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return ToolCallResult::error("Missing required parameter: skill_id"),
    };

    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry configured — cannot update skills"),
    };

    let registry = RegistryClient::new(url);
    match install_from_registry(state, &registry, skill_id, None) {
        Ok(version) => ToolCallResult::success(format!("Updated {skill_id} to v{version}")),
        Err(e) => ToolCallResult::error(format!("Update failed: {e}")),
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
                    "min_params_b": r.min_params_b,
                    "recommended": r.recommended,
                    "fallback": r.fallback,
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

    let description = arguments
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("A skill that helps with {}", name.to_lowercase()));

    let system_prompt = match arguments.get("system_prompt").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("Missing required parameter: system_prompt"),
    };

    let mode = arguments
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("interactive");

    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    match mode {
        "skip_metadata" => {
            // Scaffold immediately with bare defaults, no analysis
            scaffold_skill_bundle(name, &description, system_prompt, output_dir, None)
        }
        "accept_suggestions" => {
            // Run recommendation engine, scaffold immediately with recommendations
            let rec = skillrunner_core::recommend::recommend_from_prompt(
                name,
                &description,
                system_prompt,
            );
            scaffold_skill_bundle(name, &description, system_prompt, output_dir, Some(&rec))
        }
        _ => {
            // "interactive" (default): run recommendation engine, return for review
            let rec = skillrunner_core::recommend::recommend_from_prompt(
                name,
                &description,
                system_prompt,
            );
            let result = serde_json::json!({
                "status": "recommendations_ready",
                "skill_id": skillrunner_manifest::to_skill_id(name),
                "recommendations": {
                    "vh_triggers": rec.triggers,
                    "vh_permissions": {
                        "network": rec.permissions.network,
                        "filesystem": rec.permissions.filesystem,
                        "clipboard": rec.permissions.clipboard,
                    },
                    "vh_model": {
                        "min_params_b": rec.model.min_params_b,
                        "recommended": rec.model.recommended,
                        "fallback": rec.model.fallback,
                    },
                    "vh_execution": {
                        "timeout_ms": rec.execution.timeout_ms,
                        "memory_mb": rec.execution.memory_mb,
                        "sandbox": rec.execution.sandbox,
                    },
                },
                "confidence": format!("{:?}", rec.confidence).to_lowercase(),
                "message": format!(
                    "I've analyzed your skill '{}' and generated metadata recommendations. \
                     Review the suggestions above and tell me what to change, or say 'looks good' \
                     to finalize. Call skillclub_author_confirm with the final values to create the skill.",
                    name
                ),
            });
            match serde_json::to_string_pretty(&result) {
                Ok(text) => ToolCallResult::success(text),
                Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
            }
        }
    }
}

fn handle_author_confirm(arguments: &serde_json::Value) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name"),
    };

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

    let rec = build_recommendations_from_args(arguments);

    scaffold_skill_bundle(name, &description, system_prompt, output_dir, Some(&rec))
}

fn build_recommendations_from_args(
    args: &serde_json::Value,
) -> skillrunner_core::recommend::Recommendations {
    use skillrunner_core::recommend::{
        RecommendationConfidence, Recommendations, RecommendedExecution, RecommendedModel,
        RecommendedPermissions,
    };

    let triggers = args
        .get("vh_triggers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let perms_obj = args.get("vh_permissions");
    let permissions = RecommendedPermissions {
        network: perms_obj
            .and_then(|o| o.get("network"))
            .and_then(|v| v.as_str())
            .unwrap_or("none")
            .to_string(),
        filesystem: perms_obj
            .and_then(|o| o.get("filesystem"))
            .and_then(|v| v.as_str())
            .unwrap_or("none")
            .to_string(),
        clipboard: perms_obj
            .and_then(|o| o.get("clipboard"))
            .and_then(|v| v.as_str())
            .unwrap_or("none")
            .to_string(),
    };

    let model_obj = args.get("vh_model");
    let model = RecommendedModel {
        min_params_b: model_obj
            .and_then(|o| o.get("min_params_b"))
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0),
        recommended: model_obj
            .and_then(|o| o.get("recommended"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_else(|| vec!["gemma3:4b".to_string()]),
        fallback: model_obj
            .and_then(|o| o.get("fallback"))
            .and_then(|v| v.as_str())
            .unwrap_or("error")
            .to_string(),
    };

    let exec_obj = args.get("vh_execution");
    let execution = RecommendedExecution {
        timeout_ms: exec_obj
            .and_then(|o| o.get("timeout_ms"))
            .and_then(|v| v.as_u64())
            .unwrap_or(30000),
        memory_mb: exec_obj
            .and_then(|o| o.get("memory_mb"))
            .and_then(|v| v.as_u64())
            .unwrap_or(256),
        sandbox: exec_obj
            .and_then(|o| o.get("sandbox"))
            .and_then(|v| v.as_str())
            .unwrap_or("strict")
            .to_string(),
    };

    Recommendations {
        triggers,
        permissions,
        model,
        execution,
        // Confidence is irrelevant for confirmed values
        confidence: RecommendationConfidence::High,
    }
}

fn scaffold_skill_bundle(
    name: &str,
    description: &str,
    system_prompt: &str,
    output_dir: &str,
    recommendations: Option<&skillrunner_core::recommend::Recommendations>,
) -> ToolCallResult {
    let out = Utf8PathBuf::from(output_dir);
    if let Err(e) = fs::create_dir_all(&out) {
        return ToolCallResult::error(format!("Failed to create directory {out}: {e}"));
    }

    // Write a minimal SKILL.md so import_local_skill_md can derive the skill ID
    // and scaffold the bundle directory structure (workflow, schemas, prompts).
    let minimal_md = format!(
        "---\nname: {name}\ndescription: {description}\nlicense: Apache-2.0\n---\n\n{system_prompt}\n"
    );
    let skill_md_path = out.join("SKILL.md");
    if let Err(e) = fs::write(&skill_md_path, &minimal_md) {
        return ToolCallResult::error(format!("Failed to write SKILL.md: {e}"));
    }

    let bundle = match import_local_skill_md(&skill_md_path) {
        Ok(b) => b,
        Err(e) => return ToolCallResult::error(format!("Failed to scaffold skill bundle: {e}")),
    };

    // When recommendations are provided, overwrite the bundle's SKILL.md with
    // the full-metadata version (import_local_skill_md generates defaults; we need
    // to inject the vh_* fields the user confirmed or we inferred).
    if let Some(rec) = recommendations {
        let mut fm =
            format!("---\nname: {name}\ndescription: {description}\nlicense: Apache-2.0\n");

        if !rec.triggers.is_empty() {
            fm.push_str("vh_triggers:\n");
            for t in &rec.triggers {
                fm.push_str(&format!("  - \"{t}\"\n"));
            }
        }
        fm.push_str(&format!(
            "vh_permissions:\n  network: {}\n  filesystem: {}\n  clipboard: {}\n",
            rec.permissions.network, rec.permissions.filesystem, rec.permissions.clipboard
        ));
        fm.push_str(&format!(
            "vh_model:\n  min_params_b: {}\n  recommended:\n",
            rec.model.min_params_b
        ));
        for m in &rec.model.recommended {
            fm.push_str(&format!("    - \"{m}\"\n"));
        }
        fm.push_str(&format!("  fallback: {}\n", rec.model.fallback));
        fm.push_str(&format!(
            "vh_execution:\n  timeout_ms: {}\n  memory_mb: {}\n  sandbox: {}\n",
            rec.execution.timeout_ms, rec.execution.memory_mb, rec.execution.sandbox
        ));
        fm.push_str("vh_workflow_ref: workflow.yaml\n---\n");
        let full_md = format!("{fm}\n{system_prompt}\n");

        let bundle_skill_md = bundle.output_dir.join("SKILL.md");
        if let Err(e) = fs::write(&bundle_skill_md, &full_md) {
            return ToolCallResult::error(format!(
                "Failed to write metadata SKILL.md to bundle: {e}"
            ));
        }
    }

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
        Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
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
            return ToolCallResult::error("Not logged in. Run `skillrunner auth login` first.")
        }
        Err(e) => return ToolCallResult::error(format!("Failed to load auth tokens: {e}")),
    };

    // Archive the source tree for the compile endpoint (registry validates).
    let utf8_path = camino::Utf8Path::new(path);
    let source_bytes = match tar_gz_skill_source(utf8_path) {
        Ok(b) => b,
        Err(e) => return ToolCallResult::error(format!("Failed to archive skill source: {e}")),
    };

    // Publish via the registry compile endpoint.
    let registry = RegistryClient::new(url).with_auth(&tokens.access_token);
    let result = match registry.compile_and_publish(source_bytes) {
        Ok(resp) => {
            let name = &resp.frontmatter.name;
            let version = resp.frontmatter.vh_version.as_deref().unwrap_or("unknown");
            let mut msg = format!("Published '{name}' v{version} to registry successfully.");
            for w in &resp.warnings {
                msg.push_str(&format!("\nWarning: {w}"));
            }
            msg
        }
        Err(e) => {
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
fn ensure_auth(
    state: &AppState,
    registry_url: &str,
) -> std::result::Result<String, ToolCallResult> {
    match auth::load_tokens(state, registry_url) {
        Ok(Some(tokens)) => Ok(tokens.access_token),
        Ok(None) => Err(auth_elicitation_prompt(registry_url)),
        Err(e) => Err(ToolCallResult::error(format!(
            "Failed to load auth tokens: {e}"
        ))),
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
/// Directs the user to the `skillclub_login` MCP tool, which can be used
/// directly without leaving the AI client.
fn auth_elicitation_prompt(registry_url: &str) -> ToolCallResult {
    ToolCallResult::error(format!(
        "Authentication required.\n\n\
        Use the `skillclub_login` tool to log in:\n\
        - email: your SkillClub email\n\
        - password: your SkillClub password\n\
        - registry_url: {registry_url} (pre-filled)\n\n\
        After logging in, retry this command."
    ))
}

// ── Auth tool handlers ────────────────────────────────────────────────────────

fn handle_login(
    arguments: &serde_json::Value,
    state: &AppState,
    server_registry_url: &Option<String>,
) -> ToolCallResult {
    let email = match arguments.get("email").and_then(|v| v.as_str()) {
        Some(e) => e,
        None => return ToolCallResult::error("Missing required parameter: email"),
    };

    let password = match arguments.get("password").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolCallResult::error("Missing required parameter: password"),
    };

    // registry_url from arguments takes precedence; fall back to server's configured URL.
    let registry_url_arg = arguments
        .get("registry_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let registry_url = match registry_url_arg.as_ref().or(server_registry_url.as_ref()) {
        Some(u) => u.clone(),
        None => {
            return ToolCallResult::error(
                "No registry URL configured. Pass registry_url as an argument.",
            )
        }
    };

    let auth_client = AuthClient::new(&registry_url);

    let tokens = match auth_client.login(email, password) {
        Ok(t) => t,
        Err(e) => return ToolCallResult::error(format!("Login failed: {e}")),
    };

    if let Err(e) = auth::save_tokens(
        state,
        &registry_url,
        &tokens.access_token,
        &tokens.refresh_token,
    ) {
        return ToolCallResult::error(format!("Failed to save tokens: {e}"));
    }

    let user_info = match auth_client.me(&tokens.access_token) {
        Ok(u) => u,
        Err(e) => {
            return ToolCallResult::error(format!(
                "Login succeeded but failed to fetch user info: {e}"
            ))
        }
    };

    ToolCallResult::success(format!(
        "Logged in successfully.\n\
        Email: {}\n\
        Display name: {}",
        user_info.email, user_info.display_name,
    ))
}

fn handle_logout(state: &AppState, server_registry_url: &Option<String>) -> ToolCallResult {
    let registry_url = match server_registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    match auth::clear_tokens(state, registry_url) {
        Ok(()) => ToolCallResult::success("Logged out successfully."),
        Err(e) => ToolCallResult::error(format!("Failed to clear tokens: {e}")),
    }
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
            let _ = mcp_governance::fetch_approved_servers(state, &registry);

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

            let footer = if is_managed(state) {
                GOVERNANCE_FOOTER
            } else {
                ""
            };

            if formatted.is_empty() {
                ToolCallResult::success(format!(
                    "No approved MCP servers in catalog (approval mode: {}).\nAsk your IT admin to add servers via the SkillClub admin portal.{footer}",
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
                        output.push_str(footer);
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
                    Err(e) => {
                        return ToolCallResult::error(format!("Failed to submit request: {e}"))
                    }
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
        "approved" => ToolCallResult::success(format!(
            "Request for '{}' was approved! \
                 Use `skillclub_mcp_install` with server_name '{}' to activate it now.",
            server_name, server_name
        )),
        "pending" => ToolCallResult::success(format!(
            "Request for '{}' has been submitted and is pending IT review.\n\n\
             Your admin will review it in the SkillClub portal. \
             Run `skillclub_mcp_status` to check on it later, then use \
             `skillclub_mcp_install` to activate it once approved.",
            server_name
        )),
        _ => ToolCallResult::success(format!("Request submitted with status: {}", req_status)),
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
                    Err(e) => {
                        return ToolCallResult::error(format!("Failed to fetch requests: {e}"))
                    }
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

fn handle_import(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    let input = match arguments.get("input").and_then(|v| v.as_str()) {
        Some(i) if !i.is_empty() => i,
        _ => return ToolCallResult::error("Missing required parameter: input. Provide an npm package name, npx command, or GitHub URL."),
    };

    let confirm = arguments
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Ensure auth with refresh fallback
    let access_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let registry = RegistryClient::new(url);

    // Always preview first
    let preview = match registry.import_preview(input, &access_token) {
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
                match registry.import_preview(input, &new_token) {
                    Ok(v) => v,
                    Err(e) => return ToolCallResult::error(format!("Import preview failed: {e}")),
                }
            } else {
                return ToolCallResult::error(format!("Import preview failed: {e}"));
            }
        }
    };

    let preview_text = format_import_preview(&preview);

    if !confirm {
        return ToolCallResult::success(format!(
            "{}\n\nSet confirm=true to submit this import.",
            preview_text
        ));
    }

    // Submit
    let submit_token = match ensure_auth(state, url) {
        Ok(t) => t,
        Err(e) => return e,
    };

    match registry.import_submit(input, &submit_token) {
        Ok(result) => {
            let result_text = format_import_result(&result);
            ToolCallResult::success(result_text)
        }
        Err(e) => {
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
                match registry.import_submit(input, &new_token) {
                    Ok(result) => ToolCallResult::success(format_import_result(&result)),
                    Err(e) => ToolCallResult::error(format!("Import submit failed: {e}")),
                }
            } else {
                ToolCallResult::error(format!("Import submit failed: {e}"))
            }
        }
    }
}

fn format_import_preview(preview: &serde_json::Value) -> String {
    let import_type = preview
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match import_type {
        "skill" => {
            let name = preview.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let skill_id = preview
                .get("skill_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let version = preview
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let publisher = preview
                .get("publisher")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let desc = preview
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!(
                "Skill Import Preview\n  Name: {}\n  ID: {}\n  Version: {}\n  Publisher: {}\n  Description: {}",
                name, skill_id, version, publisher, desc
            )
        }
        "mcp_server" => {
            let pkg = preview
                .get("package_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let desc = preview
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ver = preview
                .get("latest_version")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let license = preview
                .get("license")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let in_catalog = preview
                .get("already_in_catalog")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mode = preview
                .get("approval_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let mut text = format!(
                "MCP Server Import Preview\n  Package: {}\n  Description: {}\n  Version: {}\n  License: {}\n  In catalog: {}\n  Approval mode: {}",
                pkg, desc, ver, license, in_catalog, mode
            );
            if let Some(keywords) = preview.get("keywords").and_then(|v| v.as_array()) {
                let kws: Vec<&str> = keywords.iter().filter_map(|k| k.as_str()).collect();
                if !kws.is_empty() {
                    text.push_str(&format!("\n  Keywords: {}", kws.join(", ")));
                }
            }
            text
        }
        _ => format!("Unknown import type: {}", import_type),
    }
}

fn format_import_result(result: &serde_json::Value) -> String {
    let import_type = result
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match import_type {
        "skill" => {
            let skill_id = result
                .get("skill_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let version = result
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = result
                .get("review_status")
                .and_then(|v| v.as_str())
                .unwrap_or("submitted");
            format!(
                "Skill imported successfully!\n  ID: {}\n  Version: {}\n  Status: {}",
                skill_id, version, status
            )
        }
        "mcp_server" => {
            let name = result
                .get("server_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let pkg = result
                .get("package_source")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = result.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let mut text = format!(
                "MCP Server import {}!\n  Server: {}\n  Package: {}\n  Status: {}",
                if status == "approved" {
                    "approved"
                } else {
                    "submitted"
                },
                name,
                pkg,
                status
            );
            if status == "approved" {
                text.push_str(
                    "\n\nThe server has been added to your catalog and will appear in your AI tools shortly.",
                );
            } else if status == "pending" {
                text.push_str("\n\nYour request has been submitted for admin review.");
            }
            text
        }
        _ => format!("Import completed (type: {})", import_type),
    }
}

// ── MCP install/uninstall handlers (need aggregator access) ──────────────────

/// Activate an approved MCP server by forcing an immediate aggregator sync.
/// Called from server.rs which has access to the aggregator.
pub fn handle_mcp_install(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
    aggregator: &crate::aggregator::BackendRegistry,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured"),
    };

    let server_name = match arguments.get("server_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: server_name"),
    };

    let server_id = crate::aggregator::sanitize_id(server_name);

    // Force an immediate sync with the registry
    let registry_client = RegistryClient::new(url);
    if let Err(e) = aggregator.sync(state, &registry_client) {
        return ToolCallResult::error(format!(
            "Failed to sync with registry: {e}. Check your network connection and registry URL."
        ));
    }

    // Check if the server is now in the aggregator
    if aggregator.has_backend(&server_id) {
        let tools = aggregator.backend_tools(&server_id);
        let tool_list = if tools.is_empty() {
            "No tools were exposed by this server.".to_string()
        } else {
            tools.join(", ")
        };
        ToolCallResult::success(format!(
            "MCP server '{}' is now active through SkillRunner governance.\n\nAvailable tools: {}",
            server_name, tool_list
        ))
    } else {
        // Server not in approved list — check if it's blocked or just not approved
        ToolCallResult::error(format!(
            "Server '{}' is not in the approved server list. \
             It may be pending approval, blocked, or not yet requested.\n\n\
             Use skillclub_mcp_request to request access, then retry skillclub_mcp_install \
             after approval.",
            server_name
        ))
    }
}

/// Remove a governed MCP server from the aggregator.
/// Called from server.rs which has access to the aggregator.
pub fn handle_mcp_uninstall(
    arguments: &serde_json::Value,
    registry_url: &Option<String>,
    aggregator: &crate::aggregator::BackendRegistry,
) -> ToolCallResult {
    if registry_url.is_none() {
        return ToolCallResult::error("No registry URL configured");
    }

    let server_name = match arguments.get("server_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: server_name"),
    };

    let server_id = crate::aggregator::sanitize_id(server_name);

    if aggregator.remove_backend(&server_id) {
        ToolCallResult::success(format!(
            "MCP server '{}' has been deactivated. Its tools are no longer available.",
            server_name
        ))
    } else {
        ToolCallResult::error(format!(
            "No active MCP server found with name '{}'. Use skillclub_mcp_status to see your servers.",
            server_name
        ))
    }
}

// ── Plugin handlers ─────────────────────────────────────────────────────────

fn handle_plugin_search(
    arguments: &serde_json::Value,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => return ToolCallResult::error("No registry URL configured."),
    };

    let query = arguments
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let registry = RegistryClient::new(url);
    match registry.search_plugins(query) {
        Ok(results) => {
            if results.is_empty() {
                ToolCallResult::success(format!("No plugins found matching '{query}'."))
            } else {
                let formatted: Vec<serde_json::Value> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "slug": r.slug,
                            "name": r.name,
                            "description": r.description,
                            "latest_version": r.latest_version,
                            "publisher": r.publisher_name,
                            "install_count": r.install_count,
                        })
                    })
                    .collect();
                match serde_json::to_string_pretty(&formatted) {
                    Ok(text) => ToolCallResult::success(text),
                    Err(e) => ToolCallResult::error(format!("Failed to serialize: {e}")),
                }
            }
        }
        Err(e) => ToolCallResult::error(format!("Plugin search failed: {e}")),
    }
}

fn handle_plugin_info(arguments: &serde_json::Value, state: &AppState) -> ToolCallResult {
    let plugin_id = match arguments["plugin_id"].as_str() {
        Some(id) => id,
        None => return ToolCallResult::error("plugin_id is required"),
    };

    match skillrunner_core::plugin::get_installed_plugin(state, plugin_id) {
        Ok(Some(plugin)) => ToolCallResult::success(
            serde_json::to_string_pretty(&serde_json::json!({
                "id": plugin.id,
                "name": plugin.manifest.name,
                "version": plugin.version,
                "description": plugin.manifest.description,
                "status": plugin.status,
                "components": {
                    "skills": plugin.components.skill_ids,
                    "mcp_servers": plugin.components.mcp_server_names,
                    "commands": plugin.components.command_names,
                },
                "installed_at": plugin.installed_at,
            }))
            .unwrap_or_default(),
        ),
        Ok(None) => ToolCallResult::error(format!("Plugin '{plugin_id}' is not installed.")),
        Err(e) => ToolCallResult::error(format!("Failed to get plugin info: {e}")),
    }
}

fn handle_plugin_install(
    arguments: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let path_or_id = match arguments["path_or_id"].as_str() {
        Some(id) => id,
        None => return ToolCallResult::error("path_or_id is required"),
    };

    let plugin_path = camino::Utf8PathBuf::from(path_or_id);

    if plugin_path.join("plugin.json").exists() {
        install_plugin_from_local(state, &plugin_path)
    } else {
        install_plugin_from_registry_slug(path_or_id, state, registry_url)
    }
}

fn install_plugin_from_local(state: &AppState, plugin_path: &camino::Utf8Path) -> ToolCallResult {
    match skillrunner_core::plugin::install_plugin_from_dir(state, plugin_path) {
        Ok(result) => {
            let mut response = serde_json::json!({
                "status": "success",
                "plugin_id": result.id,
                "version": result.version,
                "install_status": result.status,
                "installed_skills": result.components.skill_ids,
                "installed_commands": result.components.command_names,
            });
            if !result.components.mcp_server_names.is_empty() {
                response["mcp_servers_pending"] =
                    serde_json::json!(result.components.mcp_server_names);
                response["note"] = serde_json::json!(
                    "MCP servers require approval. Use skillclub_mcp_request to request access, then skillclub_mcp_install to activate."
                );
            }
            ToolCallResult::success(serde_json::to_string_pretty(&response).unwrap_or_default())
        }
        Err(e) => ToolCallResult::error(format!("Failed to install plugin: {e}")),
    }
}

fn install_plugin_from_registry_slug(
    slug: &str,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let url = match registry_url {
        Some(u) => u,
        None => {
            return ToolCallResult::error(
                "No registry URL configured. To install from a local directory, \
                 provide the path to a directory containing plugin.json.",
            )
        }
    };

    let registry = RegistryClient::new(url);
    match install_plugin_from_registry(state, &registry, slug) {
        Ok(result) => {
            let mut response = serde_json::json!({
                "status": "success",
                "plugin_id": result.id,
                "version": result.version,
                "install_status": result.status,
                "installed_skills": result.components.skill_ids,
                "installed_commands": result.components.command_names,
            });
            if !result.components.mcp_server_names.is_empty() {
                response["mcp_servers_pending"] =
                    serde_json::json!(result.components.mcp_server_names);
                response["note"] = serde_json::json!(
                    "MCP servers require approval. Use skillclub_mcp_request to request access, then skillclub_mcp_install to activate."
                );
            }
            ToolCallResult::success(serde_json::to_string_pretty(&response).unwrap_or_default())
        }
        Err(e) => ToolCallResult::error(format!(
            "Failed to install plugin '{slug}' from registry: {e}"
        )),
    }
}

fn handle_plugin_uninstall(arguments: &serde_json::Value, state: &AppState) -> ToolCallResult {
    let plugin_id = match arguments["plugin_id"].as_str() {
        Some(id) => id,
        None => return ToolCallResult::error("plugin_id is required"),
    };

    match skillrunner_core::plugin::uninstall_plugin(state, plugin_id) {
        Ok(Some(version)) => ToolCallResult::success(
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "success",
                "plugin_id": plugin_id,
                "version_removed": version,
            }))
            .unwrap_or_default(),
        ),
        Ok(None) => ToolCallResult::error(format!("Plugin '{plugin_id}' is not installed.")),
        Err(e) => ToolCallResult::error(format!("Failed to uninstall plugin: {e}")),
    }
}

fn handle_plugin_list(state: &AppState) -> ToolCallResult {
    match skillrunner_core::plugin::list_installed_plugins(state) {
        Ok(plugins) if plugins.is_empty() => ToolCallResult::success("No plugins installed."),
        Ok(plugins) => {
            let list: Vec<serde_json::Value> = plugins
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "name": p.manifest.name,
                        "version": p.version,
                        "status": p.status,
                        "skills": p.components.skill_ids,
                        "mcp_servers": p.components.mcp_server_names,
                        "commands": p.components.command_names,
                    })
                })
                .collect();
            ToolCallResult::success(
                serde_json::to_string_pretty(&serde_json::json!(list)).unwrap_or_default(),
            )
        }
        Err(e) => ToolCallResult::error(format!("Failed to list plugins: {e}")),
    }
}

fn handle_plugin_author(arguments: &serde_json::Value) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name"),
    };
    let description = arguments.get("description").and_then(|v| v.as_str());
    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    // Derive plugin ID: lowercase, spaces and special chars become hyphens
    let plugin_id: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    let plugin_dir = Utf8PathBuf::from(output_dir).join(&plugin_id);

    if let Err(e) = std::fs::create_dir_all(plugin_dir.join("skills")) {
        return ToolCallResult::error(format!("Failed to create plugin directory: {e}"));
    }
    if let Err(e) = std::fs::create_dir_all(plugin_dir.join("commands")) {
        return ToolCallResult::error(format!("Failed to create commands directory: {e}"));
    }

    let desc_value = description
        .map(|d| serde_json::Value::String(d.to_string()))
        .unwrap_or(serde_json::Value::Null);

    let manifest = serde_json::json!({
        "schema_version": "1.0",
        "id": plugin_id,
        "name": name,
        "version": "0.1.0",
        "publisher": "my-org",
        "description": desc_value,
        "skills": [],
        "mcp_servers": [],
        "commands": []
    });

    let manifest_str = match serde_json::to_string_pretty(&manifest) {
        Ok(s) => s,
        Err(e) => return ToolCallResult::error(format!("Failed to serialize manifest: {e}")),
    };

    if let Err(e) = std::fs::write(plugin_dir.join("plugin.json"), &manifest_str) {
        return ToolCallResult::error(format!("Failed to write plugin.json: {e}"));
    }

    let readme = format!(
        "# {name}\n\n{}\n",
        description.unwrap_or("A SkillClub plugin.")
    );
    if let Err(e) = std::fs::write(plugin_dir.join("README.md"), readme) {
        return ToolCallResult::error(format!("Failed to write README.md: {e}"));
    }

    ToolCallResult::success(serde_json::to_string_pretty(&serde_json::json!({
        "status": "created",
        "plugin_id": plugin_id,
        "path": plugin_dir.as_str(),
        "next_steps": "Add skills to skills/, MCP servers, or commands to commands/ before running plugin validate. A plugin must contain at least one component."
    })).unwrap_or_default())
}

fn handle_plugin_publish(
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
            return ToolCallResult::error("Not logged in. Run `skillrunner auth login` first.")
        }
        Err(e) => return ToolCallResult::error(format!("Failed to load auth tokens: {e}")),
    };

    // Package the plugin
    let utf8_path = camino::Utf8Path::new(path);
    let (archive_path, _sha) = match package_plugin(utf8_path) {
        Ok(r) => r,
        Err(e) => return ToolCallResult::error(format!("Failed to package plugin: {e}")),
    };

    // Publish to registry
    let registry = RegistryClient::new(url).with_auth(&tokens.access_token);
    let result = match registry.publish_plugin(&archive_path) {
        Ok(resp) => {
            let _ = fs::remove_file(&archive_path);
            let slug = resp
                .get("slug")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let version = resp
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            format!("Published plugin {slug}@{version} to registry successfully.")
        }
        Err(e) => {
            let _ = fs::remove_file(&archive_path);
            let err_msg = e.to_string();
            if err_msg.contains("already exists") && err_msg.contains("ersion") {
                return ToolCallResult::error(format!(
                    "{err_msg}\n\nTo publish a new version, bump the version in plugin.json \
                     (e.g., 0.1.0 → 0.2.0) and run skillclub_plugin_publish again."
                ));
            }
            return ToolCallResult::error(format!("Failed to publish plugin: {e}"));
        }
    };

    ToolCallResult::success(result)
}

// ── Plugin export handler ────────────────────────────────────────────────────

fn handle_plugin_export(arguments: &serde_json::Value) -> ToolCallResult {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolCallResult::error("Missing required parameter: path"),
    };

    let format = match arguments.get("format").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ToolCallResult::error("Missing required parameter: format"),
    };

    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let plugin_path = camino::Utf8Path::new(path);
    let out_path = camino::Utf8Path::new(output_dir);

    let result = match format {
        "claude-code" => skillrunner_core::plugin_export::export_claude_code(plugin_path, out_path),
        "mcpb" => skillrunner_core::plugin_export::export_mcpb(plugin_path, out_path),
        other => {
            return ToolCallResult::error(format!(
                "Unsupported format '{other}'. Use 'claude-code' or 'mcpb'."
            ))
        }
    };

    match result {
        Ok(exported_path) => {
            ToolCallResult::success(format!("Plugin exported successfully to: {exported_path}"))
        }
        Err(e) => ToolCallResult::error(format!("Export failed: {e}")),
    }
}

// ── Plugin import handler ─────────────────────────────────────────────────────

fn handle_plugin_import(arguments: &serde_json::Value) -> ToolCallResult {
    use skillrunner_core::plugin_import;

    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) => camino::Utf8PathBuf::from(p),
        None => return ToolCallResult::error("Missing required parameter: path"),
    };

    let output_dir = arguments
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let out = camino::Utf8PathBuf::from(output_dir);

    let format = match plugin_import::detect_plugin_format(&path) {
        Some(f) => f,
        None => {
            return ToolCallResult::error(format!(
                "Could not detect plugin format at '{}'. \
             Expected a Claude Code plugin directory (with .claude-plugin/) or a .mcpb file.",
                path
            ))
        }
    };

    let format_label = format!("{:?}", format);

    let result = match format {
        plugin_import::ExternalPluginFormat::ClaudeCode => {
            plugin_import::import_claude_code_plugin(&path, &out)
        }
        plugin_import::ExternalPluginFormat::Mcpb => plugin_import::import_mcpb(&path, &out),
    };

    match result {
        Ok(p) => {
            let payload = serde_json::json!({
                "status": "imported",
                "format": format_label,
                "output_path": p.as_str(),
                "next_steps": format!(
                    "Run 'skillrunner plugin validate {}' then 'skillrunner plugin install {}'",
                    p, p
                )
            });
            match serde_json::to_string_pretty(&payload) {
                Ok(text) => ToolCallResult::success(text),
                Err(e) => ToolCallResult::error(format!("Failed to serialize result: {e}")),
            }
        }
        Err(e) => ToolCallResult::error(format!("Import failed: {e}")),
    }
}

// ── Rating handler ───────────────────────────────────────────────────────────

fn handle_rate(arguments: &serde_json::Value, state: &AppState) -> ToolCallResult {
    let skill_id = match arguments.get("skill_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("Missing required parameter: skill_id"),
    };

    let rating = match arguments.get("rating").and_then(|v| v.as_str()) {
        Some(r) if r == "up" || r == "down" => r,
        Some(other) => {
            return ToolCallResult::error(format!(
                "Invalid rating '{other}': must be 'up' or 'down'"
            ))
        }
        None => return ToolCallResult::error("Missing required parameter: rating"),
    };

    // Look up the active version for this skill.
    let version: String = match Connection::open(&state.db_path) {
        Ok(conn) => conn
            .query_row(
                "SELECT active_version FROM installed_skills WHERE skill_id = ?1",
                rusqlite::params![skill_id],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "unknown".to_string()),
        Err(_) => "unknown".to_string(),
    };

    match Connection::open(&state.db_path) {
        Ok(conn) => {
            match skillrunner_core::ratings::record_rating(&conn, skill_id, &version, rating) {
                Ok(()) => {
                    let label = if rating == "up" {
                        "Thumbs up"
                    } else {
                        "Thumbs down"
                    };
                    ToolCallResult::success(format!(
                        "{label} recorded for {skill_id}. Thanks for the feedback!"
                    ))
                }
                Err(e) => ToolCallResult::error(format!("Failed to record rating: {e}")),
            }
        }
        Err(e) => ToolCallResult::error(format!("Failed to open database: {e}")),
    }
}

// ── Skill execution handler ──────────────────────────────────────────────────

/// Build the one-liner LLM execution prefix for a skill run.
///
/// Returns `None` when no LLM steps ran (pure tool/transform workflows).
/// Returns `Some(prefix_string)` when at least one LLM step fired.
///
/// The prefix distinguishes local Ollama from remote MCP sampling, and
/// collapses multi-step token counts into a single aggregate line.
fn build_llm_execution_summary(result: &RunResult) -> Option<String> {
    let llm_steps: Vec<_> = result
        .steps
        .iter()
        .filter(|s| s.model_source.is_some())
        .collect();

    if llm_steps.is_empty() {
        return None;
    }

    let total_prompt: u64 = llm_steps.iter().filter_map(|s| s.prompt_tokens).sum();
    let total_completion: u64 = llm_steps.iter().filter_map(|s| s.completion_tokens).sum();
    let total_latency: u64 = llm_steps.iter().filter_map(|s| s.latency_ms).sum();
    let step_count = llm_steps.len();

    // Determine the dominant source. If any step used MCP sampling we report
    // that, since it is the more surprising / noteworthy path.
    let has_sampling = llm_steps
        .iter()
        .any(|s| matches!(&s.model_source, Some(ModelSource::McpSampling)));

    let source_label = if has_sampling {
        "remote model via MCP sampling".to_string()
    } else {
        // All local — use the model name from the first LLM step.
        let model_name = llm_steps
            .iter()
            .find_map(|s| match &s.model_source {
                Some(ModelSource::Local(name)) => Some(name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "local model".to_string());
        format!("local model {model_name}")
    };

    let step_phrase = if step_count == 1 {
        "1 step".to_string()
    } else {
        format!("{step_count} steps")
    };

    Some(format!(
        "\u{25b6} Ran {} v{} \u{2014} used {source_label} across {step_phrase} \
         ({total_prompt}\u{2192}{total_completion} tokens, {total_latency}ms)",
        result.skill_id, result.version,
    ))
}

fn handle_skill_run(
    skill_id: &str,
    arguments: &serde_json::Value,
    state: &AppState,
    policy_client: &dyn PolicyClient,
    model_client: Option<&dyn ModelClient>,
    registry_client: Option<&RegistryClient>,
    update_check_cache: &UpdateCheckCache,
) -> ToolCallResult {
    // Check if skill is deactivated before attempting execution
    if let Ok(conn) = Connection::open(&state.db_path) {
        if let Ok(status) = conn.query_row(
            "SELECT current_status FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| row.get::<_, String>(0),
        ) {
            if status == "deactivated" {
                return ToolCallResult::error(format!(
                    "The skill '{}' has been deactivated by your organization's administrator. \
                     Please contact your IT department to resolve this or request reactivation.",
                    skill_id
                ));
            }
        }
    }

    // Update-check gate: unless the caller passes skip_update_check=true,
    // query the registry (with a 10-minute cache) and prompt the user if a
    // newer version is available.  Registry failures are silenced so they
    // never block the user's actual work.
    let skip_update_check = arguments
        .get("skip_update_check")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !skip_update_check {
        if let Some(update_prompt) =
            maybe_build_update_prompt(skill_id, state, registry_client, update_check_cache)
        {
            return update_prompt;
        }
    }

    match run_skill(
        state,
        policy_client,
        skill_id,
        arguments,
        model_client,
        registry_client,
    ) {
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

            let output_text = match &output {
                serde_json::Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };

            // Build the full response: LLM summary prefix (if any LLM steps
            // ran) at the top, skill output in the middle, rating prompt
            // (when due) at the bottom.
            let llm_summary = build_llm_execution_summary(&result);

            let mut text = match llm_summary {
                Some(summary) => format!("{summary}\n\n{output_text}"),
                None => output_text,
            };

            // Increment execution count and conditionally append the rating prompt.
            // Open a separate connection so failures here never block the response.
            if let Ok(conn) = Connection::open(&state.db_path) {
                let version_str = result.version.clone();
                let prompt_due = match skillrunner_core::ratings::increment_execution_count(
                    &conn,
                    &result.skill_id,
                    &version_str,
                ) {
                    Ok(count) => skillrunner_core::ratings::should_prompt_for_rating(count),
                    Err(_) => false,
                };

                if prompt_due {
                    let already_rated = skillrunner_core::ratings::has_existing_rating(
                        &conn,
                        &result.skill_id,
                        &version_str,
                    )
                    .unwrap_or(true); // default to true (suppress) on error

                    if !already_rated {
                        text.push_str(
                            "\n\n---\nWas this skill helpful? Reply 'thumbs up' or 'thumbs down'.",
                        );
                    }
                }
            }

            ToolCallResult::success(text)
        }
        Err(e) => {
            // Record failure in execution stats so the registry can track success rate.
            // Best-effort: open a separate connection so errors here never mask the real error.
            if let Ok(conn) = Connection::open(&state.db_path) {
                // Extract skill_id+version from resolver or fall back to the name we were given.
                // At this point we don't have a resolved version, so we record against the
                // skill_id string with an empty version sentinel that the registry can ignore.
                let _ =
                    skillrunner_core::ratings::record_failed_execution(&conn, skill_id, "unknown");
            }

            // Surface blocked/unpublished errors with a clear, actionable message
            // rather than the generic "Skill execution failed" prefix.
            let err_str = e.to_string();
            if let Some((_, reason_raw)) = err_str.split_once("is blocked:") {
                // Extract the reason from the error: "skill '...' is blocked: <reason>"
                let reason = reason_raw.trim();
                return ToolCallResult::error(format!(
                    "\u{26d4} Skill '{skill_id}' is blocked: {reason}\n\n\
                     If this skill was recently unpublished, run `skillclub_uninstall` to remove it.\n\
                     If you believe this is an error, contact your administrator."
                ));
            }

            // Build full error chain so the actual root cause (e.g. jsonschema
            // violation) is visible in the MCP response, not just the outermost
            // context wrap.
            let mut chain = format!("Skill execution failed: {e}");
            let mut source = e.source();
            while let Some(src) = source {
                chain.push_str(&format!("\n  caused by: {src}"));
                source = src.source();
            }
            ToolCallResult::error(chain)
        }
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
        Utf8PathBuf::from_path_buf(std::env::temp_dir().join(format!("mcp-tests-{label}-{nanos}")))
            .unwrap()
    }

    fn write_test_skill(root: &Utf8PathBuf) {
        // AUTH1f: SKILL.md-rooted test bundle (manifest.json is no longer
        // accepted by SkillPackage::load_from_dir). Inline schemas via
        // vh_schemas so the loader captures them as serde_json::Value.
        fs::create_dir_all(root.join("prompts")).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\n\
             name: Test Skill\n\
             description: A test skill for MCP testing\n\
             license: Apache-2.0\n\
             vh_version: 0.1.0\n\
             vh_publisher: skillclub\n\
             vh_permissions:\n  \
               network: none\n  \
               filesystem: none\n  \
               clipboard: none\n\
             vh_execution:\n  \
               sandbox: strict\n  \
               timeout_ms: 30000\n  \
               memory_mb: 256\n\
             vh_schemas:\n  \
               inputs:\n    \
                 type: object\n    \
                 properties:\n      \
                   query:\n        \
                     type: string\n    \
                 required:\n      \
                   - query\n  \
               outputs:\n    \
                 type: object\n\
             vh_workflow_ref: workflow.yaml\n\
             ---\n\
             \n\
             Do the thing.\n",
        )
        .unwrap();
        fs::write(
            root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .unwrap();
        fs::write(root.join("prompts/system.txt"), "Do the thing.").unwrap();
    }

    /// Store fake auth tokens so build_tool_list sees the user as logged in.
    fn fake_login(state: &AppState, url: &str) {
        skillrunner_core::auth::save_tokens(state, url, "fake-access", "fake-refresh").unwrap();
    }

    #[test]
    fn build_tool_list_includes_management_tools() {
        let state_root = temp_root("tool-list");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let url = "http://localhost:8000".to_string();
        fake_login(&state, &url);

        let tools = build_tool_list(&state, &Some(url));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"skillclub_list"));
        assert!(names.contains(&"skillclub_search"));
        assert!(names.contains(&"skillclub_install"));
        assert!(names.contains(&"skillclub_info"));
        assert!(names.contains(&"skillclub_author"));
        assert!(names.contains(&"skillclub_author_confirm"));
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
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

        let tools = build_tool_list(&state, &None);
        let skill_tool = tools.iter().find(|t| t.name == "test-skill");

        assert!(
            skill_tool.is_some(),
            "installed skill should appear as tool"
        );
        let tool = skill_tool.unwrap();
        // AUTH1f: description is the versioned manifest description; the old
        // auto-generated trigger suffix was tied to the legacy import path
        // and is no longer emitted when the skill's `triggers` list is empty.
        assert!(
            tool.description
                .starts_with("A test skill for MCP testing (v0.1.0)"),
            "description should start with versioned desc, got: {}",
            tool.description
        );
        // The input schema should match the vh_schemas.inputs block.
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
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

        let result = handle_list(
            &state,
            &None,
            &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        );
        assert!(result.is_error.is_none());
        let text = &result.content[0].text;
        assert!(
            text.contains("test-skill"),
            "should list test-skill, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_list_empty() {
        let state_root = temp_root("handle-list-empty");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_list(
            &state,
            &None,
            &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        );
        assert!(result.is_error.is_none());
        assert!(result.content[0].text.contains("No skills installed"));

        let _ = fs::remove_dir_all(&state_root);
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
        assert!(
            result.content[0].text.contains("path") || result.content[0].text.contains("skill_id")
        );

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
        assert!(
            result.is_error.is_none(),
            "got: {:?}",
            result.content[0].text
        );
        assert!(result.content[0].text.contains("test-skill"));
        assert!(result.content[0].text.contains("0.1.0"));

        // Verify the skill appears in the list
        let list_result = handle_list(
            &state,
            &None,
            &std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        );
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
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

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
            "mode": "skip_metadata",
            "output_dir": out_dir.as_str(),
        }));

        assert!(
            result.is_error.is_none(),
            "got: {:?}",
            result.content[0].text
        );
        let text = &result.content[0].text;
        assert!(text.contains("my-test-skill"), "got: {text}");

        // Verify bundle files were created (AUTH1f: SKILL.md, not manifest.json)
        let skill_dir = out_dir.join("my-test-skill");
        assert!(skill_dir.join("SKILL.md").exists());
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
    fn handle_author_interactive_returns_recommendations() {
        let result = handle_author(&serde_json::json!({
            "name": "Contract Compare",
            "system_prompt": "You compare two legal contracts and highlight differences.",
        }));
        assert!(result.is_error.is_none());
        let text = &result.content[0].text;
        assert!(
            text.contains("recommendations_ready"),
            "expected recommendations, got: {text}"
        );
        assert!(text.contains("vh_triggers"), "should include triggers");
        assert!(
            text.contains("vh_permissions"),
            "should include permissions"
        );
        assert!(text.contains("vh_model"), "should include model");
        assert!(text.contains("vh_execution"), "should include execution");
    }

    #[test]
    fn handle_author_accept_suggestions_scaffolds_with_metadata() {
        let out_dir = temp_root("author-accept");
        fs::create_dir_all(&out_dir).unwrap();

        let result = handle_author(&serde_json::json!({
            "name": "API Fetcher",
            "description": "Fetches data from REST APIs",
            "system_prompt": "You fetch data from API endpoints and format the JSON response.",
            "mode": "accept_suggestions",
            "output_dir": out_dir.as_str(),
        }));

        assert!(
            result.is_error.is_none(),
            "got: {:?}",
            result.content[0].text
        );

        let skill_dir = out_dir.join("api-fetcher");
        let skill_md = fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(
            skill_md.contains("vh_triggers"),
            "SKILL.md should have triggers"
        );
        assert!(
            skill_md.contains("vh_permissions"),
            "SKILL.md should have permissions"
        );
        assert!(skill_md.contains("vh_model"), "SKILL.md should have model");
        assert!(
            skill_md.contains("vh_execution"),
            "SKILL.md should have execution"
        );

        let _ = fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn handle_author_skip_metadata_scaffolds_bare() {
        let out_dir = temp_root("author-skip");
        fs::create_dir_all(&out_dir).unwrap();

        let result = handle_author(&serde_json::json!({
            "name": "Simple Skill",
            "system_prompt": "You help with things.",
            "mode": "skip_metadata",
            "output_dir": out_dir.as_str(),
        }));

        assert!(
            result.is_error.is_none(),
            "got: {:?}",
            result.content[0].text
        );

        let skill_dir = out_dir.join("simple-skill");
        let skill_md = fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(
            !skill_md.contains("vh_triggers"),
            "skip_metadata should not add triggers"
        );

        let _ = fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn handle_author_confirm_scaffolds_with_provided_values() {
        let out_dir = temp_root("author-confirm");
        fs::create_dir_all(&out_dir).unwrap();

        let result = handle_author_confirm(&serde_json::json!({
            "name": "My Confirmed Skill",
            "system_prompt": "You do confirmed things.",
            "description": "A confirmed skill",
            "vh_triggers": ["do confirmed things", "help with confirmation"],
            "vh_permissions": {
                "network": "registry",
                "filesystem": "none",
                "clipboard": "none"
            },
            "vh_model": {
                "min_params_b": 7.0,
                "recommended": ["llama3.2:8b"],
                "fallback": "error"
            },
            "vh_execution": {
                "timeout_ms": 60000,
                "memory_mb": 512,
                "sandbox": "relaxed"
            },
            "output_dir": out_dir.as_str(),
        }));

        assert!(
            result.is_error.is_none(),
            "got: {:?}",
            result.content[0].text
        );

        let skill_dir = out_dir.join("my-confirmed-skill");
        let skill_md = fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(
            skill_md.contains("do confirmed things"),
            "should contain provided trigger"
        );
        assert!(
            skill_md.contains("network: registry"),
            "should contain provided permission"
        );
        assert!(
            skill_md.contains("min_params_b: 7"),
            "should contain provided model param"
        );
        assert!(
            skill_md.contains("timeout_ms: 60000"),
            "should contain provided timeout"
        );

        let _ = fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn handle_author_confirm_requires_name() {
        let result = handle_author_confirm(&serde_json::json!({
            "system_prompt": "test",
        }));
        assert_eq!(result.is_error, Some(true));
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

        let result = handle_publish(&serde_json::json!({"path": "/tmp/fake"}), &state, &None);
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

    fn empty_update_cache() -> UpdateCheckCache {
        Arc::new(Mutex::new(std::collections::HashMap::new()))
    }

    #[test]
    fn handle_skill_run_not_installed() {
        let state_root = temp_root("handle-run-missing");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let policy = MockPolicyClient::new();
        let cache = empty_update_cache();

        let result = handle_skill_run(
            "ghost-skill",
            &serde_json::json!({}),
            &state,
            &policy,
            None,
            None,
            &cache,
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
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

        let policy = MockPolicyClient::new();
        let cache = empty_update_cache();
        let result = handle_skill_run(
            "test-skill",
            &serde_json::json!({"query": "hello"}),
            &state,
            &policy,
            None, // stub mode
            None,
            &cache,
        );
        // Stub mode returns no output, so we get the summary
        assert!(result.is_error.is_none());

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    // ── Update-check and skillclub_update tests ───────────────────────────

    #[test]
    fn handle_skill_run_skips_update_check_when_flag_set() {
        // With skip_update_check=true, the skill should run normally even when
        // the cache contains an update entry (no registry needed).
        let state_root = temp_root("run-skip-update");
        let skill_root = temp_root("run-skip-update-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

        // Pre-populate cache with an "update available" entry.
        let cache: UpdateCheckCache = Arc::new(Mutex::new(std::collections::HashMap::new()));
        {
            let mut guard = cache.lock().unwrap();
            guard.insert(
                "test-skill".to_string(),
                UpdateCheckEntry {
                    checked_at: Instant::now(),
                    latest_version: Some(Version::parse("9.9.9").unwrap()),
                },
            );
        }

        let policy = MockPolicyClient::new();
        let result = handle_skill_run(
            "test-skill",
            &serde_json::json!({"query": "hello", "skip_update_check": true}),
            &state,
            &policy,
            None,
            None,
            &cache,
        );
        // Should succeed (not blocked by the update prompt)
        assert!(
            result.is_error.is_none(),
            "expected success, got: {}",
            result.content[0].text
        );

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_skill_run_returns_update_prompt_when_available() {
        // When the cache already holds an update entry, the skill should NOT
        // execute — instead it should return the update prompt as an error.
        let state_root = temp_root("run-update-prompt");
        let skill_root = temp_root("run-update-prompt-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

        // Use a RegistryClient pointing at a mock server that returns a
        // higher version.  We bypass the network by pre-seeding the cache
        // directly so this test remains a unit test.
        let cache: UpdateCheckCache = Arc::new(Mutex::new(std::collections::HashMap::new()));
        {
            let mut guard = cache.lock().unwrap();
            guard.insert(
                "test-skill".to_string(),
                UpdateCheckEntry {
                    checked_at: Instant::now(),
                    latest_version: Some(Version::parse("2.0.0").unwrap()),
                },
            );
        }

        let policy = MockPolicyClient::new();
        // We need a RegistryClient so the update-check gate doesn't short-circuit.
        // Point it at a dummy URL — the cache hit means no actual HTTP call is made.
        let registry = RegistryClient::new("http://127.0.0.1:1"); // unreachable but not called
        let result = handle_skill_run(
            "test-skill",
            &serde_json::json!({"query": "hello"}),
            &state,
            &policy,
            None,
            Some(&registry),
            &cache,
        );
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].text;
        assert!(
            text.contains("Update available") || text.contains("update"),
            "expected update prompt, got: {text}"
        );
        assert!(
            text.contains("skillclub_update") || text.contains("skip_update_check"),
            "expected instructions in prompt, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_update_requires_skill_id() {
        let state_root = temp_root("update-no-id");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_update(
            &serde_json::json!({}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("skill_id"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_update_requires_registry_url() {
        let state_root = temp_root("update-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_update(
            &serde_json::json!({"skill_id": "some-skill"}),
            &state,
            &None,
        );
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0].text.contains("registry"),
            "got: {}",
            result.content[0].text
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_update_calls_install_from_registry() {
        use mockito::Server;
        let mut server = Server::new();

        // Endpoint 1: fetch_skill_detail returns latest_version
        let _mock_detail = server
            .mock("GET", "/skills/test-skill")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"skill_id":"test-skill","name":"Test Skill","latest_version":"0.2.0","publisher_name":"test","description":"A test skill"}"#)
            .create();

        // Endpoint 2: fetch_artifact_metadata
        let _mock_meta = server
            .mock("GET", "/skills/test-skill/versions/0.2.0/artifact")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"download_url":"/artifacts/test-skill-0.2.0.cskill","sha256":"abc123"}"#)
            .create();

        // We don't go further (download would fail) — but handle_update returning
        // "Update failed" is acceptable here because install_from_registry will
        // fail on the download step.  The key assertion is that it got past the
        // parameter validation and attempted the registry call.
        let state_root = temp_root("update-calls-registry");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let registry_url = server.url();

        let result = handle_update(
            &serde_json::json!({"skill_id": "test-skill"}),
            &state,
            &Some(registry_url),
        );

        // Either success (if the mock download somehow completes) or a registry-
        // related error — either way it must NOT be the "missing skill_id" or
        // "no registry" early-exit error.
        let text = &result.content[0].text;
        assert!(
            !text.contains("Missing required parameter"),
            "should not be param error, got: {text}"
        );
        assert!(
            !text.contains("No registry configured"),
            "should not be missing-registry error, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    // ── Auth tool tests ───────────────────────────────────────────────────

    #[test]
    fn handle_login_requires_email() {
        let state_root = temp_root("login-no-email");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_login(
            &serde_json::json!({"password": "secret"}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("email"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_login_requires_password() {
        let state_root = temp_root("login-no-password");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_login(
            &serde_json::json!({"email": "user@example.com"}),
            &state,
            &Some("http://localhost:8000".to_string()),
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("password"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_login_requires_registry_url() {
        let state_root = temp_root("login-no-registry");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_login(
            &serde_json::json!({"email": "user@example.com", "password": "secret"}),
            &state,
            &None,
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_login_success_returns_user_info() {
        use mockito::Server;
        let mut server = Server::new();

        let _mock_login = server
            .mock("POST", "/portal/auth/login")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"access_token":"tok_abc","refresh_token":"ref_xyz","token_type":"bearer"}"#,
            )
            .create();

        let _mock_me = server
            .mock("GET", "/portal/auth/me")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":"user-1","email":"alice@example.com","display_name":"Alice"}"#)
            .create();

        let state_root = temp_root("login-success");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let registry_url = server.url();

        let result = handle_login(
            &serde_json::json!({"email": "alice@example.com", "password": "correct"}),
            &state,
            &Some(registry_url.clone()),
        );

        assert!(
            result.is_error.is_none(),
            "expected success, got: {}",
            result.content[0].text
        );
        let text = &result.content[0].text;
        assert!(text.contains("alice@example.com"), "got: {text}");
        assert!(text.contains("Alice"), "got: {text}");

        // Verify tokens were persisted
        let loaded = auth::load_tokens(&state, &registry_url).unwrap();
        assert!(loaded.is_some(), "tokens should be saved after login");
        let loaded = loaded.unwrap();
        assert_eq!(loaded.access_token, "tok_abc");
        assert_eq!(loaded.refresh_token, "ref_xyz");

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_login_forwards_registry_url_arg() {
        use mockito::Server;
        let mut server = Server::new();

        let _mock_login = server
            .mock("POST", "/portal/auth/login")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"access_token":"tok_override","refresh_token":"ref_override","token_type":"bearer"}"#,
            )
            .create();

        let _mock_me = server
            .mock("GET", "/portal/auth/me")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":"u2","email":"bob@example.com","display_name":"Bob"}"#)
            .create();

        let state_root = temp_root("login-url-override");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let override_url = server.url();

        // Pass registry_url as an argument, server config has a different (wrong) URL.
        let result = handle_login(
            &serde_json::json!({
                "email": "bob@example.com",
                "password": "secret",
                "registry_url": override_url,
            }),
            &state,
            &Some("http://wrong-server:9999".to_string()),
        );

        assert!(
            result.is_error.is_none(),
            "expected success with URL override, got: {}",
            result.content[0].text
        );
        assert!(result.content[0].text.contains("Bob"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_login_bad_credentials_returns_error() {
        use mockito::Server;
        let mut server = Server::new();

        let _mock = server
            .mock("POST", "/portal/auth/login")
            .with_status(401)
            .with_body(r#"{"detail":"Invalid credentials"}"#)
            .create();

        let state_root = temp_root("login-bad-creds");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_login(
            &serde_json::json!({"email": "bad@example.com", "password": "wrong"}),
            &state,
            &Some(server.url()),
        );

        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0].text.contains("Login failed"),
            "got: {}",
            result.content[0].text
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_logout_clears_tokens() {
        let state_root = temp_root("logout-clears");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let registry_url = "http://localhost:8000".to_string();

        // Save some tokens first
        auth::save_tokens(&state, &registry_url, "tok", "ref").unwrap();
        assert!(auth::load_tokens(&state, &registry_url).unwrap().is_some());

        let result = handle_logout(&state, &Some(registry_url.clone()));
        assert!(
            result.is_error.is_none(),
            "expected success, got: {}",
            result.content[0].text
        );
        assert!(result.content[0].text.contains("Logged out"));

        // Tokens should be gone
        assert!(auth::load_tokens(&state, &registry_url).unwrap().is_none());

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_logout_requires_registry_url() {
        let state_root = temp_root("logout-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_logout(&state, &None);
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0].text.contains("registry"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_logout_succeeds_when_no_tokens_exist() {
        // Calling logout when already logged out should not error.
        let state_root = temp_root("logout-no-tokens");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let result = handle_logout(&state, &Some("http://localhost:8000".to_string()));
        assert!(
            result.is_error.is_none(),
            "logout with no stored tokens should succeed: {}",
            result.content[0].text
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_shows_login_when_not_logged_in() {
        let state_root = temp_root("tool-list-auth");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let tools = build_tool_list(&state, &Some("http://localhost:8000".to_string()));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            names.contains(&"skillclub_login"),
            "login tool should show when not logged in: {names:?}"
        );
        assert!(
            !names.contains(&"skillclub_logout"),
            "logout should not show when not logged in: {names:?}"
        );
        assert!(
            !names.contains(&"skillclub_search"),
            "search should not show when not logged in: {names:?}"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_shows_logout_when_logged_in() {
        let state_root = temp_root("tool-list-auth-logged-in");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let url = "http://localhost:8000".to_string();
        fake_login(&state, &url);

        let tools = build_tool_list(&state, &Some(url));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            !names.contains(&"skillclub_login"),
            "login should not show when logged in: {names:?}"
        );
        assert!(
            names.contains(&"skillclub_logout"),
            "logout tool should show when logged in: {names:?}"
        );
        assert!(
            names.contains(&"skillclub_search"),
            "search should show when logged in: {names:?}"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_omits_login_logout_without_registry() {
        let state_root = temp_root("tool-list-auth-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let tools = build_tool_list(&state, &None);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            !names.contains(&"skillclub_login"),
            "login should not appear without registry"
        );
        assert!(
            !names.contains(&"skillclub_logout"),
            "logout should not appear without registry"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn auth_elicitation_prompt_mentions_skillclub_login() {
        let prompt = auth_elicitation_prompt("http://registry.example.com");
        assert!(
            prompt.content[0].text.contains("skillclub_login"),
            "elicitation prompt should reference skillclub_login tool, got: {}",
            prompt.content[0].text
        );
    }

    // ── Governance edge case tests ───────────────────────────────────────────

    #[test]
    fn build_tool_list_includes_mcp_install_uninstall_when_logged_in() {
        let state_root = temp_root("tool-list-mcp-install");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let url = "http://localhost:8000".to_string();
        fake_login(&state, &url);

        let tools = build_tool_list(&state, &Some(url));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            names.contains(&"skillclub_mcp_install"),
            "mcp_install tool missing: {names:?}"
        );
        assert!(
            names.contains(&"skillclub_mcp_uninstall"),
            "mcp_uninstall tool missing: {names:?}"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn build_tool_list_omits_mcp_install_uninstall_without_registry() {
        let state_root = temp_root("tool-list-mcp-install-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let tools = build_tool_list(&state, &None);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            !names.contains(&"skillclub_mcp_install"),
            "mcp_install should not appear without registry"
        );
        assert!(
            !names.contains(&"skillclub_mcp_uninstall"),
            "mcp_uninstall should not appear without registry"
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_install_requires_server_name() {
        let state_root = temp_root("mcp-install-no-name");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let aggregator = crate::aggregator::BackendRegistry::new();
        let registry_url = Some("http://localhost:8000".to_string());

        let result = handle_mcp_install(&serde_json::json!({}), &state, &registry_url, &aggregator);
        assert!(result.is_error.unwrap_or(false));
        assert!(result.content[0].text.contains("server_name"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_install_requires_registry_url() {
        let state_root = temp_root("mcp-install-no-reg");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let aggregator = crate::aggregator::BackendRegistry::new();

        let result = handle_mcp_install(
            &serde_json::json!({"server_name": "playwright"}),
            &state,
            &None,
            &aggregator,
        );
        assert!(result.is_error.unwrap_or(false));
        assert!(result.content[0].text.contains("No registry URL"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_install_without_approval_returns_error() {
        // Server not in aggregator (no approved servers) → error
        let state_root = temp_root("mcp-install-no-approval");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let aggregator = crate::aggregator::BackendRegistry::new();

        // Use a mock server that returns empty approved list
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/api/runner/mcp-servers")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"approval_mode": "strict", "servers": []}"#)
            .create();

        let registry_url = Some(server.url());

        let result = handle_mcp_install(
            &serde_json::json!({"server_name": "unapproved-server"}),
            &state,
            &registry_url,
            &aggregator,
        );
        assert!(
            result.is_error.unwrap_or(false),
            "should error for unapproved server, got: {}",
            result.content[0].text
        );
        // After sync succeeds with empty list, server won't be in aggregator
        assert!(
            result.content[0]
                .text
                .contains("not in the approved server list"),
            "error should mention approval, got: {}",
            result.content[0].text
        );
        // Should not be in the aggregator
        assert!(!aggregator.has_backend("unapproved-server"));

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_install_sync_failure_returns_error() {
        // Registry unreachable and no cache → sync fails → error
        let state_root = temp_root("mcp-install-offline");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let aggregator = crate::aggregator::BackendRegistry::new();

        // Point to unreachable registry with no cache
        let registry_url = Some("http://127.0.0.1:1".to_string());

        let result = handle_mcp_install(
            &serde_json::json!({"server_name": "some-server"}),
            &state,
            &registry_url,
            &aggregator,
        );
        assert!(
            result.is_error.unwrap_or(false),
            "should error when sync fails, got: {}",
            result.content[0].text
        );
        assert!(
            result.content[0].text.contains("Failed to sync"),
            "error should mention sync failure, got: {}",
            result.content[0].text
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_install_blocked_server_returns_error() {
        // Server is in the list but status is "blocked" → aggregator filters it → error
        let state_root = temp_root("mcp-install-blocked");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();
        let aggregator = crate::aggregator::BackendRegistry::new();

        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/api/runner/mcp-servers")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"approval_mode": "strict", "servers": [{"name": "Bad Server", "status": "blocked", "package_source": "http://localhost:9999"}]}"#)
            .create();

        let registry_url = Some(server.url());

        let result = handle_mcp_install(
            &serde_json::json!({"server_name": "Bad Server"}),
            &state,
            &registry_url,
            &aggregator,
        );
        assert!(
            result.is_error.unwrap_or(false),
            "blocked server should return error, got: {}",
            result.content[0].text
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_uninstall_requires_server_name() {
        let aggregator = crate::aggregator::BackendRegistry::new();
        let registry_url = Some("http://localhost:8000".to_string());

        let result = handle_mcp_uninstall(&serde_json::json!({}), &registry_url, &aggregator);
        assert!(result.is_error.unwrap_or(false));
        assert!(result.content[0].text.contains("server_name"));
    }

    #[test]
    fn handle_mcp_uninstall_requires_registry_url() {
        let aggregator = crate::aggregator::BackendRegistry::new();

        let result = handle_mcp_uninstall(
            &serde_json::json!({"server_name": "test"}),
            &None,
            &aggregator,
        );
        assert!(result.is_error.unwrap_or(false));
        assert!(result.content[0].text.contains("No registry URL"));
    }

    #[test]
    fn handle_mcp_uninstall_nonexistent_server_returns_error() {
        let aggregator = crate::aggregator::BackendRegistry::new();
        let registry_url = Some("http://localhost:8000".to_string());

        let result = handle_mcp_uninstall(
            &serde_json::json!({"server_name": "nonexistent"}),
            &registry_url,
            &aggregator,
        );
        assert!(
            result.is_error.unwrap_or(false),
            "should error for nonexistent server"
        );
        assert!(
            result.content[0].text.contains("No active MCP server"),
            "got: {}",
            result.content[0].text
        );
    }

    #[test]
    fn handle_mcp_uninstall_removes_active_backend() {
        use crate::aggregator::{BackendConnection, HttpBackend, ToolVisibility};

        let aggregator = crate::aggregator::BackendRegistry::new();
        let registry_url = Some("http://localhost:8000".to_string());

        // Manually add a backend
        {
            let mut inner = aggregator.inner.lock().unwrap();
            inner.backends.insert(
                "playwright".to_string(),
                BackendConnection::Http(HttpBackend {
                    server_id: "playwright".to_string(),
                    name: "Playwright".to_string(),
                    url: "http://localhost:9999".to_string(),
                    tools: vec![],
                    tool_visibility: ToolVisibility::All,
                    priority: 50,
                    auth_token: None,
                }),
            );
        }
        assert_eq!(aggregator.backend_count(), 1);

        let result = handle_mcp_uninstall(
            &serde_json::json!({"server_name": "Playwright"}),
            &registry_url,
            &aggregator,
        );
        assert!(
            result.is_error.is_none() || !result.is_error.unwrap(),
            "should succeed, got: {}",
            result.content[0].text
        );
        assert!(result.content[0].text.contains("deactivated"));
        assert_eq!(aggregator.backend_count(), 0);
    }

    #[test]
    fn handle_mcp_request_approved_suggests_mcp_install() {
        let state_root = temp_root("mcp-req-approved");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let mut server = mockito::Server::new();
        let _mock = server
            .mock("POST", "/portal/mcp/requests")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"status": "approved", "server_name": "Playwright"}"#)
            .create();

        let mock_url = server.url();
        // Save auth tokens against the mock server URL
        auth::save_tokens(&state, &mock_url, "tok_test", "ref_test").unwrap();

        let result = handle_mcp_request(
            &serde_json::json!({"server_name": "Playwright"}),
            &state,
            &Some(mock_url),
        );
        assert!(
            result.content[0].text.contains("skillclub_mcp_install"),
            "approved response should mention skillclub_mcp_install, got: {}",
            result.content[0].text
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn handle_mcp_request_pending_suggests_mcp_install_after_approval() {
        let state_root = temp_root("mcp-req-pending");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let mut server = mockito::Server::new();
        let _mock = server
            .mock("POST", "/portal/mcp/requests")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"status": "pending", "server_name": "Slack"}"#)
            .create();

        let mock_url = server.url();
        auth::save_tokens(&state, &mock_url, "tok_test", "ref_test").unwrap();

        let result = handle_mcp_request(
            &serde_json::json!({"server_name": "Slack"}),
            &state,
            &Some(mock_url),
        );
        assert!(
            result.content[0].text.contains("skillclub_mcp_install"),
            "pending response should mention skillclub_mcp_install, got: {}",
            result.content[0].text
        );

        let _ = fs::remove_dir_all(&state_root);
    }

    // ── LLM execution summary tests ──────────────────────────────────────────

    fn write_tool_only_skill(root: &Utf8PathBuf) {
        // A skill whose workflow has only a `tool` step — no LLM.
        // extract_text reads a field named "doc" from the run input.
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\n\
             name: Tool Only Skill\n\
             description: A tool-only skill\n\
             license: Apache-2.0\n\
             vh_version: 0.1.0\n\
             vh_publisher: skillclub\n\
             vh_permissions:\n  \
               network: none\n  \
               filesystem: none\n  \
               clipboard: none\n\
             vh_execution:\n  \
               sandbox: strict\n  \
               timeout_ms: 30000\n  \
               memory_mb: 256\n\
             vh_schemas:\n  \
               inputs:\n    \
                 type: object\n    \
                 properties:\n      \
                   doc:\n        \
                     type: string\n    \
                 required:\n      \
                   - doc\n  \
               outputs:\n    \
                 type: object\n\
             vh_workflow_ref: workflow.yaml\n\
             ---\n\
             \n\
             Extract text.\n",
        )
        .unwrap();
        fs::write(
            root.join("workflow.yaml"),
            "name: tool_only_skill\nsteps:\n  - id: extract\n    type: tool\n    tool: extract_text\n    input: doc\n",
        )
        .unwrap();
    }

    #[test]
    fn handle_skill_run_prefixes_llm_summary() {
        use skillrunner_core::model::MockModelClient;

        let state_root = temp_root("run-llm-prefix");
        let skill_root = temp_root("run-llm-prefix-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

        let policy = MockPolicyClient::new();
        let cache = empty_update_cache();
        let mock_model = MockModelClient::new("skill output text").with_tokens(10, 5);

        let result = handle_skill_run(
            "test-skill",
            &serde_json::json!({"query": "hello", "skip_update_check": true}),
            &state,
            &policy,
            Some(&mock_model),
            None,
            &cache,
        );

        assert!(result.is_error.is_none(), "expected success");
        let text = &result.content[0].text;
        assert!(
            text.contains("\u{25b6} Ran test-skill"),
            "expected LLM summary prefix, got: {text}"
        );
        assert!(
            text.contains("local model mock-model"),
            "expected model name in prefix, got: {text}"
        );
        assert!(
            text.contains("skill output text"),
            "expected skill output after prefix, got: {text}"
        );
        // Prefix must come before the skill output
        let prefix_pos = text.find("\u{25b6}").unwrap();
        let output_pos = text.find("skill output text").unwrap();
        assert!(prefix_pos < output_pos, "prefix must appear before output");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_skill_run_no_prefix_for_tool_only_workflow() {
        let state_root = temp_root("run-tool-no-prefix");
        let skill_root = temp_root("run-tool-no-prefix-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_tool_only_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

        let policy = MockPolicyClient::new();
        let cache = empty_update_cache();

        let result = handle_skill_run(
            "tool-only-skill",
            &serde_json::json!({"doc": "hello world", "skip_update_check": true}),
            &state,
            &policy,
            None, // no model client — only tool steps run
            None,
            &cache,
        );

        assert!(
            result.is_error.is_none(),
            "expected success, got: {:?}",
            result.content[0].text
        );
        let text = &result.content[0].text;
        assert!(
            !text.contains("\u{25b6}"),
            "tool-only skill should have no LLM summary prefix, got: {text}"
        );

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn handle_skill_run_composite_with_rating_prompt() {
        use skillrunner_core::model::MockModelClient;

        // On the 3rd execution, both the LLM prefix and the rating prompt
        // should appear with the correct ordering: prefix → output → rating.
        let state_root = temp_root("run-composite-rating");
        let skill_root = temp_root("run-composite-rating-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_test_skill(&skill_root);
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg, skillrunner_core::install::InstallMode::Copy).unwrap();

        let policy = MockPolicyClient::new();
        let cache = empty_update_cache();
        let mock_model = MockModelClient::new("the answer").with_tokens(8, 4);

        // Run twice to increment the counter to 2.
        for _ in 0..2 {
            let _ = handle_skill_run(
                "test-skill",
                &serde_json::json!({"query": "q", "skip_update_check": true}),
                &state,
                &policy,
                Some(&mock_model),
                None,
                &cache,
            );
        }

        // 3rd run should trigger the rating prompt.
        let result = handle_skill_run(
            "test-skill",
            &serde_json::json!({"query": "q", "skip_update_check": true}),
            &state,
            &policy,
            Some(&mock_model),
            None,
            &cache,
        );

        assert!(result.is_error.is_none(), "expected success");
        let text = &result.content[0].text;

        assert!(
            text.contains("\u{25b6}"),
            "LLM summary prefix must be present, got: {text}"
        );
        assert!(
            text.contains("the answer"),
            "skill output must be present, got: {text}"
        );
        assert!(
            text.contains("Was this skill helpful?"),
            "rating prompt must be present, got: {text}"
        );

        // Order: prefix → output → rating
        let prefix_pos = text.find("\u{25b6}").unwrap();
        let output_pos = text.find("the answer").unwrap();
        let rating_pos = text.find("Was this skill helpful?").unwrap();
        assert!(prefix_pos < output_pos, "prefix must precede output");
        assert!(output_pos < rating_pos, "output must precede rating prompt");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }
}
