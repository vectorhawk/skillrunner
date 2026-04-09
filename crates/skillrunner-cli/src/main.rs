use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use skillrunner_core::{
    app::SkillRunnerApp,
    auth::{self, AuthClient},
    executor::run_skill,
    import::import_skill_md,
    install::{install_unpacked_skill, uninstall_skill},
    managed::load_managed_config,
    mcp_governance,
    ollama::{resolve_model, OllamaClient},
    policy::MockPolicyClient,
    registry::{HttpPolicyClient, RegistryClient},
    resolver::{resolve_skill, ResolveOutcome},
    updater::{check_skill_updates, install_from_registry, package_plugin, package_skill},
    validator::validate_bundle,
};
use skillrunner_manifest::SkillPackage;
use skillrunner_mcp::{
    server::{McpServerConfig, run_server},
    setup::{configure_client, detect_ai_clients, install_claude_skills, install_npx_claude_hook, install_npx_shell_wrapper, mark_first_run_offered},
};
use rusqlite::Connection;

#[derive(Parser)]
#[command(name = "skillrunner", version)]
#[command(about = "SkillRunner — local AI skill runtime and MCP aggregator", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Doctor {
        /// Ollama base URL (default: http://localhost:11434)
        #[arg(long, default_value = "http://localhost:11434")]
        ollama_url: String,
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// MCP server for AI client integration
    Mcp {
        #[command(subcommand)]
        command: McpCommands,
    },
    /// Manage SkillClub plugins (composite bundles of skills + MCP servers + commands)
    Plugin {
        #[command(subcommand)]
        command: PluginCommands,
    },
}

#[derive(Subcommand)]
enum McpCommands {
    /// Start the MCP server (stdio transport)
    Serve {
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
        /// Ollama base URL (default: http://localhost:11434)
        #[arg(long, default_value = "http://localhost:11434")]
        ollama_url: String,
        /// Model name to use via Ollama (default: auto-detect from Ollama)
        #[arg(long, default_value = "auto")]
        model: String,
    },
    /// Configure SkillRunner as an MCP server for detected AI clients
    Setup {
        /// SkillClub registry URL to configure
        #[arg(long)]
        registry_url: Option<String>,
        /// Which client to configure (claude, cursor, all)
        #[arg(long, default_value = "all")]
        client: String,
        /// Non-interactive mode for automated installs
        #[arg(long)]
        auto: bool,
    },
    /// Show aggregator backend status (approved servers from registry)
    Backends {
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Manually trigger a skill sync (check for updates, lifecycle changes)
    Sync {
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Log in to the SkillClub registry
    Login {
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
        /// Email address
        #[arg(long)]
        email: Option<String>,
        /// Password (use for non-interactive/CI environments without a TTY)
        #[arg(long)]
        password: Option<String>,
    },
    /// Log out from the SkillClub registry
    Logout {
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Show current authentication status
    Status {
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    Import { path: Utf8PathBuf },
    Search {
        query: String,
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    Info { path: Utf8PathBuf },
    /// Install a skill from a local path or from the registry by ID
    Install {
        /// Skill ID (for registry install) or local path to a skill directory
        skill_ref: String,
        /// Specific version to install from registry (default: latest)
        #[arg(long)]
        version: Option<String>,
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Publish a skill to the registry
    Publish {
        /// Path to the skill directory to publish
        path: Utf8PathBuf,
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Uninstall an installed skill
    Uninstall { skill_id: String },
    List,
    Resolve { skill_id: String },
    Run {
        skill_id: String,
        #[arg(long)]
        input: Utf8PathBuf,
        /// Ollama base URL (default: http://localhost:11434)
        #[arg(long, default_value = "http://localhost:11434")]
        ollama_url: String,
        /// Model name to use via Ollama (default: llama3.2)
        #[arg(long, default_value = "llama3.2")]
        model: String,
        /// Skip model invocation and use stub execution
        #[arg(long)]
        stub: bool,
        /// SkillClub registry URL for policy fetch and auto-update (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    Validate { path: Utf8PathBuf },
}

#[derive(Subcommand)]
enum PluginCommands {
    /// Install a plugin from a local directory
    Install {
        /// Path to a plugin directory containing plugin.json
        path: Utf8PathBuf,
    },
    /// Uninstall an installed plugin (removes skills, commands, and MCP server records)
    Uninstall {
        /// Plugin ID
        plugin_id: String,
    },
    /// List all installed plugins
    List,
    /// Validate a plugin bundle directory
    Validate {
        /// Path to the plugin directory to validate
        path: Utf8PathBuf,
    },
    /// Show detailed info about an installed plugin
    Info {
        /// Plugin ID
        plugin_id: String,
    },
    /// Search the registry for plugins
    Search {
        /// Search query
        query: Option<String>,
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Publish a plugin to the registry
    Publish {
        /// Path to the plugin directory
        path: Utf8PathBuf,
        /// SkillClub registry URL (overrides SKILLCLUB_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Scaffold a new plugin directory
    Author {
        /// Plugin name
        name: String,
        /// Output directory (default: current directory)
        #[arg(long)]
        output_dir: Option<Utf8PathBuf>,
    },
    /// Export a plugin to an external format for distribution
    Export {
        /// Path to the plugin directory
        path: Utf8PathBuf,
        /// Output format: "claude-code" or "mcpb"
        #[arg(long)]
        format: String,
        /// Output directory (default: current directory)
        #[arg(long)]
        output_dir: Option<Utf8PathBuf>,
    },
    /// Import an external plugin (Claude Code plugin or .mcpb) into SkillClub format
    Import {
        /// Path to the external plugin directory or .mcpb file
        path: Utf8PathBuf,
        /// Output directory for the converted plugin (default: current directory)
        #[arg(long)]
        output_dir: Option<Utf8PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // MCP serve mode: force logs to stderr with no ANSI colors to avoid
    // contaminating the stdio JSON-RPC transport on stdout.
    let is_mcp_serve = matches!(cli.command, Commands::Mcp { command: McpCommands::Serve { .. } });
    if is_mcp_serve {
        tracing_subscriber::fmt()
            .with_env_filter("info")
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter("info").init();
    }
    let app = SkillRunnerApp::bootstrap()?;
    let managed_registry_url: Option<String> = load_managed_config(&app.state)
        .and_then(|c| c.registry_url);

    match cli.command {
        Commands::Doctor { ollama_url, registry_url } => {
            println!("SkillRunner root: {}", app.state.root_dir);
            println!("State DB:         {}", app.state.db_path);
            println!("Status:           OK");

            let ollama = OllamaClient::new(&ollama_url, "");
            let health = ollama.health_check();
            if health.reachable {
                match ollama.list_models() {
                    Ok(models) => {
                        println!("Ollama:           OK ({} models available) at {}", models.len(), ollama_url);
                        for m in &models {
                            println!("  - {}", m.name);
                        }
                    }
                    Err(e) => {
                        println!("Ollama:           REACHABLE but failed to list models: {e}");
                    }
                }
            } else {
                println!("Ollama:           NOT REACHABLE at {}", ollama_url);
            }

            let effective_url = resolve_registry_url(registry_url, managed_registry_url.as_deref());
            match effective_url {
                Some(url) => {
                    let reg = RegistryClient::new(&url);
                    match reg.health_check() {
                        Ok(true) => println!("Registry:         OK at {}", url),
                        Ok(false) => println!("Registry:         NOT REACHABLE at {}", url),
                        Err(_) => println!("Registry:         NOT REACHABLE at {}", url),
                    }

                    match auth::load_tokens(&app.state, &url) {
                        Ok(Some(tokens)) => {
                            let auth_client = AuthClient::new(&url);
                            match auth_client.me(&tokens.access_token) {
                                Ok(user) => println!("Auth:             logged in as {} ({})", user.display_name, user.email),
                                Err(_) => println!("Auth:             token expired (run `skillrunner auth login`)"),
                            }
                        }
                        _ => println!("Auth:             not logged in"),
                    }
                }
                None => {
                    println!("Registry:         not configured");
                }
            }

            // MCP status
            let skillrunner_path = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "skillrunner".to_string());
            let clients = detect_ai_clients(&skillrunner_path);
            if clients.is_empty() {
                println!("MCP:              no AI clients detected");
            } else {
                for c in &clients {
                    if c.already_configured {
                        println!("MCP:              {} — configured ✓", c.name);
                    } else {
                        println!(
                            "MCP:              {} — not configured (run `skillrunner mcp setup`)",
                            c.name
                        );
                    }
                }
            }

            // Aggregator backend status (from cache if available)
            let effective_url_for_backends = resolve_registry_url(None, managed_registry_url.as_deref());
            if let Some(url) = effective_url_for_backends {
                let reg = RegistryClient::new(&url);
                match mcp_governance::fetch_approved_servers(&app.state, &reg) {
                    Ok(resp) => {
                        let approved = resp.servers.iter().filter(|s| s.status != "blocked").count();
                        let blocked = resp.servers.iter().filter(|s| s.status == "blocked").count();
                        println!(
                            "Aggregator:       {} backend(s) approved, {} blocked (run `skillrunner mcp backends` for details)",
                            approved, blocked
                        );
                    }
                    Err(_) => {
                        println!("Aggregator:       no backend data (server not yet started or no registry connection)");
                    }
                }
            } else {
                println!("Aggregator:       not configured (no registry URL)");
            }
        }
        Commands::Auth { command } => match command {
            AuthCommands::Login { registry_url, email, password } => {
                let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;

                let email = match email {
                    Some(e) => e,
                    None => {
                        eprint!("Email: ");
                        let mut buf = String::new();
                        std::io::stdin().read_line(&mut buf)?;
                        buf.trim().to_string()
                    }
                };

                let password = match password {
                    Some(p) => p,
                    None => rpassword::read_password_from_tty(Some("Password: "))?,
                };
                let auth_client = AuthClient::new(&url);
                let tokens = auth_client.login(&email, &password)?;
                auth::save_tokens(&app.state, &url, &tokens.access_token, &tokens.refresh_token)?;

                let user = auth_client.me(&tokens.access_token)?;
                println!("Logged in as {} ({}) at {}", user.display_name, user.email, url);
            }
            AuthCommands::Logout { registry_url } => {
                let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;
                auth::clear_tokens(&app.state, &url)?;
                println!("Logged out from {}", url);
            }
            AuthCommands::Status { registry_url } => {
                let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;
                match auth::load_tokens(&app.state, &url)? {
                    Some(tokens) => {
                        let auth_client = AuthClient::new(&url);
                        match auth_client.me(&tokens.access_token) {
                            Ok(user) => println!("Logged in as {} ({}) at {}", user.display_name, user.email, url),
                            Err(_) => println!("Token expired at {}. Run `skillrunner auth login` to re-authenticate.", url),
                        }
                    }
                    None => println!("Not logged in at {}", url),
                }
            }
        },
        Commands::Mcp { command } => match command {
            McpCommands::Serve { registry_url, ollama_url, model } => {
                let effective_url = resolve_registry_url(registry_url, managed_registry_url.as_deref());
                let config = McpServerConfig {
                    registry_url: effective_url,
                    ollama_url,
                    model,
                };
                run_server(app.state, config)?;
            }
            McpCommands::Setup { registry_url, client, auto } => {
                let effective_url = resolve_registry_url(registry_url, managed_registry_url.as_deref());
                let skillrunner_path = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.to_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "skillrunner".to_string());

                let clients = detect_ai_clients(&skillrunner_path);
                if clients.is_empty() {
                    if !auto {
                        println!("No AI clients detected (looked for Claude Code, Cursor, Windsurf, VS Code, Gemini CLI).");
                    }
                    return Ok(());
                }

                let mut configured = 0u32;
                for detected in &clients {
                    let should_configure = match client.as_str() {
                        "all" => true,
                        "claude" => detected.name == "Claude Code",
                        "cursor" => detected.name == "Cursor",
                        "windsurf" => detected.name == "Windsurf",
                        "vscode" => detected.name == "VS Code",
                        "gemini" => detected.name == "Gemini CLI",
                        _ => {
                            if !auto {
                                println!("Unknown client '{}'. Use: claude, cursor, windsurf, vscode, gemini, or all", client);
                            }
                            return Ok(());
                        }
                    };

                    if !should_configure {
                        continue;
                    }

                    if detected.already_configured {
                        if !auto {
                            println!("  {} — already configured ✓", detected.name);
                        }
                        configured += 1;
                        continue;
                    }

                    match configure_client(detected, &skillrunner_path, &effective_url) {
                        Ok(()) => {
                            configured += 1;
                            if !auto {
                                println!(
                                    "  {} — configured ✓ ({})",
                                    detected.name,
                                    detected.config_path.display()
                                );
                            }
                        }
                        Err(e) => {
                            if !auto {
                                println!("  {} — failed: {e}", detected.name);
                            }
                        }
                    }
                }

                // Install slash command skills to ~/.claude/skills/
                match install_claude_skills() {
                    Ok(installed) => {
                        if !auto && !installed.is_empty() {
                            println!("\n  Installed {} slash command(s): {}",
                                installed.len(),
                                installed.iter().map(|s| format!("/{s}")).collect::<Vec<_>>().join(", "));
                        }
                    }
                    Err(e) => {
                        if !auto {
                            println!("\n  Warning: could not install slash commands: {e}");
                        }
                    }
                }

                // Install Claude Code npx guard hook
                match install_npx_claude_hook() {
                    Ok(true) => {
                        if !auto {
                            println!("  NPX guard hook — installed ✓");
                        }
                    }
                    Ok(false) => {} // already installed
                    Err(e) => {
                        if !auto {
                            println!("  NPX guard hook — skipped: {e}");
                        }
                    }
                }

                // Install shell npx wrapper
                match install_npx_shell_wrapper(&app.state) {
                    Ok(Some(path)) => {
                        if !auto {
                            println!("  NPX shell wrapper — installed ✓");
                            println!("    To activate: add {} to your PATH", path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(&path));
                        }
                    }
                    Ok(None) => {} // already installed
                    Err(e) => {
                        if !auto {
                            println!("  NPX shell wrapper — skipped: {e}");
                        }
                    }
                }

                mark_first_run_offered(&app.state)?;

                if !auto && configured > 0 {
                    println!("\nRestart your AI client to activate SkillRunner MCP.");
                }
            }
            McpCommands::Backends { registry_url } => {
                let url = resolve_registry_url(registry_url, managed_registry_url.as_deref());
                match url {
                    None => {
                        println!("No registry URL configured. Pass --registry-url or set SKILLCLUB_REGISTRY_URL.");
                    }
                    Some(url) => {
                        let registry = RegistryClient::new(&url);
                        match mcp_governance::fetch_approved_servers(&app.state, &registry) {
                            Ok(resp) => {
                                let approved: Vec<_> = resp.servers.iter()
                                    .filter(|s| s.status != "blocked")
                                    .collect();
                                let blocked: Vec<_> = resp.servers.iter()
                                    .filter(|s| s.status == "blocked")
                                    .collect();

                                println!("Approval mode:    {}", resp.approval_mode);
                                println!("Backends:         {} approved, {} blocked", approved.len(), blocked.len());
                                println!();

                                if approved.is_empty() {
                                    println!("No approved backends. Ask your IT admin to approve servers via the SkillClub portal.");
                                } else {
                                    println!("{:<25} {:<12} {:<12} {:<8} VISIBILITY", "NAME", "SERVER ID", "TRANSPORT", "PRIORITY");
                                    println!("{}", "-".repeat(75));
                                    for s in &approved {
                                        let server_id = s.server_id.as_deref().unwrap_or(&s.name);
                                        let transport = s.transport_type.as_deref().unwrap_or("http");
                                        let priority = s.priority.map(|p| p.to_string()).unwrap_or_else(|| "50".to_string());
                                        let visibility = s.tool_visibility.as_deref().unwrap_or("all");
                                        println!("{:<25} {:<12} {:<12} {:<8} {}", s.name, server_id, transport, priority, visibility);
                                    }
                                }

                                if !blocked.is_empty() {
                                    println!();
                                    println!("Blocked servers ({}):", blocked.len());
                                    for s in &blocked {
                                        println!("  {} ({})", s.name, s.package_source);
                                    }
                                }
                            }
                            Err(e) => {
                                println!("Failed to fetch backend list: {e}");
                                println!("(No cached data available — start the server with --registry-url to populate cache.)");
                            }
                        }
                    }
                }
            }
            McpCommands::Sync { registry_url } => {
                let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;
                let registry = RegistryClient::new(&url);
                let policy_client = HttpPolicyClient::new(RegistryClient::new(&url), &app.state);

                println!("Syncing skills from {}...", url);
                match check_skill_updates(&app.state, &registry, &policy_client) {
                    Ok(count) => {
                        if count > 0 {
                            println!("Updated {} skill(s).", count);
                        } else {
                            println!("All skills are up to date.");
                        }
                    }
                    Err(e) => println!("Sync failed: {e}"),
                }
            }
        },
        Commands::Skill { command } => match command {
            SkillCommands::Import { path } => {
                let bundle = import_skill_md(&path)?;
                println!("Imported skill: {}", bundle.id);
                println!("Output:         {}", bundle.output_dir);
                for f in &bundle.files {
                    println!("  wrote {f}");
                }
            }
            SkillCommands::Search { query, registry_url } => {
                let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;
                let reg = RegistryClient::new(&url);
                let results = reg.search_skills(&query)?;
                if results.is_empty() {
                    println!("No skills found matching '{query}'.");
                } else {
                    println!("{:<25} {:<30} {:<10} PUBLISHER", "ID", "NAME", "VERSION");
                    println!("{}", "-".repeat(75));
                    for r in &results {
                        println!(
                            "{:<25} {:<30} {:<10} {}",
                            r.skill_id,
                            r.name,
                            r.latest_version.as_deref().unwrap_or("-"),
                            r.publisher_name.as_deref().unwrap_or("-"),
                        );
                    }
                }
            }
            SkillCommands::Info { path } => {
                let skill = SkillPackage::load_from_dir(path)?;
                println!("id: {}", skill.manifest.id);
                println!("name: {}", skill.manifest.name);
                println!("version: {}", skill.manifest.version);
                println!("publisher: {}", skill.manifest.publisher);
                println!("entrypoint: {}", skill.manifest.entrypoint);
                println!("steps: {}", skill.workflow.steps.len());
            }
            SkillCommands::Uninstall { skill_id } => {
                match uninstall_skill(&app.state, &skill_id)? {
                    Some(version) => println!("Uninstalled {}@{}", skill_id, version),
                    None => println!("Skill '{}' is not installed.", skill_id),
                }
            }
            SkillCommands::Install { skill_ref, version, registry_url } => {
                // Heuristic: if skill_ref looks like a path, install from local dir.
                let is_local = skill_ref.contains('/')
                    || skill_ref.starts_with('.')
                    || std::path::Path::new(&skill_ref).exists();

                if is_local {
                    let path = Utf8PathBuf::from(&skill_ref);
                    let skill = SkillPackage::load_from_dir(path)?;
                    install_unpacked_skill(&app.state, &skill)?;
                    println!("Installed {}@{}", skill.manifest.id, skill.manifest.version);
                } else {
                    let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;
                    let registry = RegistryClient::new(&url);
                    let installed_ver = install_from_registry(
                        &app.state,
                        &registry,
                        &skill_ref,
                        version.as_deref(),
                    )?;
                    println!("Installed {}@{} from registry", skill_ref, installed_ver);
                }
            }
            SkillCommands::Publish { path, registry_url } => {
                let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;
                let tokens = auth::load_tokens(&app.state, &url)?
                    .ok_or_else(|| anyhow::anyhow!(
                        "not logged in; run `skillrunner auth login --registry-url {url}` first"
                    ))?;

                let (archive_path, _sha) = package_skill(&path)?;
                println!("Packaged {}", archive_path);

                let registry = RegistryClient::new(&url).with_auth(&tokens.access_token);
                match registry.publish_skill(&archive_path) {
                    Ok(resp) => {
                        println!("Published successfully!");
                        if let Some(id) = resp.get("skill_id").and_then(|v| v.as_str()) {
                            println!("  skill_id: {id}");
                        }
                        if let Some(ver) = resp.get("version").and_then(|v| v.as_str()) {
                            println!("  version:  {ver}");
                        }
                    }
                    Err(e) => {
                        // Try token refresh before giving up
                        let auth_client = AuthClient::new(&url);
                        if let Ok(new_tokens) = auth_client.refresh(&tokens.refresh_token) {
                            auth::save_tokens(&app.state, &url, &new_tokens.access_token, &new_tokens.refresh_token)?;
                            let registry = RegistryClient::new(&url).with_auth(&new_tokens.access_token);
                            let resp = registry.publish_skill(&archive_path)?;
                            println!("Published successfully!");
                            if let Some(id) = resp.get("skill_id").and_then(|v| v.as_str()) {
                                println!("  skill_id: {id}");
                            }
                            if let Some(ver) = resp.get("version").and_then(|v| v.as_str()) {
                                println!("  version:  {ver}");
                            }
                        } else {
                            return Err(e);
                        }
                    }
                }

                let _ = std::fs::remove_file(&archive_path);
            }
            SkillCommands::List => {
                let conn = Connection::open(&app.state.db_path)?;
                let mut stmt = conn.prepare(
                    "SELECT skill_id, active_version, current_status FROM installed_skills ORDER BY skill_id",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                for row in rows {
                    let (skill_id, version, status) = row?;
                    println!("{} {} [{}]", skill_id, version, status);
                }
            }
            SkillCommands::Resolve { skill_id } => {
                let outcome = if let Some(url) = resolve_registry_url(None, managed_registry_url.as_deref()) {
                    let policy_client = HttpPolicyClient::new(RegistryClient::new(&url), &app.state);
                    resolve_skill(&app.state, &policy_client, &skill_id)?
                } else {
                    let policy_client = MockPolicyClient::new();
                    resolve_skill(&app.state, &policy_client, &skill_id)?
                };
                match outcome {
                    ResolveOutcome::Active {
                        skill_id,
                        version,
                        install_path,
                    } => {
                        println!("status: active");
                        println!("skill_id: {}", skill_id);
                        println!("version: {}", version);
                        println!("install_path: {}", install_path);
                    }
                    ResolveOutcome::Blocked { skill_id, reason } => {
                        println!("status: blocked");
                        println!("skill_id: {}", skill_id);
                        println!("reason: {}", reason);
                    }
                    ResolveOutcome::NotInstalled { skill_id } => {
                        println!("status: not_installed");
                        println!("skill_id: {}", skill_id);
                    }
                }
            }
            SkillCommands::Run { skill_id, input, ollama_url, model, stub, registry_url } => {
                let input_text = std::fs::read_to_string(&input)
                    .map_err(|e| anyhow::anyhow!("failed to read {input}: {e}"))?;
                let input_json: serde_json::Value = serde_json::from_str(&input_text)
                    .map_err(|e| anyhow::anyhow!("{input} is not valid JSON: {e}"))?;

                // When not in stub mode, resolve the model against what Ollama
                // actually has available.  If the requested model is missing we
                // fall back to the first installed model rather than failing
                // with a 404 at generate time.  In stub mode we skip the
                // network call entirely.
                let effective_model = if stub {
                    model.clone()
                } else {
                    let probe = OllamaClient::new(&ollama_url, "");
                    resolve_model(&probe, &model)
                        .with_context(|| format!("failed to resolve model '{model}' from Ollama at {ollama_url}"))?
                };

                let ollama = OllamaClient::new(ollama_url, effective_model);
                let model_client: Option<&dyn skillrunner_core::model::ModelClient> = if stub {
                    None
                } else {
                    Some(&ollama)
                };

                let effective_url = resolve_registry_url(registry_url, managed_registry_url.as_deref());
                let result = if let Some(url) = effective_url {
                    let registry = RegistryClient::new(&url);
                    let http_policy = HttpPolicyClient::new(RegistryClient::new(&url), &app.state);
                    run_skill(&app.state, &http_policy, &skill_id, &input_json, model_client, Some(&registry))?
                } else {
                    let policy_client = MockPolicyClient::new();
                    run_skill(&app.state, &policy_client, &skill_id, &input_json, model_client, None)?
                };
                println!("Running {}@{}", result.skill_id, result.version);
                for step in &result.steps {
                    println!("  [{}] {}: {}", step.step_type.to_uppercase(), step.id, step.note);
                    if let Some(output) = &step.output {
                        println!("       output: {}", serde_json::to_string_pretty(output).unwrap_or_default());
                    }
                }
                println!(
                    "Done. ({} prompt + {} completion tokens, {}ms total)",
                    result.total_prompt_tokens,
                    result.total_completion_tokens,
                    result.total_latency_ms,
                );
            }
            SkillCommands::Validate { path } => {
                println!("Validating {path}");
                let report = validate_bundle(&path);
                for check in &report.checks {
                    if check.passed {
                        println!("  [OK]   {}", check.name);
                    } else {
                        println!("  [FAIL] {}", check.name);
                        if let Some(detail) = &check.detail {
                            println!("         {detail}");
                        }
                    }
                }
                if report.all_passed() {
                    println!("All checks passed.");
                } else {
                    anyhow::bail!("one or more validation checks failed");
                }
            }
        },
        Commands::Plugin { command } => match command {
            PluginCommands::Install { path } => {
                let pkg = skillrunner_manifest::PluginPackage::load_from_dir(&path)?;
                println!("Installing plugin '{}' v{}", pkg.manifest.name, pkg.manifest.version);

                let result = skillrunner_core::plugin::install_plugin_from_dir(&app.state, &path)?;

                if !result.components.skill_ids.is_empty() {
                    println!("  Skills: {}", result.components.skill_ids.join(", "));
                }
                if !result.components.mcp_server_names.is_empty() {
                    println!("  MCP servers (pending approval): {}", result.components.mcp_server_names.join(", "));
                }
                if !result.components.command_names.is_empty() {
                    println!("  Commands: {}", result.components.command_names.iter().map(|c| format!("/{c}")).collect::<Vec<_>>().join(", "));
                }
                println!("  Status: {}", result.status);
                println!("\nPlugin '{}' installed.", result.id);
            }
            PluginCommands::Uninstall { plugin_id } => {
                match skillrunner_core::plugin::uninstall_plugin(&app.state, &plugin_id)? {
                    Some(version) => println!("Uninstalled plugin '{plugin_id}' v{version}"),
                    None => println!("Plugin '{plugin_id}' is not installed."),
                }
            }
            PluginCommands::List => {
                let plugins = skillrunner_core::plugin::list_installed_plugins(&app.state)?;
                if plugins.is_empty() {
                    println!("No plugins installed.");
                } else {
                    for p in &plugins {
                        println!("  {} v{} [{}]", p.id, p.version, p.status);
                        if !p.components.skill_ids.is_empty() {
                            println!("    skills: {}", p.components.skill_ids.join(", "));
                        }
                        if !p.components.mcp_server_names.is_empty() {
                            println!("    mcp servers: {}", p.components.mcp_server_names.join(", "));
                        }
                        if !p.components.command_names.is_empty() {
                            println!("    commands: {}", p.components.command_names.iter().map(|c| format!("/{c}")).collect::<Vec<_>>().join(", "));
                        }
                    }
                }
            }
            PluginCommands::Validate { path } => {
                println!("Validating plugin at {path}");
                match skillrunner_manifest::PluginPackage::load_from_dir(&path) {
                    Ok(pkg) => {
                        println!("  [OK] plugin.json valid");
                        println!("  ID:        {}", pkg.manifest.id);
                        println!("  Name:      {}", pkg.manifest.name);
                        println!("  Version:   {}", pkg.manifest.version);
                        println!("  Skills:    {}", pkg.manifest.skills.len());
                        println!("  MCP Srvrs: {}", pkg.manifest.mcp_servers.len());
                        println!("  Commands:  {}", pkg.manifest.commands.len());
                        println!("All checks passed.");
                    }
                    Err(e) => {
                        println!("  [FAIL] {e}");
                        anyhow::bail!("plugin validation failed");
                    }
                }
            }
            PluginCommands::Info { plugin_id } => {
                match skillrunner_core::plugin::get_installed_plugin(&app.state, &plugin_id)? {
                    Some(p) => {
                        println!("Plugin: {} v{}", p.id, p.version);
                        println!("Name:   {}", p.manifest.name);
                        if let Some(desc) = &p.manifest.description {
                            println!("Desc:   {desc}");
                        }
                        println!("Status: {}", p.status);
                        if !p.components.skill_ids.is_empty() {
                            println!("Skills: {}", p.components.skill_ids.join(", "));
                        }
                        if !p.components.mcp_server_names.is_empty() {
                            println!("MCP:    {}", p.components.mcp_server_names.join(", "));
                        }
                        if !p.components.command_names.is_empty() {
                            println!("Cmds:   {}", p.components.command_names.iter().map(|c| format!("/{c}")).collect::<Vec<_>>().join(", "));
                        }
                    }
                    None => println!("Plugin '{plugin_id}' is not installed."),
                }
            }
            PluginCommands::Search { query, registry_url } => {
                let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;
                let query_str = query.as_deref().unwrap_or("");
                let registry = RegistryClient::new(&url);
                let results = registry
                    .search_plugins(query_str)
                    .with_context(|| format!("failed to search plugins matching '{query_str}'"))?;
                if results.is_empty() {
                    println!("No plugins found matching '{query_str}'.");
                } else {
                    println!("{:<30} {:<12} NAME", "SLUG", "VERSION");
                    for r in &results {
                        println!(
                            "{:<30} {:<12} {}",
                            r.slug,
                            r.latest_version.as_deref().unwrap_or("-"),
                            r.name
                        );
                    }
                }
            }
            PluginCommands::Publish { path, registry_url } => {
                let url = require_registry_url(registry_url, managed_registry_url.as_deref())?;
                let (archive_path, _sha) = package_plugin(&path)
                    .with_context(|| format!("failed to package plugin at {path}"))?;
                println!("Packaged plugin: {archive_path}");

                let token = auth::load_tokens(&app.state, &url)?
                    .ok_or_else(|| anyhow::anyhow!("not logged in; run `skillrunner auth login` first"))?;

                let registry = RegistryClient::new(&url).with_auth(&token.access_token);
                let resp = registry
                    .publish_plugin(&archive_path)
                    .with_context(|| "failed to publish plugin to registry")?;

                let _ = std::fs::remove_file(&archive_path);

                let slug = resp.get("slug").and_then(|v| v.as_str()).unwrap_or("unknown");
                let version = resp.get("version").and_then(|v| v.as_str()).unwrap_or("unknown");
                println!("Published plugin {slug}@{version} to registry.");
            }
            PluginCommands::Export { path, format, output_dir } => {
                let out = output_dir
                    .as_deref()
                    .unwrap_or_else(|| camino::Utf8Path::new("."));
                let result = match format.as_str() {
                    "claude-code" => {
                        skillrunner_core::plugin_export::export_claude_code(&path, out)
                            .with_context(|| format!("failed to export plugin at {path} as claude-code"))?
                    }
                    "mcpb" => {
                        skillrunner_core::plugin_export::export_mcpb(&path, out)
                            .with_context(|| format!("failed to export plugin at {path} as mcpb"))?
                    }
                    other => {
                        anyhow::bail!("unsupported format '{}'. Use 'claude-code' or 'mcpb'", other)
                    }
                };
                println!("Exported to {result}");
            }
            PluginCommands::Import { path, output_dir } => {
                let out = output_dir
                    .as_deref()
                    .unwrap_or_else(|| camino::Utf8Path::new("."));
                let format = skillrunner_core::plugin_import::detect_plugin_format(&path)
                    .ok_or_else(|| anyhow::anyhow!(
                        "Could not detect plugin format at '{}'. \
                         Expected a Claude Code plugin directory (with .claude-plugin/) or a .mcpb file.",
                        path
                    ))?;
                let result = match format {
                    skillrunner_core::plugin_import::ExternalPluginFormat::ClaudeCode => {
                        skillrunner_core::plugin_import::import_claude_code_plugin(&path, out)
                            .with_context(|| format!("failed to import Claude Code plugin at {path}"))?
                    }
                    skillrunner_core::plugin_import::ExternalPluginFormat::Mcpb => {
                        skillrunner_core::plugin_import::import_mcpb(&path, out)
                            .with_context(|| format!("failed to import .mcpb at {path}"))?
                    }
                };
                println!("Imported to {result}");
                println!("Next: skillrunner plugin validate {result}");
            }
            PluginCommands::Author { name, output_dir } => {
                let out = output_dir
                    .as_deref()
                    .unwrap_or_else(|| camino::Utf8Path::new("."));

                // Derive plugin ID: lowercase, non-alphanumeric chars become hyphens
                let plugin_id: String = name
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
                    .collect::<String>()
                    .split('-')
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("-");

                let plugin_dir = out.join(&plugin_id);
                std::fs::create_dir_all(plugin_dir.join("skills"))
                    .with_context(|| format!("failed to create {}", plugin_dir.join("skills")))?;
                std::fs::create_dir_all(plugin_dir.join("commands"))
                    .with_context(|| format!("failed to create {}", plugin_dir.join("commands")))?;

                let manifest = serde_json::json!({
                    "schema_version": "1.0",
                    "id": plugin_id,
                    "name": name,
                    "version": "0.1.0",
                    "publisher": "my-org",
                    "description": null,
                    "skills": [],
                    "mcp_servers": [],
                    "commands": []
                });
                let manifest_str = serde_json::to_string_pretty(&manifest)
                    .context("failed to serialize plugin.json")?;
                std::fs::write(plugin_dir.join("plugin.json"), &manifest_str)
                    .with_context(|| format!("failed to write {}", plugin_dir.join("plugin.json")))?;

                std::fs::write(plugin_dir.join("README.md"), format!("# {name}\n"))
                    .with_context(|| format!("failed to write {}", plugin_dir.join("README.md")))?;

                println!("Created plugin scaffold at {plugin_dir}");
                println!("Next: add skills to {plugin_dir}/skills/, MCP servers, or commands to {plugin_dir}/commands/");
                println!("      A plugin must contain at least one component to pass validation.");
                println!("      Then run: skillrunner plugin validate {plugin_dir}");
            }
        },
    }

    Ok(())
}

fn registry_url_from_env() -> Option<String> {
    std::env::var("SKILLCLUB_REGISTRY_URL").ok().filter(|s| !s.is_empty())
}

/// Resolve the effective registry URL using priority order:
/// 1. managed.json registry_url (IT override, already resolved before call)
/// 2. --registry-url CLI flag
/// 3. SKILLCLUB_REGISTRY_URL env var
fn resolve_registry_url(flag: Option<String>, managed_url: Option<&str>) -> Option<String> {
    managed_url
        .map(|s| s.to_string())
        .or(flag)
        .or_else(registry_url_from_env)
}

fn require_registry_url(flag: Option<String>, managed_url: Option<&str>) -> Result<String> {
    resolve_registry_url(flag, managed_url).ok_or_else(|| {
        anyhow::anyhow!("no registry URL configured; set SKILLCLUB_REGISTRY_URL or use --registry-url")
    })
}
