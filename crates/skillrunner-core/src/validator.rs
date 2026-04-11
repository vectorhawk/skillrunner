use camino::Utf8Path;
use skillrunner_manifest::SkillPackage;

#[derive(Debug)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct ValidationReport {
    pub checks: Vec<CheckResult>,
}

impl ValidationReport {
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
}

/// Run all validation checks against an unpacked skill bundle directory.
///
/// Checks (in order):
/// 1. Manifest parses, required fields present, referenced files exist,
///    workflow parses, and workflow prompt refs resolve.
/// 2. `inputs_schema` file is valid JSON and a valid JSON Schema document.
/// 3. `outputs_schema` file is valid JSON and a valid JSON Schema document.
///
/// All checks are always run; the caller receives the full report.
pub fn validate_bundle(path: &Utf8Path) -> ValidationReport {
    let mut checks = Vec::new();

    // Check 1: SkillPackage::load_from_dir covers manifest + workflow + file refs.
    let pkg = match SkillPackage::load_from_dir(path) {
        Ok(pkg) => {
            checks.push(ok("manifest and workflow"));
            Some(pkg)
        }
        Err(e) => {
            checks.push(fail("manifest and workflow", &e.to_string()));
            None::<skillrunner_manifest::SkillPackage>
        }
    };

    if let Some(pkg) = pkg {
        checks.push(check_json_schema_value(
            "inputs_schema",
            pkg.manifest.inputs_schema.as_ref(),
        ));
        checks.push(check_json_schema_value(
            "outputs_schema",
            pkg.manifest.outputs_schema.as_ref(),
        ));
    }

    ValidationReport { checks }
}

/// Validate that an already-parsed JSON Schema value is a valid JSON Schema.
/// When `schema` is `None` (omitted from manifest), the check is a no-op pass.
fn check_json_schema_value(label: &str, schema: Option<&serde_json::Value>) -> CheckResult {
    let name = format!("{label} is valid JSON Schema");

    let json = match schema {
        Some(v) => v,
        None => return ok(&name), // absent schema is valid (pass-through)
    };

    match jsonschema::JSONSchema::compile(json) {
        Ok(_) => ok(&name),
        Err(e) => fail(&name, &format!("not valid JSON Schema: {e}")),
    }
}

fn ok(name: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        passed: true,
        detail: None,
    }
}

fn fail(name: &str, detail: &str) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        passed: false,
        detail: Some(detail.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("forge-tests-validator-{label}-{nanos}")),
        )
        .unwrap()
    }

    fn write_valid_bundle(root: &Utf8PathBuf) {
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
            r#"{"type":"object"}"#,
        )
        .unwrap();
        fs::write(
            root.join("schemas/output.schema.json"),
            r#"{"type":"object"}"#,
        )
        .unwrap();
    }

    #[test]
    fn validate_passes_for_well_formed_bundle() {
        let dir = temp_dir("ok");
        write_valid_bundle(&dir);

        let report = validate_bundle(&dir);

        assert!(
            report.all_passed(),
            "expected all checks to pass: {report:?}"
        );
        assert_eq!(report.checks.len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_fails_manifest_check_when_manifest_is_missing() {
        let dir = temp_dir("no-manifest");
        write_valid_bundle(&dir);
        fs::remove_file(dir.join("manifest.json")).unwrap();

        let report = validate_bundle(&dir);

        let manifest_check = report
            .checks
            .iter()
            .find(|c| c.name == "manifest and workflow")
            .unwrap();
        assert!(!manifest_check.passed);
        assert!(!report.all_passed());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_fails_manifest_check_when_schema_file_is_not_valid_json() {
        // Schema files are parsed at manifest-load time (AUTH1b: inputs_schema is
        // Option<serde_json::Value>). Invalid JSON in a schema file causes the
        // "manifest and workflow" check to fail, not a separate schema check.
        let dir = temp_dir("bad-schema");
        write_valid_bundle(&dir);
        fs::write(dir.join("schemas/input.schema.json"), "not json {{{").unwrap();

        let report = validate_bundle(&dir);

        let manifest_check = report
            .checks
            .iter()
            .find(|c| c.name == "manifest and workflow")
            .unwrap();
        assert!(!manifest_check.passed, "expected manifest check to fail");
        assert!(!report.all_passed());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_reports_all_checks_even_when_some_fail() {
        let dir = temp_dir("partial-fail");
        write_valid_bundle(&dir);
        // Valid JSON but not valid JSON Schema syntax.
        fs::write(dir.join("schemas/output.schema.json"), r#"{"type": 42}"#).unwrap();

        let report = validate_bundle(&dir);

        // Manifest check still runs and passes.
        let manifest_check = report
            .checks
            .iter()
            .find(|c| c.name == "manifest and workflow")
            .unwrap();
        assert!(manifest_check.passed);
        // Output schema check fails.
        let output_check = report
            .checks
            .iter()
            .find(|c| c.name.contains("output"))
            .unwrap();
        assert!(!output_check.passed);

        let _ = fs::remove_dir_all(&dir);
    }
}
