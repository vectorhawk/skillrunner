use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use skillrunner_core::{
    app::SkillRunnerApp,
    executor::run_skill,
    import::import_skill_md,
    install::install_unpacked_skill,
    ollama::OllamaClient,
    policy::MockPolicyClient,
    registry::{HttpPolicyClient, RegistryClient},
    resolver::{resolve_skill, ResolveOutcome},
    validator::validate_bundle,
};
use skillrunner_manifest::SkillPackage;
use rusqlite::Connection;

#[derive(Parser)]
#[command(name = "skillrunner")]
#[command(about = "SkillRunner — the local runtime for SkillClub skills", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Doctor,
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    Import { path: Utf8PathBuf },
    Info { path: Utf8PathBuf },
    Install { path: Utf8PathBuf },
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
        /// SkillClub registry URL for policy fetch and auto-update
        #[arg(long)]
        registry_url: Option<String>,
    },
    Validate { path: Utf8PathBuf },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cli = Cli::parse();
    let app = SkillRunnerApp::bootstrap()?;

    match cli.command {
        Commands::Doctor => {
            println!("SkillRunner root: {}", app.state.root_dir);
            println!("State DB:         {}", app.state.db_path);
            println!("Status:           OK");
        }
        Commands::Skill { command } => match command {
            SkillCommands::Import { path } => {
                let bundle = import_skill_md(&path)?;
                println!("Imported skill: {}", bundle.id);
                println!("Output:         {}", bundle.output_dir);
                for f in &bundle.files {
                    println!("  wrote {f}");
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
            SkillCommands::Install { path } => {
                let skill = SkillPackage::load_from_dir(path)?;
                install_unpacked_skill(&app.state, &skill)?;
                println!("Installed {}@{}", skill.manifest.id, skill.manifest.version);
            }
            SkillCommands::Resolve { skill_id } => {
                let policy_client = MockPolicyClient::new();
                let outcome = resolve_skill(&app.state, &policy_client, &skill_id)?;
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

                let ollama = OllamaClient::new(ollama_url, model);
                let model_client: Option<&dyn skillrunner_core::model::ModelClient> = if stub {
                    None
                } else {
                    Some(&ollama)
                };

                // When --registry-url is provided, use the HTTP policy client
                // (with caching + offline grace) and enable silent auto-update.
                let result = if let Some(url) = registry_url {
                    let registry = RegistryClient::new(&url);
                    let http_policy = HttpPolicyClient::new(RegistryClient::new(url), &app.state);
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
        },
    }

    Ok(())
}
