use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use rusqlite::Connection;
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
    updater::{
        check_skill_updates, install_from_registry, package_plugin, tar_gz_skill_source,
    },
    validator::validate_bundle,
};
use skillrunner_manifest::SkillPackage;
use skillrunner_mcp::{
    migration::{list_backups, migrate_existing_servers, restore_backup},
    server::{run_server, McpServerConfig},
    setup::{
        configure_client, detect_ai_clients, install_claude_skills, install_npx_claude_hook,
        install_npx_shell_wrapper, mark_first_run_offered,
    },
};

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
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
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
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
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
        /// VectorHawk registry URL to configure
        #[arg(long)]
        registry_url: Option<String>,
        /// Which client to configure (claude, cursor, all)
        #[arg(long, default_value = "all")]
        client: String,
        /// Non-interactive mode for automated installs
        #[arg(long)]
        auto: bool,
        /// Migrate existing MCP servers into backends.yaml without prompting (for Homebrew post_install)
        #[arg(long, conflicts_with = "no_migrate")]
        migrate_all: bool,
        /// Skip the migration step entirely
        #[arg(long, conflicts_with = "migrate_all")]
        no_migrate: bool,
    },
    /// Migrate existing MCP servers from AI client configs into backends.yaml
    Migrate,
    /// List available config backups and optionally restore one
    Restore {
        /// Path to the backup file to restore (if omitted, lists available backups)
        #[arg(long)]
        backup: Option<String>,
    },
    /// Show aggregator backend status (approved servers from registry)
    Backends {
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Manually trigger a skill sync (check for updates, lifecycle changes)
    Sync {
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Log in to the SkillClub registry
    Login {
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
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
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Show current authentication status
    Status {
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    /// Import a SKILL.md file and scaffold a skill bundle
    Import {
        path: Utf8PathBuf,
        /// Auto-accept metadata recommendations for missing fields
        #[arg(long)]
        accept_suggestions: bool,
        /// Skip metadata enrichment
        #[arg(long)]
        skip_metadata: bool,
    },
    /// Create a new skill with smart metadata recommendations
    Author {
        /// Skill name
        #[arg(long)]
        name: Option<String>,
        /// Path to a file containing the system prompt
        #[arg(long)]
        prompt_file: Option<Utf8PathBuf>,
        /// Auto-accept all metadata recommendations without prompting
        #[arg(long)]
        accept_suggestions: bool,
        /// Skip metadata analysis, use bare defaults
        #[arg(long)]
        skip_metadata: bool,
        /// Output directory (default: current directory)
        #[arg(long, default_value = ".")]
        output_dir: Utf8PathBuf,
    },
    Search {
        query: String,
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    Info {
        path: Utf8PathBuf,
    },
    /// Install a skill from a local path or from the registry by ID
    Install {
        /// Skill ID (for registry install) or local path to a skill directory
        skill_ref: String,
        /// Specific version to install from registry (default: latest)
        #[arg(long)]
        version: Option<String>,
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Publish a skill to the registry
    Publish {
        /// Path to the skill directory to publish
        path: Utf8PathBuf,
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Uninstall an installed skill
    Uninstall {
        skill_id: String,
    },
    List,
    Resolve {
        skill_id: String,
    },
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
        /// VectorHawk registry URL for policy fetch and auto-update (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    Validate {
        path: Utf8PathBuf,
    },
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
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Publish a plugin to the registry
    Publish {
        /// Path to the plugin directory
        path: Utf8PathBuf,
        /// VectorHawk registry URL (overrides VECTORHAWK_REGISTRY_URL)
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
    let is_mcp_serve = matches!(
        cli.command,
        Commands::Mcp {
            command: McpCommands::Serve { .. }
        }
    );
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
    let managed_registry_url: Option<String> =
        load_managed_config(&app.state).and_then(|c| c.registry_url);

    match cli.command {
        Commands::Doctor {
            ollama_url,
            registry_url,
        } => {
            println!("SkillRunner root: {}", app.state.root_dir);
            println!("State DB:         {}", app.state.db_path);
            println!("Status:           OK");

            let ollama = OllamaClient::new(&ollama_url, "");
            let health = ollama.health_check();
            if health.reachable {
                match ollama.list_models() {
                    Ok(models) => {
                        println!(
                            "Ollama:           OK ({} models available) at {}",
                            models.len(),
                            ollama_url
                        );
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
            let effective_url_for_backends =
                resolve_registry_url(None, managed_registry_url.as_deref());
            if let Some(url) = effective_url_for_backends {
                let reg = RegistryClient::new(&url);
                match mcp_governance::fetch_approved_servers(&app.state, &reg) {
                    Ok(resp) => {
                        let approved = resp
                            .servers
                            .iter()
                            .filter(|s| s.status != "blocked")
                            .count();
                        let blocked = resp
                            .servers
                            .iter()
                            .filter(|s| s.status == "blocked")
                            .count();
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
            AuthCommands::Login {
                registry_url,
                email,
                password,
            } => {
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
                auth::save_tokens(
                    &app.state,
                    &url,
                    &tokens.access_token,
                    &tokens.refresh_token,
                )?;

                let user = auth_client.me(&tokens.access_token)?;
                println!(
                    "Logged in as {} ({}) at {}",
                    user.display_name, user.email, url
                );
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
            McpCommands::Serve {
                registry_url,
                ollama_url,
                model,
            } => {
                let effective_url =
                    resolve_registry_url(registry_url, managed_registry_url.as_deref());
                let config = McpServerConfig {
                    registry_url: effective_url,
                    ollama_url,
                    model,
                };
                run_server(app.state, config)?;
            }
            McpCommands::Setup {
                registry_url,
                client,
                auto,
                migrate_all,
                no_migrate,
            } => {
                let effective_url =
                    resolve_registry_url(registry_url, managed_registry_url.as_deref());
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
                            println!(
                                "\n  Installed {} slash command(s): {}",
                                installed.len(),
                                installed
                                    .iter()
                                    .map(|s| format!("/{s}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
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
                            println!(
                                "    To activate: add {} to your PATH",
                                path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(&path)
                            );
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

                // ── Migration step ───────────────────────────────────────────
                if !no_migrate {
                    run_migration_step(&app.state, &clients, migrate_all, auto)?;
                }
            }
            McpCommands::Migrate => {
                let skillrunner_path = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.to_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "skillrunner".to_string());
                let clients = detect_ai_clients(&skillrunner_path);
                run_migration_step(&app.state, &clients, true, false)?;
            }
            McpCommands::Restore { backup } => match backup {
                Some(path) => {
                    restore_backup(std::path::Path::new(&path))
                        .with_context(|| format!("failed to restore backup {path}"))?;
                    println!("Restored backup: {path}");
                }
                None => {
                    let backups = list_backups(&app.state)?;
                    if backups.is_empty() {
                        println!("No backups found in {}/backups/", app.state.root_dir);
                    } else {
                        println!("Available backups:");
                        for b in &backups {
                            println!("  {} ({})", b.path.display(), b.original_path.display());
                        }
                        println!("\nTo restore: skillrunner mcp restore --backup <path>");
                    }
                }
            },
            McpCommands::Backends { registry_url } => {
                let url = resolve_registry_url(registry_url, managed_registry_url.as_deref());
                match url {
                    None => {
                        println!("No registry URL configured. Pass --registry-url or set VECTORHAWK_REGISTRY_URL.");
                    }
                    Some(url) => {
                        let registry = RegistryClient::new(&url);
                        match mcp_governance::fetch_approved_servers(&app.state, &registry) {
                            Ok(resp) => {
                                let approved: Vec<_> = resp
                                    .servers
                                    .iter()
                                    .filter(|s| s.status != "blocked")
                                    .collect();
                                let blocked: Vec<_> = resp
                                    .servers
                                    .iter()
                                    .filter(|s| s.status == "blocked")
                                    .collect();

                                println!("Approval mode:    {}", resp.approval_mode);
                                println!(
                                    "Backends:         {} approved, {} blocked",
                                    approved.len(),
                                    blocked.len()
                                );
                                println!();

                                if approved.is_empty() {
                                    println!("No approved backends. Ask your IT admin to approve servers via the SkillClub portal.");
                                } else {
                                    println!(
                                        "{:<25} {:<12} {:<12} {:<8} VISIBILITY",
                                        "NAME", "SERVER ID", "TRANSPORT", "PRIORITY"
                                    );
                                    println!("{}", "-".repeat(75));
                                    for s in &approved {
                                        let server_id = s.server_id.as_deref().unwrap_or(&s.name);
                                        let transport =
                                            s.transport_type.as_deref().unwrap_or("http");
                                        let priority = s
                                            .priority
                                            .map(|p| p.to_string())
                                            .unwrap_or_else(|| "50".to_string());
                                        let visibility =
                                            s.tool_visibility.as_deref().unwrap_or("all");
                                        println!(
                                            "{:<25} {:<12} {:<12} {:<8} {}",
                                            s.name, server_id, transport, priority, visibility
                                        );
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
            SkillCommands::Import {
                path,
                accept_suggestions,
                skip_metadata,
            } => {
                let bundle = import_skill_md(&path)?;
                println!("Imported skill: {}", bundle.id);
                println!("Output:         {}", bundle.output_dir);
                for f in &bundle.files {
                    println!("  wrote {f}");
                }

                if !skip_metadata {
                    let pkg = SkillPackage::load_from_dir(&bundle.output_dir)?;
                    let missing = detect_missing_metadata(&pkg);
                    if !missing.is_empty() {
                        println!("\nMissing recommended metadata: {}", missing.join(", "));

                        let should_enrich = if accept_suggestions {
                            true
                        } else {
                            print!("Run recommendation engine to fill these? [Y/n]: ");
                            std::io::Write::flush(&mut std::io::stdout())?;
                            let mut input = String::new();
                            std::io::stdin().read_line(&mut input)?;
                            let answer = input.trim().to_lowercase();
                            answer.is_empty() || answer == "y" || answer == "yes"
                        };

                        if should_enrich {
                            let description =
                                pkg.manifest.description.as_deref().unwrap_or("").to_string();
                            let skill_md_path = bundle.output_dir.join("SKILL.md");
                            let skill_md_content =
                                std::fs::read_to_string(&skill_md_path).with_context(|| {
                                    format!("failed to read {skill_md_path}")
                                })?;
                            let body = extract_skill_md_body(&skill_md_content);

                            let rec = skillrunner_core::recommend::recommend_from_prompt(
                                &pkg.manifest.name,
                                &description,
                                &body,
                            );

                            let rec = if accept_suggestions {
                                println!("Applying recommended metadata...");
                                rec
                            } else {
                                prompt_for_recommendations(rec)?
                            };

                            let enriched = build_enriched_skill_md(
                                &pkg.manifest.name,
                                &description,
                                &body,
                                &rec,
                            );
                            std::fs::write(&skill_md_path, enriched).with_context(|| {
                                format!("failed to write {skill_md_path}")
                            })?;
                            println!("\nUpdated SKILL.md with metadata recommendations.");
                        }
                    }
                }
            }
            SkillCommands::Author {
                name,
                prompt_file,
                accept_suggestions,
                skip_metadata,
                output_dir,
            } => {
                let skill_name = match name {
                    Some(n) => n,
                    None => {
                        print!("Skill name: ");
                        std::io::Write::flush(&mut std::io::stdout())?;
                        let mut input = String::new();
                        std::io::stdin().read_line(&mut input)?;
                        let trimmed = input.trim().to_string();
                        if trimmed.is_empty() {
                            anyhow::bail!("Skill name cannot be empty");
                        }
                        trimmed
                    }
                };

                let system_prompt = match prompt_file {
                    Some(ref path) => std::fs::read_to_string(path)
                        .with_context(|| format!("Failed to read prompt file: {path}"))?,
                    None => {
                        println!("Enter system prompt (end with Ctrl-D on a new line):");
                        let mut input = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
                        if input.trim().is_empty() {
                            anyhow::bail!("System prompt cannot be empty");
                        }
                        input
                    }
                };

                let description = format!("A skill that helps with {}", skill_name.to_lowercase());

                if skip_metadata {
                    let skill_md =
                        build_bare_skill_md(&skill_name, &description, system_prompt.trim());
                    let bundle = write_and_import_skill_md(&output_dir, &skill_md)?;
                    print_bundle_result(&bundle);
                } else {
                    let rec = skillrunner_core::recommend::recommend_from_prompt(
                        &skill_name,
                        &description,
                        system_prompt.trim(),
                    );

                    if accept_suggestions {
                        println!("\nUsing recommended metadata...");
                        let skill_md = build_enriched_skill_md(
                            &skill_name,
                            &description,
                            system_prompt.trim(),
                            &rec,
                        );
                        let bundle = write_and_import_skill_md(&output_dir, &skill_md)?;
                        print_bundle_result(&bundle);
                        print_recommendations_summary(&rec);
                    } else {
                        println!("\nAnalyzing prompt...\n");
                        let rec = prompt_for_recommendations(rec)?;
                        let skill_md = build_enriched_skill_md(
                            &skill_name,
                            &description,
                            system_prompt.trim(),
                            &rec,
                        );
                        let bundle = write_and_import_skill_md(&output_dir, &skill_md)?;
                        print_bundle_result(&bundle);
                    }
                }
            }
            SkillCommands::Search {
                query,
                registry_url,
            } => {
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
            SkillCommands::Install {
                skill_ref,
                version,
                registry_url,
            } => {
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
                let tokens = auth::load_tokens(&app.state, &url)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "not logged in; run `skillrunner auth login --registry-url {url}` first"
                    )
                })?;

                // Package the source tree for the compile endpoint (no local
                // validation — the registry does all validation server-side).
                let source_bytes = tar_gz_skill_source(&path)
                    .with_context(|| format!("failed to archive skill source at {path}"))?;
                println!(
                    "Archived {} ({} bytes) — uploading to registry compile endpoint",
                    path,
                    source_bytes.len()
                );

                let do_publish = |token: &str| -> Result<_> {
                    let registry = RegistryClient::new(&url).with_auth(token);
                    registry.compile_and_publish(source_bytes.clone())
                };

                let resp = match do_publish(&tokens.access_token) {
                    Ok(r) => r,
                    Err(e) => {
                        // Try token refresh once before surfacing the error.
                        let auth_client = AuthClient::new(&url);
                        match auth_client.refresh(&tokens.refresh_token) {
                            Ok(new_tokens) => {
                                auth::save_tokens(
                                    &app.state,
                                    &url,
                                    &new_tokens.access_token,
                                    &new_tokens.refresh_token,
                                )?;
                                do_publish(&new_tokens.access_token)?
                            }
                            Err(_) => return Err(e),
                        }
                    }
                };

                println!("Published successfully!");
                println!("  name:    {}", resp.frontmatter.name);
                if let Some(ver) = &resp.frontmatter.vh_version {
                    println!("  version: {ver}");
                }
                println!("  hash:    {}", resp.content_hash);
                for w in &resp.warnings {
                    println!("  warning: {w}");
                }
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
                let outcome = if let Some(url) =
                    resolve_registry_url(None, managed_registry_url.as_deref())
                {
                    let policy_client =
                        HttpPolicyClient::new(RegistryClient::new(&url), &app.state);
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
            SkillCommands::Run {
                skill_id,
                input,
                ollama_url,
                model,
                stub,
                registry_url,
            } => {
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
                    resolve_model(&probe, &model).with_context(|| {
                        format!("failed to resolve model '{model}' from Ollama at {ollama_url}")
                    })?
                };

                let ollama = OllamaClient::new(ollama_url, effective_model);
                let model_client: Option<&dyn skillrunner_core::model::ModelClient> =
                    if stub { None } else { Some(&ollama) };

                let effective_url =
                    resolve_registry_url(registry_url, managed_registry_url.as_deref());
                let result = if !stub {
                    if let Some(url) = effective_url {
                        let registry = RegistryClient::new(&url);
                        let http_policy =
                            HttpPolicyClient::new(RegistryClient::new(&url), &app.state);
                        run_skill(
                            &app.state,
                            &http_policy,
                            &skill_id,
                            &input_json,
                            model_client,
                            Some(&registry),
                        )?
                    } else {
                        let policy_client = MockPolicyClient::new();
                        run_skill(
                            &app.state,
                            &policy_client,
                            &skill_id,
                            &input_json,
                            model_client,
                            None,
                        )?
                    }
                } else {
                    // Stub mode: skip registry policy, use allow-all mock
                    let policy_client = MockPolicyClient::new();
                    run_skill(
                        &app.state,
                        &policy_client,
                        &skill_id,
                        &input_json,
                        model_client,
                        None,
                    )?
                };
                println!("Running {}@{}", result.skill_id, result.version);
                for step in &result.steps {
                    println!(
                        "  [{}] {}: {}",
                        step.step_type.to_uppercase(),
                        step.id,
                        step.note
                    );
                    if let Some(output) = &step.output {
                        println!(
                            "       output: {}",
                            serde_json::to_string_pretty(output).unwrap_or_default()
                        );
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
                println!(
                    "Installing plugin '{}' v{}",
                    pkg.manifest.name, pkg.manifest.version
                );

                let result = skillrunner_core::plugin::install_plugin_from_dir(&app.state, &path)?;

                if !result.components.skill_ids.is_empty() {
                    println!("  Skills: {}", result.components.skill_ids.join(", "));
                }
                if !result.components.mcp_server_names.is_empty() {
                    println!(
                        "  MCP servers (pending approval): {}",
                        result.components.mcp_server_names.join(", ")
                    );
                }
                if !result.components.command_names.is_empty() {
                    println!(
                        "  Commands: {}",
                        result
                            .components
                            .command_names
                            .iter()
                            .map(|c| format!("/{c}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
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
                            println!(
                                "    mcp servers: {}",
                                p.components.mcp_server_names.join(", ")
                            );
                        }
                        if !p.components.command_names.is_empty() {
                            println!(
                                "    commands: {}",
                                p.components
                                    .command_names
                                    .iter()
                                    .map(|c| format!("/{c}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
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
                            println!(
                                "Cmds:   {}",
                                p.components
                                    .command_names
                                    .iter()
                                    .map(|c| format!("/{c}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                        }
                    }
                    None => println!("Plugin '{plugin_id}' is not installed."),
                }
            }
            PluginCommands::Search {
                query,
                registry_url,
            } => {
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

                let token = auth::load_tokens(&app.state, &url)?.ok_or_else(|| {
                    anyhow::anyhow!("not logged in; run `skillrunner auth login` first")
                })?;

                let registry = RegistryClient::new(&url).with_auth(&token.access_token);
                let resp = registry
                    .publish_plugin(&archive_path)
                    .with_context(|| "failed to publish plugin to registry")?;

                let _ = std::fs::remove_file(&archive_path);

                let slug = resp
                    .get("slug")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let version = resp
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                println!("Published plugin {slug}@{version} to registry.");
            }
            PluginCommands::Export {
                path,
                format,
                output_dir,
            } => {
                let out = output_dir
                    .as_deref()
                    .unwrap_or_else(|| camino::Utf8Path::new("."));
                let result = match format.as_str() {
                    "claude-code" => {
                        skillrunner_core::plugin_export::export_claude_code(&path, out)
                            .with_context(|| {
                                format!("failed to export plugin at {path} as claude-code")
                            })?
                    }
                    "mcpb" => skillrunner_core::plugin_export::export_mcpb(&path, out)
                        .with_context(|| format!("failed to export plugin at {path} as mcpb"))?,
                    other => {
                        anyhow::bail!(
                            "unsupported format '{}'. Use 'claude-code' or 'mcpb'",
                            other
                        )
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
                            .with_context(|| {
                                format!("failed to import Claude Code plugin at {path}")
                            })?
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
                std::fs::write(plugin_dir.join("plugin.json"), &manifest_str).with_context(
                    || format!("failed to write {}", plugin_dir.join("plugin.json")),
                )?;

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

/// Build a SKILL.md with bare defaults (no vh_* metadata beyond minimum).
fn build_bare_skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\nlicense: Apache-2.0\n---\n\n{body}\n")
}

/// Build a SKILL.md with full vh_* metadata from recommendations.
fn build_enriched_skill_md(
    name: &str,
    description: &str,
    body: &str,
    rec: &skillrunner_core::recommend::Recommendations,
) -> String {
    let mut fm = format!("---\nname: {name}\ndescription: {description}\nlicense: Apache-2.0\n");

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

    fm.push_str("---\n\n");
    fm.push_str(body);
    fm.push('\n');
    fm
}

/// Write a SKILL.md to the output directory and run import_skill_md.
fn write_and_import_skill_md(
    output_dir: &Utf8PathBuf,
    skill_md_content: &str,
) -> Result<skillrunner_core::import::ScaffoldedBundle> {
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("failed to create directory {output_dir}"))?;
    let skill_md_path = output_dir.join("SKILL.md");
    std::fs::write(&skill_md_path, skill_md_content)
        .with_context(|| format!("failed to write {skill_md_path}"))?;
    import_skill_md(&skill_md_path).context("Failed to scaffold skill bundle")
}

fn print_bundle_result(bundle: &skillrunner_core::import::ScaffoldedBundle) {
    println!("\nCreated skill: {}", bundle.id);
    println!("Output:        {}", bundle.output_dir);
    for f in &bundle.files {
        println!("  wrote {f}");
    }
}

fn print_recommendations_summary(rec: &skillrunner_core::recommend::Recommendations) {
    println!("\nApplied metadata:");
    if !rec.triggers.is_empty() {
        println!("  triggers: {}", rec.triggers.join(", "));
    }
    println!(
        "  network: {}, filesystem: {}, clipboard: {}",
        rec.permissions.network, rec.permissions.filesystem, rec.permissions.clipboard
    );
    println!(
        "  model: {}B min, recommended: {}, fallback: {}",
        rec.model.min_params_b,
        rec.model.recommended.join("/"),
        rec.model.fallback
    );
    println!(
        "  timeout: {}ms, memory: {}MB, sandbox: {}",
        rec.execution.timeout_ms, rec.execution.memory_mb, rec.execution.sandbox
    );
}

/// Detect which vh_* metadata fields are missing/empty.
fn detect_missing_metadata(pkg: &SkillPackage) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if pkg.manifest.triggers.is_empty() {
        missing.push("vh_triggers");
    }
    if pkg.manifest.model_requirements.is_none() {
        missing.push("vh_model");
    }
    missing
}

/// Extract the body (after YAML frontmatter) from a SKILL.md string.
fn extract_skill_md_body(content: &str) -> String {
    if let Some(after_open) = content.strip_prefix("---\n") {
        if let Some(close_pos) = after_open.find("\n---\n") {
            return after_open[close_pos + 5..].trim().to_string();
        }
    }
    content.to_string()
}

/// Interactive prompt loop: show each recommendation group and ask Y/n.
fn prompt_for_recommendations(
    mut rec: skillrunner_core::recommend::Recommendations,
) -> Result<skillrunner_core::recommend::Recommendations> {
    use std::io::Write;

    // Triggers
    println!("Recommended triggers:");
    for (i, t) in rec.triggers.iter().enumerate() {
        println!("  {}. {}", i + 1, t);
    }
    print!("Accept triggers? [Y/n]: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    if input.trim().eq_ignore_ascii_case("n") {
        rec.triggers.clear();
        println!("  (triggers skipped)");
    }

    // Permissions
    println!("\nRecommended permissions:");
    println!("  network: {}", rec.permissions.network);
    println!("  filesystem: {}", rec.permissions.filesystem);
    println!("  clipboard: {}", rec.permissions.clipboard);
    print!("Accept permissions? [Y/n]: ");
    std::io::stdout().flush()?;
    input.clear();
    std::io::stdin().read_line(&mut input)?;
    if input.trim().eq_ignore_ascii_case("n") {
        rec.permissions.network = "none".to_string();
        rec.permissions.filesystem = "none".to_string();
        rec.permissions.clipboard = "none".to_string();
        println!("  (reset to none/none/none)");
    }

    // Model
    println!("\nRecommended model:");
    println!("  min_params_b: {}", rec.model.min_params_b);
    println!("  recommended: {}", rec.model.recommended.join(", "));
    println!("  fallback: {}", rec.model.fallback);
    print!("Accept model settings? [Y/n]: ");
    std::io::stdout().flush()?;
    input.clear();
    std::io::stdin().read_line(&mut input)?;
    if input.trim().eq_ignore_ascii_case("n") {
        rec.model.min_params_b = 1.0;
        rec.model.recommended = vec!["gemma3:4b".to_string()];
        rec.model.fallback = "error".to_string();
        println!("  (reset to defaults)");
    }

    // Execution
    println!("\nRecommended execution:");
    println!("  timeout_ms: {}", rec.execution.timeout_ms);
    println!("  memory_mb: {}", rec.execution.memory_mb);
    println!("  sandbox: {}", rec.execution.sandbox);
    print!("Accept execution settings? [Y/n]: ");
    std::io::stdout().flush()?;
    input.clear();
    std::io::stdin().read_line(&mut input)?;
    if input.trim().eq_ignore_ascii_case("n") {
        rec.execution.timeout_ms = 30_000;
        rec.execution.memory_mb = 256;
        rec.execution.sandbox = "strict".to_string();
        println!("  (reset to defaults)");
    }

    Ok(rec)
}

/// Default registry URL for release builds.
const DEFAULT_REGISTRY_URL: &str = "https://app.vectorhawk.ai";

fn registry_url_from_env() -> Option<String> {
    std::env::var("VECTORHAWK_REGISTRY_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Resolve the effective registry URL using priority order:
/// 1. managed.json registry_url (IT override, already resolved before call)
/// 2. --registry-url CLI flag
/// 3. VECTORHAWK_REGISTRY_URL env var
/// 4. Built-in default (https://app.vectorhawk.ai)
fn resolve_registry_url(flag: Option<String>, managed_url: Option<&str>) -> Option<String> {
    managed_url
        .map(|s| s.to_string())
        .or(flag)
        .or_else(registry_url_from_env)
        .or_else(|| Some(DEFAULT_REGISTRY_URL.to_string()))
}

fn require_registry_url(flag: Option<String>, managed_url: Option<&str>) -> Result<String> {
    resolve_registry_url(flag, managed_url).ok_or_else(|| {
        anyhow::anyhow!(
            "no registry URL configured; set VECTORHAWK_REGISTRY_URL or use --registry-url"
        )
    })
}

/// Run the migration step: scan client configs for non-SkillRunner MCP servers
/// and move them into `backends.yaml`.
///
/// - `migrate_all` true  → run silently (no prompt, for `--migrate-all` and Homebrew post_install)
/// - `migrate_all` false → interactive prompt (Y/s/N) unless `auto` is also true (non-TTY)
/// - `auto` true         → skip the interactive prompt and don't migrate (conservative default
///   for `--auto` without an explicit `--migrate-all`)
fn run_migration_step(
    state: &skillrunner_core::state::AppState,
    clients: &[skillrunner_mcp::setup::ClientConfig],
    migrate_all: bool,
    auto: bool,
) -> Result<()> {
    // Peek at what would be migrated so we can show an informative prompt.
    use skillrunner_mcp::setup::detect_unmanaged_servers;
    let unmanaged = detect_unmanaged_servers();

    if unmanaged.is_empty() {
        if !auto && !migrate_all {
            println!("\nNo existing MCP servers found to migrate.");
        }
        return Ok(());
    }

    let should_migrate = if migrate_all {
        // Non-interactive path: always migrate.
        true
    } else if auto {
        // --auto without --migrate-all: skip migration to avoid surprises in
        // non-interactive environments where we can't read stdin safely.
        false
    } else {
        // Interactive path: prompt the user.
        println!(
            "\nFound {} existing MCP server(s) in your AI client configs:",
            unmanaged.len()
        );
        for s in &unmanaged {
            println!("  {} ({})", s.server_name, s.client_name);
        }
        println!("\nMigrate these into SkillRunner's aggregator (backends.yaml)?");
        println!("  [Y]es — migrate and remove from client configs (recommended)");
        println!("  [S]kip — leave them as-is (you can run `skillrunner mcp migrate` later)");
        print!("  Choice [Y/s]: ");
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim().to_lowercase();
        trimmed.is_empty() || trimmed == "y" || trimmed == "yes"
    };

    if !should_migrate {
        if !auto {
            println!("  Skipped. Run `skillrunner mcp migrate` at any time to migrate.");
        }
        return Ok(());
    }

    match migrate_existing_servers(state, clients) {
        Ok(report) => {
            if !auto || migrate_all {
                if report.migrated.is_empty() {
                    println!("  No new servers to migrate (already in backends.yaml).");
                } else {
                    println!(
                        "\n  Migrated {} server(s) to backends.yaml:",
                        report.migrated.len()
                    );
                    for m in &report.migrated {
                        println!("    {} ({})", m.server_name, m.transport);
                    }
                    if !report.backups.is_empty() {
                        println!(
                            "  Original configs backed up to {}/backups/",
                            state.root_dir
                        );
                        println!("  To restore: skillrunner mcp restore");
                    }
                    if !report.skipped.is_empty() {
                        println!(
                            "  {} server(s) skipped (disabled or duplicate).",
                            report.skipped.len()
                        );
                    }
                }
            }
        }
        Err(e) => {
            if !auto {
                println!("  Migration warning: {e}");
            }
        }
    }
    Ok(())
}
