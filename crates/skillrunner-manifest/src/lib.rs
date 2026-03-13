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
}
