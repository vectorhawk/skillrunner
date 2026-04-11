use crate::{
    model::{ModelClient, ModelRequest},
    policy::PolicyClient,
    resolver::{resolve_skill, ResolveOutcome},
    state::AppState,
};
#[cfg(feature = "registry")]
use crate::{registry::RegistryClient, updater::auto_update_if_needed};

/// Stub type used when the `registry` feature is disabled.
/// Allows `run_skill` to keep its signature without depending on `RegistryClient`.
#[cfg(not(feature = "registry"))]
pub struct RegistryClient;
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rusqlite::Connection;
use skillrunner_manifest::{PromptSource, SkillPackage, WorkflowStep};
use std::collections::HashMap;
use std::fs;

// ── Public result types ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct StepResult {
    pub id: String,
    pub step_type: String,
    /// Human-readable summary of what happened.
    pub note: String,
    /// Parsed output produced by the step (None for stubs / non-llm steps).
    pub output: Option<serde_json::Value>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub latency_ms: Option<u64>,
}

#[derive(Debug)]
pub struct RunResult {
    pub skill_id: String,
    pub version: String,
    pub steps: Vec<StepResult>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_latency_ms: u64,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Resolve, load, validate input, and execute a skill's workflow.
///
/// When `model_client` is `Some`, `llm` steps are sent to the model. When
/// `None`, every step is stub-executed (no network calls, useful for tests
/// and dry-runs).
///
/// When `registry_client` is `Some`, the runner silently updates the skill
/// to the registry's `target_version` before execution if the installed
/// version is below `minimum_allowed_version`.
pub fn run_skill(
    state: &AppState,
    policy_client: &dyn PolicyClient,
    skill_id: &str,
    input: &serde_json::Value,
    model_client: Option<&dyn ModelClient>,
    registry_client: Option<&RegistryClient>,
) -> Result<RunResult> {
    let wall_start = std::time::Instant::now();

    // 0. Silent auto-update if registry is available and version is stale.
    #[cfg(feature = "registry")]
    if let Some(registry) = registry_client {
        let policy = policy_client.fetch_policy(skill_id)?;
        auto_update_if_needed(state, registry, skill_id, &policy)?;
    }
    #[cfg(not(feature = "registry"))]
    let _ = registry_client;

    // 1. Resolve → get version and install path.
    let outcome = resolve_skill(state, policy_client, skill_id)?;
    let (version, install_path) = match outcome {
        ResolveOutcome::Active {
            version,
            install_path,
            ..
        } => (version, install_path),
        ResolveOutcome::NotInstalled { skill_id } => {
            anyhow::bail!("skill '{}' is not installed", skill_id)
        }
        ResolveOutcome::Blocked { skill_id, reason } => {
            anyhow::bail!("skill '{}' is blocked: {}", skill_id, reason)
        }
    };

    // 2. Load the skill package from the active install path.
    let pkg_path = Utf8PathBuf::from(&install_path);
    let pkg = SkillPackage::load_from_dir(&pkg_path)
        .with_context(|| format!("failed to load skill package at {pkg_path}"))?;

    // 3. Validate input against inputs_schema (uses default pass-through schema when absent).
    validate_input_against_schema(&pkg.manifest.inputs_schema_or_default(), input)?;

    // 4. Warn if model requirements are specified but may not be met.
    if let (Some(client), Some(reqs)) = (model_client, &pkg.manifest.model_requirements) {
        check_model_requirements(reqs, client);
    }

    // 5. Execute each workflow step, threading outputs forward.
    let mut steps: Vec<StepResult> = Vec::new();
    let mut step_outputs: HashMap<String, serde_json::Value> = HashMap::new();
    for step in &pkg.workflow.steps {
        let result = execute_step(&pkg, step, input, &step_outputs, model_client)?;
        if let Some(out) = &result.output {
            step_outputs.insert(result.id.clone(), out.clone());
        }
        steps.push(result);
    }

    let total_latency_ms = wall_start.elapsed().as_millis() as u64;
    let total_prompt_tokens: u64 = steps.iter().filter_map(|s| s.prompt_tokens).sum();
    let total_completion_tokens: u64 = steps.iter().filter_map(|s| s.completion_tokens).sum();

    // 5. Record execution history.
    record_execution(
        state,
        skill_id,
        &version,
        total_prompt_tokens,
        total_completion_tokens,
        total_latency_ms,
    )?;

    Ok(RunResult {
        skill_id: skill_id.to_string(),
        version,
        steps,
        total_prompt_tokens,
        total_completion_tokens,
        total_latency_ms,
    })
}

// ── Step dispatch ─────────────────────────────────────────────────────────────

fn execute_step(
    pkg: &SkillPackage,
    step: &WorkflowStep,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
    model_client: Option<&dyn ModelClient>,
) -> Result<StepResult> {
    match step {
        WorkflowStep::Tool { id, tool, input } => execute_tool_step(id, tool, input, run_input),
        WorkflowStep::Llm {
            id,
            prompt,
            inputs,
            output_schema,
        } => match model_client {
            Some(client) => execute_llm_step(
                pkg,
                LlmStepParams {
                    id,
                    prompt_source: prompt,
                    step_inputs: inputs,
                    output_schema_rel: output_schema.as_deref(),
                    run_input,
                    step_outputs,
                    client,
                },
            ),
            None => Ok(stub_step(step)),
        },
        WorkflowStep::Transform { id, op, input } => {
            execute_transform_step(id, op, input, run_input, step_outputs)
        }
        WorkflowStep::Validate { id, schema, input } => {
            execute_validate_step(pkg, id, schema, input, run_input, step_outputs)
        }
    }
}

// ── Tool step ────────────────────────────────────────────────────────────────

fn execute_tool_step(
    id: &str,
    tool: &str,
    input: &serde_yaml::Value,
    run_input: &serde_json::Value,
) -> Result<StepResult> {
    match tool {
        "extract_text" => {
            let field = input.as_str().ok_or_else(|| {
                anyhow::anyhow!("tool step '{id}': extract_text input must be a field name string")
            })?;
            let text = match run_input.get(field) {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => anyhow::bail!("tool step '{id}': input field '{field}' not found"),
            };
            Ok(StepResult {
                id: id.to_string(),
                step_type: "tool".to_string(),
                note: format!(
                    "extract_text: extracted field '{field}' ({} chars)",
                    text.len()
                ),
                output: Some(serde_json::Value::String(text)),
                prompt_tokens: None,
                completion_tokens: None,
                latency_ms: None,
            })
        }
        other => anyhow::bail!("tool step '{id}': unknown built-in tool '{other}'"),
    }
}

// ── Transform step ───────────────────────────────────────────────────────────

fn execute_transform_step(
    id: &str,
    op: &str,
    input: &serde_yaml::Value,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
) -> Result<StepResult> {
    let ref_str = input.as_str().ok_or_else(|| {
        anyhow::anyhow!("transform step '{id}': input must be a reference string")
    })?;
    let resolved = resolve_ref(ref_str, run_input, step_outputs);

    let output = match op {
        "json_parse" => serde_json::from_str(&resolved)
            .map_err(|e| anyhow::anyhow!("transform step '{id}': json_parse failed: {e}"))?,
        "to_string" => serde_json::Value::String(resolved.clone()),
        "to_uppercase" => serde_json::Value::String(resolved.to_uppercase()),
        "to_lowercase" => serde_json::Value::String(resolved.to_lowercase()),
        "trim" => serde_json::Value::String(resolved.trim().to_string()),
        other => anyhow::bail!("transform step '{id}': unknown op '{other}'"),
    };

    Ok(StepResult {
        id: id.to_string(),
        step_type: "transform".to_string(),
        note: format!("transform op '{op}' applied"),
        output: Some(output),
        prompt_tokens: None,
        completion_tokens: None,
        latency_ms: None,
    })
}

// ── Validate step ─────────────────────────────────────────────────────────────

fn execute_validate_step(
    pkg: &SkillPackage,
    id: &str,
    schema_rel: &str,
    input: &serde_yaml::Value,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
) -> Result<StepResult> {
    let ref_str = input
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("validate step '{id}': input must be a reference string"))?;
    let resolved_str = resolve_ref(ref_str, run_input, step_outputs);

    // Try to parse the resolved string as JSON; if it's already a step output
    // value it will be a valid JSON string representation.
    let value: serde_json::Value = serde_json::from_str(&resolved_str)
        .unwrap_or(serde_json::Value::String(resolved_str.clone()));

    validate_output(&pkg.root, schema_rel, &value)
        .with_context(|| format!("validate step '{id}' failed schema check"))?;

    Ok(StepResult {
        id: id.to_string(),
        step_type: "validate".to_string(),
        note: format!("validated against '{schema_rel}': ok"),
        output: Some(value),
        prompt_tokens: None,
        completion_tokens: None,
        latency_ms: None,
    })
}

// ── LLM step ─────────────────────────────────────────────────────────────────

struct LlmStepParams<'a> {
    id: &'a str,
    prompt_source: &'a PromptSource,
    step_inputs: &'a Option<serde_yaml::Value>,
    output_schema_rel: Option<&'a str>,
    run_input: &'a serde_json::Value,
    step_outputs: &'a HashMap<String, serde_json::Value>,
    client: &'a dyn ModelClient,
}

fn execute_llm_step(pkg: &SkillPackage, p: LlmStepParams<'_>) -> Result<StepResult> {
    let LlmStepParams {
        id,
        prompt_source,
        step_inputs,
        output_schema_rel,
        run_input,
        step_outputs,
        client,
    } = p;
    // Resolve the system prompt: inline body used directly, file path read from disk.
    let system_prompt = match prompt_source {
        PromptSource::Inline(body) => body.clone(),
        PromptSource::File(rel_path) => {
            let prompt_path = pkg.root.join(rel_path.as_str());
            fs::read_to_string(&prompt_path)
                .with_context(|| format!("failed to read prompt file {prompt_path}"))?
        }
    };

    // Resolve step inputs → user message string.
    let user_message = resolve_inputs(step_inputs, run_input, step_outputs);

    let request = ModelRequest {
        system_prompt,
        user_message,
        json_output: output_schema_rel.is_some(),
    };

    let response = client
        .generate(request)
        .with_context(|| format!("LLM call failed for step '{id}'"))?;

    // Parse model output.
    let output: Option<serde_json::Value> = if output_schema_rel.is_some() {
        serde_json::from_str(&response.text)
            .ok()
            .or_else(|| Some(serde_json::Value::String(response.text.clone())))
    } else {
        Some(serde_json::Value::String(response.text.clone()))
    };

    // Validate output against schema when present.
    if let (Some(schema_rel), Some(output_val)) = (output_schema_rel, &output) {
        validate_output(&pkg.root, schema_rel, output_val)
            .with_context(|| format!("step '{id}' output failed schema validation"))?;
    }

    Ok(StepResult {
        id: id.to_string(),
        step_type: "llm".to_string(),
        note: format!(
            "completed in {}ms ({} prompt + {} completion tokens)",
            response.latency_ms, response.prompt_tokens, response.completion_tokens
        ),
        output,
        prompt_tokens: Some(response.prompt_tokens),
        completion_tokens: Some(response.completion_tokens),
        latency_ms: Some(response.latency_ms),
    })
}

// ── Input resolution ──────────────────────────────────────────────────────────

/// Convert a step's `inputs` YAML mapping into a plain-text user message by
/// resolving `input.<field>` and `<step_id>.output` references.
fn resolve_inputs(
    step_inputs: &Option<serde_yaml::Value>,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
) -> String {
    let Some(inputs) = step_inputs else {
        return String::new();
    };
    let Some(mapping) = inputs.as_mapping() else {
        return String::new();
    };

    let mut parts = Vec::new();
    for (key, value) in mapping {
        let key_str = key.as_str().unwrap_or_default();
        let ref_str = value.as_str().unwrap_or_default();
        let resolved = resolve_ref(ref_str, run_input, step_outputs);
        parts.push(format!("{key_str}: {resolved}"));
    }
    parts.join("\n")
}

/// Resolve a reference to its string value.
///
/// Supports:
/// - `input.<field>` — field from the run's input JSON
/// - `<step_id>.output` — output of a previously executed step
fn resolve_ref(
    ref_str: &str,
    run_input: &serde_json::Value,
    step_outputs: &HashMap<String, serde_json::Value>,
) -> String {
    if let Some(field) = ref_str.strip_prefix("input.") {
        if let Some(val) = run_input.get(field) {
            return match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
    }
    if let Some(step_id) = ref_str.strip_suffix(".output") {
        if let Some(val) = step_outputs.get(step_id) {
            return match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
    }
    ref_str.to_string()
}

// ── Stub execution ────────────────────────────────────────────────────────────

fn stub_step(step: &WorkflowStep) -> StepResult {
    // Only Llm steps reach this path (when model_client is None).
    // Tool, Transform, and Validate are always dispatched to real implementations.
    let (id, step_type, note) = match step {
        WorkflowStep::Llm { id, prompt, .. } => {
            let prompt_desc = match prompt {
                PromptSource::File(path) => format!("file '{path}'"),
                PromptSource::Inline(_) => "inline body".to_string(),
            };
            (
                id.clone(),
                "llm",
                format!("LLM call with prompt {prompt_desc} — stub, no model invoked"),
            )
        }
        WorkflowStep::Tool { id, tool, .. } => {
            (id.clone(), "tool", format!("built-in tool '{tool}' — stub"))
        }
        WorkflowStep::Transform { id, op, .. } => (
            id.clone(),
            "transform",
            format!("transform op '{op}' — stub"),
        ),
        WorkflowStep::Validate { id, schema, .. } => (
            id.clone(),
            "validate",
            format!("schema validation '{schema}' — stub"),
        ),
    };
    StepResult {
        id,
        step_type: step_type.to_string(),
        note,
        output: None,
        prompt_tokens: None,
        completion_tokens: None,
        latency_ms: None,
    }
}

// ── Schema validation helpers ─────────────────────────────────────────────────

/// Validate `input` against an already-parsed JSON Schema value.
/// The schema comes from `Manifest::inputs_schema_or_default()`.
fn validate_input_against_schema(
    schema_json: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<()> {
    let validator = jsonschema::JSONSchema::compile(schema_json)
        .map_err(|e| anyhow::anyhow!("inputs_schema is not a valid JSON Schema: {e}"))?;

    if !validator.is_valid(input) {
        anyhow::bail!("input failed validation against inputs_schema");
    }

    Ok(())
}

fn validate_output(
    pkg_root: &Utf8PathBuf,
    schema_rel: &str,
    output: &serde_json::Value,
) -> Result<()> {
    let schema_path = pkg_root.join(schema_rel);
    let schema_text = fs::read_to_string(&schema_path)
        .with_context(|| format!("failed to read output schema {schema_path}"))?;
    let schema_json: serde_json::Value = serde_json::from_str(&schema_text)
        .with_context(|| format!("{schema_rel} is not valid JSON"))?;

    let validator = jsonschema::JSONSchema::compile(&schema_json)
        .map_err(|e| anyhow::anyhow!("{schema_rel} is not a valid JSON Schema: {e}"))?;

    if !validator.is_valid(output) {
        anyhow::bail!("output failed validation against {schema_rel}");
    }

    Ok(())
}

// ── Execution history ─────────────────────────────────────────────────────────

fn record_execution(
    state: &AppState,
    skill_id: &str,
    version: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    latency_ms: u64,
) -> Result<()> {
    let conn =
        Connection::open(&state.db_path).context("failed to open state DB to record execution")?;
    conn.execute(
        "INSERT INTO execution_history (skill_id, version, status, prompt_tokens, completion_tokens, latency_ms)
         VALUES (?1, ?2, 'completed', ?3, ?4, ?5)",
        rusqlite::params![skill_id, version, prompt_tokens, completion_tokens, latency_ms],
    )
    .context("failed to insert execution_history row")?;
    Ok(())
}

// ── Model requirements check ──────────────────────────────────────────────────

fn check_model_requirements(
    reqs: &skillrunner_manifest::ModelRequirements,
    _client: &dyn ModelClient,
) {
    // Advisory checks — log relevant requirements, don't block execution.
    if let Some(min_params) = reqs.min_params_b {
        tracing::info!(
            "skill requires min_params_b={min_params}B — cannot verify locally, proceeding"
        );
    }
    if !reqs.recommended.is_empty() {
        tracing::info!("skill recommended models: {:?}", reqs.recommended);
    }
    if let Some(fallback) = reqs.fallback {
        tracing::info!("skill model fallback: {fallback:?}");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        install::install_unpacked_skill, model::MockModelClient, policy::MockPolicyClient,
        state::AppState,
    };
    use camino::Utf8PathBuf;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("forge-tests-executor-{label}-{nanos}")),
        )
        .unwrap()
    }

    fn write_skill_bundle(root: &Utf8PathBuf, input_schema_json: &str) {
        fs::create_dir_all(root.join("prompts")).unwrap();
        // Embed the input schema inline in the SKILL.md frontmatter as a JSON
        // string that serde_yaml will parse. For the simple cases the tests use
        // this is always a valid JSON object literal.
        let input_schema_yaml = format!(
            "  inputs: {input_schema_json}\n  outputs: {{\"type\": \"object\"}}"
        );
        let skill_md = format!(
            "---\nname: Test Skill\ndescription: A test skill.\nlicense: MIT\nvh_version: 0.1.0\nvh_publisher: skillclub\nvh_permissions:\n  filesystem: none\n  network: none\n  clipboard: none\nvh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\nvh_schemas:\n{input_schema_yaml}\nvh_workflow_ref: ./workflow.yaml\n---\n\nDo the thing.\n"
        );
        fs::write(root.join("SKILL.md"), skill_md).unwrap();
        fs::write(
            root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .unwrap();
        fs::write(root.join("prompts/system.txt"), "Do the thing.").unwrap();
    }

    #[test]
    fn run_executes_steps_and_returns_results_for_installed_skill() {
        let state_root = temp_root("run-ok");
        let skill_root = temp_root("run-ok-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "{}");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let client = MockPolicyClient::new();
        let result = run_skill(
            &state,
            &client,
            "test-skill",
            &serde_json::json!({}),
            None,
            None,
        )
        .unwrap();

        assert_eq!(result.skill_id, "test-skill");
        assert_eq!(result.steps.len(), 1);
        assert_eq!(result.steps[0].id, "run");
        assert_eq!(result.steps[0].step_type, "llm");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn run_errors_when_skill_is_not_installed() {
        let state_root = temp_root("run-not-installed");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let client = MockPolicyClient::new();
        let err = run_skill(
            &state,
            &client,
            "ghost-skill",
            &serde_json::json!({}),
            None,
            None,
        )
        .expect_err("uninstalled skill should fail");

        assert!(err.to_string().contains("not installed"), "got: {err}");

        let _ = fs::remove_dir_all(&state_root);
    }

    #[test]
    fn tool_step_extract_text_produces_output_for_chaining() {
        let state_root = temp_root("tool-chain");
        let skill_root = temp_root("tool-chain-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        fs::create_dir_all(skill_root.join("prompts")).unwrap();
        fs::write(
            skill_root.join("SKILL.md"),
            "---\nname: Test Skill\ndescription: A test skill.\nlicense: MIT\nvh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\nvh_workflow_ref: ./workflow.yaml\n---\n\nSummarise.\n",
        )
        .unwrap();
        // workflow: extract doc → stub llm that references extracted output
        fs::write(
            skill_root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: extract\n    type: tool\n    tool: extract_text\n    input: doc\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs:\n      text: extract.output\n",
        )
        .unwrap();
        fs::write(skill_root.join("prompts/system.txt"), "Summarise.").unwrap();

        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let client = MockPolicyClient::new();
        let result = run_skill(
            &state,
            &client,
            "test-skill",
            &serde_json::json!({"doc": "hello world"}),
            None, // stub model
            None,
        )
        .unwrap();

        assert_eq!(result.steps.len(), 2);
        let extract = &result.steps[0];
        assert_eq!(extract.id, "extract");
        assert_eq!(extract.step_type, "tool");
        assert_eq!(
            extract.output,
            Some(serde_json::Value::String("hello world".to_string()))
        );

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn run_errors_when_input_fails_schema_validation() {
        let state_root = temp_root("run-schema-fail");
        let skill_root = temp_root("run-schema-fail-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(
            &skill_root,
            r#"{"type":"object","required":["query"],"properties":{"query":{"type":"string"}}}"#,
        );
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let client = MockPolicyClient::new();
        let err = run_skill(
            &state,
            &client,
            "test-skill",
            &serde_json::json!({"other": 1}),
            None,
            None,
        )
        .expect_err("invalid input should fail");

        assert!(err.to_string().contains("validation"), "got: {err}");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    // ── MockModelClient integration tests ────────────────────────────────────

    #[test]
    fn llm_step_with_mock_model_produces_output_and_token_counts() {
        let state_root = temp_root("mock-llm");
        let skill_root = temp_root("mock-llm-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "{}");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let mock_model = MockModelClient::new("mock response text").with_tokens(20, 15);
        let policy = MockPolicyClient::new();
        let result = run_skill(
            &state,
            &policy,
            "test-skill",
            &serde_json::json!({}),
            Some(&mock_model),
            None,
        )
        .unwrap();

        assert_eq!(result.steps.len(), 1);
        let step = &result.steps[0];
        assert_eq!(step.step_type, "llm");
        assert_eq!(
            step.output,
            Some(serde_json::Value::String("mock response text".to_string()))
        );
        assert_eq!(step.prompt_tokens, Some(20));
        assert_eq!(step.completion_tokens, Some(15));
        assert!(step.latency_ms.is_some());
        assert_eq!(result.total_prompt_tokens, 20);
        assert_eq!(result.total_completion_tokens, 15);

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn llm_step_output_validated_against_schema() {
        let state_root = temp_root("mock-llm-schema");
        let skill_root = temp_root("mock-llm-schema-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        // Create a skill with output_schema on the LLM step.
        fs::create_dir_all(skill_root.join("schemas")).unwrap();
        fs::create_dir_all(skill_root.join("prompts")).unwrap();
        fs::write(
            skill_root.join("SKILL.md"),
            "---\nname: Test Skill\ndescription: A test skill.\nlicense: MIT\nvh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\nvh_workflow_ref: ./workflow.yaml\n---\n\nReturn JSON.\n",
        )
        .unwrap();
        fs::write(
            skill_root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n    output_schema: schemas/output.schema.json\n",
        )
        .unwrap();
        fs::write(skill_root.join("prompts/system.txt"), "Return JSON.").unwrap();
        fs::write(
            skill_root.join("schemas/output.schema.json"),
            r#"{"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}}"#,
        )
        .unwrap();

        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        // Mock returns valid JSON matching the schema.
        let mock_model = MockModelClient::new(r#"{"summary":"all good"}"#);
        let policy = MockPolicyClient::new();
        let result = run_skill(
            &state,
            &policy,
            "test-skill",
            &serde_json::json!({}),
            Some(&mock_model),
            None,
        )
        .unwrap();

        let output = result.steps[0].output.as_ref().unwrap();
        assert_eq!(output["summary"], "all good");

        // Mock returns JSON that does NOT match the schema — should fail validation.
        let bad_model = MockModelClient::new(r#"{"wrong_field":"oops"}"#);
        let err = run_skill(
            &state,
            &policy,
            "test-skill",
            &serde_json::json!({}),
            Some(&bad_model),
            None,
        )
        .expect_err("schema mismatch should fail");

        assert!(err.to_string().contains("schema validation"), "got: {err}");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn multi_step_tool_then_llm_with_mock_model() {
        let state_root = temp_root("multi-step-mock");
        let skill_root = temp_root("multi-step-mock-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        fs::create_dir_all(skill_root.join("prompts")).unwrap();
        fs::write(
            skill_root.join("SKILL.md"),
            "---\nname: Test Skill\ndescription: A test skill.\nlicense: MIT\nvh_execution:\n  sandbox: strict\n  timeout_ms: 30000\n  memory_mb: 256\nvh_workflow_ref: ./workflow.yaml\n---\n\nAnalyze the text.\n",
        )
        .unwrap();
        fs::write(
            skill_root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: extract\n    type: tool\n    tool: extract_text\n    input: doc\n  - id: analyze\n    type: llm\n    prompt: prompts/system.txt\n    inputs:\n      text: extract.output\n",
        )
        .unwrap();
        fs::write(skill_root.join("prompts/system.txt"), "Analyze the text.").unwrap();

        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let mock_model = MockModelClient::new("analysis result").with_tokens(30, 20);
        let policy = MockPolicyClient::new();
        let result = run_skill(
            &state,
            &policy,
            "test-skill",
            &serde_json::json!({"doc": "contract text here"}),
            Some(&mock_model),
            None,
        )
        .unwrap();

        assert_eq!(result.steps.len(), 2);

        // Tool step extracted text.
        assert_eq!(result.steps[0].step_type, "tool");
        assert_eq!(result.steps[0].id, "extract");
        assert_eq!(
            result.steps[0].output,
            Some(serde_json::Value::String("contract text here".to_string()))
        );

        // LLM step used mock model.
        assert_eq!(result.steps[1].step_type, "llm");
        assert_eq!(result.steps[1].id, "analyze");
        assert_eq!(
            result.steps[1].output,
            Some(serde_json::Value::String("analysis result".to_string()))
        );
        assert_eq!(result.steps[1].prompt_tokens, Some(30));
        assert_eq!(result.steps[1].completion_tokens, Some(20));

        assert_eq!(result.total_prompt_tokens, 30);
        assert_eq!(result.total_completion_tokens, 20);

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }
}
