use camino::{Utf8Path, Utf8PathBuf};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("missing required file: {0}")]
    MissingFile(String),
    #[error("invalid manifest: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: String,
    pub id: String,
    pub name: String,
    pub version: Version,
    pub publisher: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub entrypoint: String,
    pub inputs_schema: String,
    pub outputs_schema: String,
    pub permissions: Permissions,
    pub execution: Execution,
    pub model_requirements: Option<ModelRequirements>,
    pub update: Option<UpdateConfig>,
    /// Trigger phrases that help AI clients decide when to invoke this skill.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Whether this skill can be offloaded to a cheaper/faster model.
    #[serde(default)]
    pub offload_eligible: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions {
    pub filesystem: String,
    pub network: String,
    pub clipboard: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub sandbox_profile: String,
    pub timeout_seconds: u64,
    pub memory_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequirements {
    pub min_context_tokens: Option<u64>,
    pub supports_structured_output: Option<bool>,
    pub supports_tool_calling: Option<bool>,
    pub preferred_execution: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    pub channel: Option<String>,
    pub auto_update: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkflowStep {
    #[serde(rename = "tool")]
    Tool {
        id: String,
        tool: String,
        input: serde_yaml::Value,
    },
    #[serde(rename = "llm")]
    Llm {
        id: String,
        prompt: String,
        inputs: Option<serde_yaml::Value>,
        output_schema: Option<String>,
    },
    #[serde(rename = "transform")]
    Transform {
        id: String,
        op: String,
        input: serde_yaml::Value,
    },
    #[serde(rename = "validate")]
    Validate {
        id: String,
        schema: String,
        input: serde_yaml::Value,
    },
}

#[derive(Debug, Clone)]
pub struct SkillPackage {
    pub root: Utf8PathBuf,
    pub manifest: Manifest,
    pub workflow: Workflow,
}

impl SkillPackage {
    pub fn load_from_dir(root: impl AsRef<Utf8Path>) -> Result<Self, ManifestError> {
        let root = root.as_ref().to_path_buf();
        let manifest_path = root.join("manifest.json");
        let manifest_text = fs::read_to_string(&manifest_path)?;
        let manifest: Manifest = serde_json::from_str(&manifest_text)?;

        validate_manifest_files(&root, &manifest)?;

        let workflow_path = root.join(&manifest.entrypoint);
        let workflow_text = fs::read_to_string(&workflow_path)?;
        let workflow: Workflow = serde_yaml::from_str(&workflow_text)?;

        validate_workflow_refs(&root, &workflow)?;

        Ok(Self {
            root,
            manifest,
            workflow,
        })
    }
}

fn validate_manifest_files(root: &Utf8Path, manifest: &Manifest) -> Result<(), ManifestError> {
    if manifest.id.trim().is_empty()
        || manifest.name.trim().is_empty()
        || manifest.publisher.trim().is_empty()
    {
        return Err(ManifestError::Invalid(
            "id, name, and publisher must be non-empty".to_string(),
        ));
    }

    if manifest.schema_version != "1.0" {
        return Err(ManifestError::Invalid(format!(
            "unsupported schema_version {}",
            manifest.schema_version
        )));
    }

    for rel in [
        manifest.entrypoint.as_str(),
        manifest.inputs_schema.as_str(),
        manifest.outputs_schema.as_str(),
    ] {
        if rel.trim().is_empty() {
            return Err(ManifestError::Invalid(
                "entrypoint, inputs_schema, and outputs_schema must be non-empty".to_string(),
            ));
        }
        let p = root.join(rel);
        if !p.exists() {
            return Err(ManifestError::MissingFile(rel.to_string()));
        }
    }

    Ok(())
}

fn validate_workflow_refs(root: &Utf8Path, workflow: &Workflow) -> Result<(), ManifestError> {
    for step in &workflow.steps {
        if let WorkflowStep::Llm { prompt, .. } = step {
            if prompt.trim().is_empty() {
                return Err(ManifestError::Invalid(
                    "llm step prompt path must be non-empty".to_string(),
                ));
            }
            let p = root.join(prompt.as_str());
            if !p.exists() {
                return Err(ManifestError::MissingFile(prompt.clone()));
            }
        }
    }
    Ok(())
}

// ── Plugin Manifest ─────────────────────────────────────────────────────────

/// A plugin is a composite, governed bundle that packages skills + MCP servers
/// + slash commands into a single installable unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub schema_version: String,
    pub id: String,
    pub name: String,
    pub version: Version,
    pub publisher: String,
    pub description: Option<String>,
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,

    /// Embedded or registry-referenced skills.
    #[serde(default)]
    pub skills: Vec<PluginSkillRef>,
    /// MCP server connections (go through governance approval on install).
    #[serde(default)]
    pub mcp_servers: Vec<PluginMcpServer>,
    /// Slash command markdown files.
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    /// User-prompted configuration values.
    #[serde(default)]
    pub user_config: std::collections::HashMap<String, PluginUserConfigEntry>,
    /// Update settings.
    pub update: Option<UpdateConfig>,
}

/// A skill referenced by a plugin — either embedded (path) or from registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSkillRef {
    /// Path to embedded skill bundle directory (relative to plugin root).
    pub path: Option<String>,
    /// Registry skill ID (resolved at install time).
    pub registry_id: Option<String>,
    /// Minimum version for registry-referenced skills.
    pub min_version: Option<String>,
}

/// An MCP server connection declared by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMcpServer {
    pub name: String,
    /// How to run the server (e.g. "npx -y @anthropic/mcp-server-jira").
    pub package_source: Option<String>,
    pub description: Option<String>,
    /// OAuth scopes needed from the backend system.
    #[serde(default)]
    pub downstream_scopes: Vec<String>,
    /// Human-readable note about credentials.
    pub credential_note: Option<String>,
}

/// A slash command markdown file declared by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    /// Path to the command markdown file (relative to plugin root).
    pub path: String,
}

/// A user-config entry prompted at install time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUserConfigEntry {
    pub description: String,
    #[serde(default)]
    pub sensitive: bool,
}

/// A loaded plugin bundle from disk.
#[derive(Debug, Clone)]
pub struct PluginPackage {
    pub root: Utf8PathBuf,
    pub manifest: PluginManifest,
}

impl PluginPackage {
    /// Load and validate a plugin bundle from a directory containing `plugin.json`.
    pub fn load_from_dir(root: impl AsRef<Utf8Path>) -> Result<Self, ManifestError> {
        let root = root.as_ref().to_path_buf();
        let manifest_path = root.join("plugin.json");
        let manifest_text = fs::read_to_string(&manifest_path)?;
        let manifest: PluginManifest = serde_json::from_str(&manifest_text)?;

        validate_plugin_manifest(&root, &manifest)?;

        Ok(Self { root, manifest })
    }
}

fn validate_plugin_manifest(root: &Utf8Path, manifest: &PluginManifest) -> Result<(), ManifestError> {
    if manifest.id.trim().is_empty() || manifest.name.trim().is_empty() || manifest.publisher.trim().is_empty() {
        return Err(ManifestError::Invalid("id, name, and publisher must be non-empty".to_string()));
    }

    if manifest.schema_version != "1.0" {
        return Err(ManifestError::Invalid(format!(
            "unsupported plugin schema_version {}",
            manifest.schema_version
        )));
    }

    // Must have at least one component
    if manifest.skills.is_empty() && manifest.mcp_servers.is_empty() && manifest.commands.is_empty() {
        return Err(ManifestError::Invalid(
            "plugin must contain at least one skill, MCP server, or command".to_string(),
        ));
    }

    // Validate embedded skill refs have paths that exist
    for skill_ref in &manifest.skills {
        if let Some(path) = &skill_ref.path {
            let skill_dir = root.join(path);
            if !skill_dir.join("manifest.json").exists() {
                return Err(ManifestError::MissingFile(format!(
                    "{path}/manifest.json"
                )));
            }
        }
        // registry_id refs are validated at install time, not load time
        if skill_ref.path.is_none() && skill_ref.registry_id.is_none() {
            return Err(ManifestError::Invalid(
                "skill ref must have either 'path' or 'registry_id'".to_string(),
            ));
        }
    }

    // Validate command paths exist
    for cmd in &manifest.commands {
        let cmd_path = root.join(&cmd.path);
        if !cmd_path.exists() {
            return Err(ManifestError::MissingFile(cmd.path.clone()));
        }
    }

    // Validate MCP servers have names
    for server in &manifest.mcp_servers {
        if server.name.trim().is_empty() {
            return Err(ManifestError::Invalid(
                "MCP server name must be non-empty".to_string(),
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(test_name: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("forge-tests-{test_name}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temporary test path should be utf-8")
    }

    fn write_example_skill(root: &Utf8Path) {
        fs::create_dir_all(root.join("schemas")).expect("schemas dir should be created");
        fs::create_dir_all(root.join("prompts")).expect("prompts dir should be created");

        fs::write(
            root.join("manifest.json"),
            r#"{
  "schema_version": "1.0",
  "id": "contract-compare",
  "name": "Contract Compare",
  "version": "0.1.0",
  "publisher": "forge",
  "description": "Compare two contracts and summarize changes.",
  "entrypoint": "workflow.yaml",
  "inputs_schema": "schemas/input.schema.json",
  "outputs_schema": "schemas/output.schema.json",
  "permissions": {
    "filesystem": "read_only_scoped",
    "network": "none",
    "clipboard": false
  },
  "execution": {
    "sandbox_profile": "strict",
    "timeout_seconds": 90,
    "memory_mb": 1024
  }
}"#,
        )
        .expect("manifest should be written");
        fs::write(
            root.join("workflow.yaml"),
            r#"name: contract_compare
steps:
  - id: compare
    type: llm
    prompt: prompts/compare.txt
    inputs:
      text_a: input.doc_a
      text_b: input.doc_b
    output_schema: schemas/output.schema.json
"#,
        )
        .expect("workflow should be written");
        fs::write(root.join("schemas/input.schema.json"), "{}")
            .expect("input schema should be written");
        fs::write(root.join("schemas/output.schema.json"), "{}")
            .expect("output schema should be written");
        fs::write(root.join("prompts/compare.txt"), "Compare the contracts.")
            .expect("prompt should be written");
    }

    #[test]
    fn load_from_dir_reads_valid_skill_package() {
        let root = temp_root("manifest-valid");
        write_example_skill(&root);

        let package = SkillPackage::load_from_dir(&root).expect("valid skill package should load");

        assert_eq!(package.manifest.id, "contract-compare");
        assert_eq!(
            package.manifest.version,
            Version::parse("0.1.0").expect("semver should parse")
        );
        assert_eq!(package.workflow.steps.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_from_dir_rejects_missing_required_manifest_files() {
        let root = temp_root("manifest-missing-file");
        write_example_skill(&root);
        fs::remove_file(root.join("schemas/output.schema.json"))
            .expect("output schema should be removable for the test");

        let error = SkillPackage::load_from_dir(&root).expect_err("missing schema should fail");

        match error {
            ManifestError::MissingFile(path) => assert_eq!(path, "schemas/output.schema.json"),
            other => panic!("expected missing file error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_from_dir_rejects_empty_id() {
        let root = temp_root("manifest-empty-id");
        write_example_skill(&root);
        fs::write(
            root.join("manifest.json"),
            r#"{
  "schema_version": "1.0",
  "id": "   ",
  "name": "Contract Compare",
  "version": "0.1.0",
  "publisher": "forge",
  "entrypoint": "workflow.yaml",
  "inputs_schema": "schemas/input.schema.json",
  "outputs_schema": "schemas/output.schema.json",
  "permissions": {
    "filesystem": "read_only_scoped",
    "network": "none",
    "clipboard": false
  },
  "execution": {
    "sandbox_profile": "strict",
    "timeout_seconds": 90,
    "memory_mb": 1024
  }
}"#,
        )
        .expect("manifest should be rewritten");

        let error = SkillPackage::load_from_dir(&root).expect_err("blank id should fail");

        match error {
            ManifestError::Invalid(message) => {
                assert!(message.contains("non-empty"), "got: {message}")
            }
            other => panic!("expected invalid manifest error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_from_dir_rejects_missing_workflow_prompt_ref() {
        let root = temp_root("manifest-missing-prompt");
        write_example_skill(&root);
        // Remove the prompt file that the workflow references.
        fs::remove_file(root.join("prompts/compare.txt"))
            .expect("prompt file should be removable for the test");

        let error =
            SkillPackage::load_from_dir(&root).expect_err("missing prompt ref should fail");

        match error {
            ManifestError::MissingFile(path) => {
                assert_eq!(path, "prompts/compare.txt")
            }
            other => panic!("expected missing file error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_from_dir_rejects_unsupported_schema_version() {
        let root = temp_root("manifest-schema-version");
        write_example_skill(&root);
        fs::write(
            root.join("manifest.json"),
            r#"{
  "schema_version": "2.0",
  "id": "contract-compare",
  "name": "Contract Compare",
  "version": "0.1.0",
  "publisher": "forge",
  "entrypoint": "workflow.yaml",
  "inputs_schema": "schemas/input.schema.json",
  "outputs_schema": "schemas/output.schema.json",
  "permissions": {
    "filesystem": "read_only_scoped",
    "network": "none",
    "clipboard": false
  },
  "execution": {
    "sandbox_profile": "strict",
    "timeout_seconds": 90,
    "memory_mb": 1024
  }
}"#,
        )
        .expect("manifest should be rewritten");

        let error =
            SkillPackage::load_from_dir(&root).expect_err("unsupported schema version should fail");

        match error {
            ManifestError::Invalid(message) => {
                assert!(message.contains("unsupported schema_version 2.0"))
            }
            other => panic!("expected invalid manifest error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    // ── Plugin manifest tests ───────────────────────────────────────────────

    fn write_example_plugin(root: &Utf8Path) {
        // Create embedded skill
        let skill_dir = root.join("skills").join("my-skill");
        fs::create_dir_all(skill_dir.join("schemas")).unwrap();
        fs::create_dir_all(skill_dir.join("prompts")).unwrap();
        fs::write(skill_dir.join("manifest.json"), r#"{
            "schema_version": "1.0", "id": "my-skill", "name": "My Skill",
            "version": "0.1.0", "publisher": "test",
            "entrypoint": "workflow.yaml",
            "inputs_schema": "schemas/input.schema.json",
            "outputs_schema": "schemas/output.schema.json",
            "permissions": {"filesystem": "none", "network": "none", "clipboard": false},
            "execution": {"sandbox_profile": "strict", "timeout_seconds": 30, "memory_mb": 512}
        }"#).unwrap();
        fs::write(skill_dir.join("workflow.yaml"), "name: test\nsteps: []").unwrap();
        fs::write(skill_dir.join("schemas/input.schema.json"), "{}").unwrap();
        fs::write(skill_dir.join("schemas/output.schema.json"), "{}").unwrap();
        fs::write(skill_dir.join("prompts/system.txt"), "test prompt").unwrap();

        // Create command
        let cmd_dir = root.join("commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(cmd_dir.join("my-command.md"), "---\nname: my-command\ndescription: test\n---\nDo the thing.").unwrap();

        // Create plugin.json
        fs::write(root.join("plugin.json"), r#"{
            "schema_version": "1.0",
            "id": "test-plugin",
            "name": "Test Plugin",
            "version": "0.1.0",
            "publisher": "test-publisher",
            "description": "A test plugin",
            "category": "testing",
            "tags": ["test"],
            "skills": [
                { "path": "./skills/my-skill" }
            ],
            "mcp_servers": [
                {
                    "name": "Test MCP",
                    "package_source": "npx -y @test/mcp-server",
                    "description": "Test server"
                }
            ],
            "commands": [
                { "path": "./commands/my-command.md" }
            ],
            "user_config": {
                "api_key": { "description": "Your API key", "sensitive": true }
            }
        }"#).unwrap();
    }

    #[test]
    fn plugin_package_loads_valid_bundle() {
        let root = temp_root("plugin-valid");
        fs::create_dir_all(&root).unwrap();
        write_example_plugin(&root);

        let pkg = PluginPackage::load_from_dir(&root).expect("valid plugin should load");
        assert_eq!(pkg.manifest.id, "test-plugin");
        assert_eq!(pkg.manifest.name, "Test Plugin");
        assert_eq!(pkg.manifest.version.to_string(), "0.1.0");
        assert_eq!(pkg.manifest.publisher, "test-publisher");
        assert_eq!(pkg.manifest.skills.len(), 1);
        assert_eq!(pkg.manifest.mcp_servers.len(), 1);
        assert_eq!(pkg.manifest.mcp_servers[0].name, "Test MCP");
        assert_eq!(pkg.manifest.commands.len(), 1);
        assert_eq!(pkg.manifest.user_config.len(), 1);
        assert!(pkg.manifest.user_config.contains_key("api_key"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_rejects_empty_components() {
        let root = temp_root("plugin-empty");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("plugin.json"), r#"{
            "schema_version": "1.0", "id": "empty", "name": "Empty",
            "version": "0.1.0", "publisher": "test"
        }"#).unwrap();

        let err = PluginPackage::load_from_dir(&root).expect_err("empty plugin should fail");
        assert!(matches!(err, ManifestError::Invalid(msg) if msg.contains("at least one")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_rejects_missing_skill_manifest() {
        let root = temp_root("plugin-missing-skill");
        fs::create_dir_all(root.join("skills/bad-skill")).unwrap();
        fs::write(root.join("plugin.json"), r#"{
            "schema_version": "1.0", "id": "bad", "name": "Bad",
            "version": "0.1.0", "publisher": "test",
            "skills": [{ "path": "./skills/bad-skill" }]
        }"#).unwrap();

        let err = PluginPackage::load_from_dir(&root).expect_err("missing skill manifest should fail");
        assert!(matches!(err, ManifestError::MissingFile(f) if f.contains("manifest.json")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_rejects_skill_ref_without_path_or_registry_id() {
        let root = temp_root("plugin-bad-ref");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("plugin.json"), r#"{
            "schema_version": "1.0", "id": "bad", "name": "Bad",
            "version": "0.1.0", "publisher": "test",
            "skills": [{}]
        }"#).unwrap();

        let err = PluginPackage::load_from_dir(&root).expect_err("bad ref should fail");
        assert!(matches!(err, ManifestError::Invalid(msg) if msg.contains("path")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_accepts_registry_id_skill_ref() {
        let root = temp_root("plugin-registry-ref");
        fs::create_dir_all(&root).unwrap();
        // Command needed so plugin has at least one component besides the registry skill
        let cmd_dir = root.join("commands");
        fs::create_dir_all(&cmd_dir).unwrap();
        fs::write(cmd_dir.join("cmd.md"), "---\nname: cmd\ndescription: test\n---\nDo it.").unwrap();

        fs::write(root.join("plugin.json"), r#"{
            "schema_version": "1.0", "id": "reg", "name": "Registry Ref",
            "version": "0.1.0", "publisher": "test",
            "skills": [{ "registry_id": "sprint-planning", "min_version": "1.0.0" }],
            "commands": [{ "path": "./commands/cmd.md" }]
        }"#).unwrap();

        let pkg = PluginPackage::load_from_dir(&root).expect("registry ref should load");
        assert_eq!(pkg.manifest.skills[0].registry_id.as_deref(), Some("sprint-planning"));

        let _ = fs::remove_dir_all(root);
    }
}
