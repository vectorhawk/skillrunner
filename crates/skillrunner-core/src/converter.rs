//! Convert a legacy bundle (manifest.json + workflow.yaml + prompts/) into
//! the canonical SKILL.md format (AUTH1d — inverse of `skill import`).

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
use std::fs;

/// Convert a legacy skill bundle directory into a SKILL.md file plus any
/// sibling assets that cannot be inlined.
///
/// The output directory will contain:
/// - `SKILL.md` with frontmatter + body (from prompts/system.txt or first prompt)
/// - `workflow.yaml` (copied only if >5 steps)
/// - Sibling dirs (prompts/, scripts/, references/) copied minus the body prompt
pub fn convert_bundle_to_skill_md(bundle_path: &Utf8Path, output_path: &Utf8Path) -> Result<()> {
    // Read and parse manifest.json
    let manifest_path = bundle_path.join("manifest.json");
    let manifest_text =
        fs::read_to_string(&manifest_path).context("failed to read manifest.json")?;
    let manifest: JsonValue =
        serde_json::from_str(&manifest_text).context("failed to parse manifest.json")?;

    // Read workflow.yaml (parse as generic YAML to count steps)
    let workflow_path = bundle_path.join("workflow.yaml");
    let workflow_text =
        fs::read_to_string(&workflow_path).context("failed to read workflow.yaml")?;
    let workflow_yaml: YamlValue =
        serde_yaml::from_str(&workflow_text).context("failed to parse workflow.yaml")?;

    // Find the body text from prompts/
    let body = find_body_prompt(bundle_path)?;

    // Count steps from the generic YAML
    let step_count = workflow_yaml
        .get("steps")
        .and_then(|s| s.as_sequence())
        .map(|s| s.len())
        .unwrap_or(0);

    // Build the frontmatter YAML
    let frontmatter = build_frontmatter(&manifest, &workflow_yaml, step_count, bundle_path)?;

    // Determine whether to inline the workflow or reference it
    let inline_workflow = step_count <= 5;

    // Create output directory
    fs::create_dir_all(output_path).context("failed to create output directory")?;

    // Write SKILL.md
    let mut skill_md = String::new();
    skill_md.push_str("---\n");
    skill_md.push_str(&frontmatter);
    if !inline_workflow {
        skill_md.push_str("vh_workflow_ref: ./workflow.yaml\n");
    }
    skill_md.push_str("---\n\n");
    skill_md.push_str(&body);
    if !body.ends_with('\n') {
        skill_md.push('\n');
    }

    fs::write(output_path.join("SKILL.md"), &skill_md).context("failed to write SKILL.md")?;

    // Copy workflow.yaml if not inlined
    if !inline_workflow {
        fs::copy(&workflow_path, output_path.join("workflow.yaml"))
            .context("failed to copy workflow.yaml")?;
    }

    // Copy sibling directories (prompts/, scripts/, references/)
    for dir_name in &["prompts", "scripts", "references"] {
        let src_dir = bundle_path.join(dir_name);
        if src_dir.is_dir() {
            copy_dir_recursive(&src_dir, &output_path.join(dir_name))?;
        }
    }

    Ok(())
}

/// Find the primary prompt text to use as the SKILL.md body.
fn find_body_prompt(bundle_path: &Utf8Path) -> Result<String> {
    let prompts_dir = bundle_path.join("prompts");
    if !prompts_dir.is_dir() {
        return Ok(String::new());
    }

    // Prefer system.txt
    let system_txt = prompts_dir.join("system.txt");
    if system_txt.exists() {
        return fs::read_to_string(&system_txt).context("failed to read prompts/system.txt");
    }

    // Fall back to first .txt or .md file
    let mut entries: Vec<_> = fs::read_dir(&prompts_dir)
        .context("failed to read prompts/ directory")?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".txt") || name.ends_with(".md")
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    if let Some(entry) = entries.first() {
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("non-utf8 path: {}", p.display()))?;
        return fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path));
    }

    Ok(String::new())
}

/// Build the YAML frontmatter string from a generic manifest JSON and workflow.
fn build_frontmatter(
    manifest: &JsonValue,
    workflow_yaml: &YamlValue,
    step_count: usize,
    bundle_path: &Utf8Path,
) -> Result<String> {
    let mut fm = serde_yaml::Mapping::new();

    let str_field = |v: &JsonValue, key: &str| -> String {
        v.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
    };

    // Standard fields
    fm.insert(y_str("name"), y_str(&str_field(manifest, "name")));
    fm.insert(y_str("description"), y_str(&str_field(manifest, "description")));
    let license = str_field(manifest, "license");
    fm.insert(y_str("license"), y_str(if license.is_empty() { "MIT" } else { &license }));

    // VectorHawk core
    fm.insert(y_str("vh_version"), y_str(&str_field(manifest, "version")));
    fm.insert(y_str("vh_publisher"), y_str(&str_field(manifest, "publisher")));

    // Permissions (pass through as-is, normalizing booleans to strings)
    if let Some(perms) = manifest.get("permissions") {
        let yaml_perms: YamlValue = serde_yaml::to_value(perms)?;
        fm.insert(y_str("vh_permissions"), yaml_perms);
    }

    // Execution
    if let Some(exec) = manifest.get("execution") {
        let yaml_exec: YamlValue = serde_yaml::to_value(exec)?;
        fm.insert(y_str("vh_execution"), yaml_exec);
    }

    // Model requirements
    if let Some(model) = manifest.get("model_requirements") {
        let yaml_model: YamlValue = serde_yaml::to_value(model)?;
        fm.insert(y_str("vh_model"), yaml_model);
    }

    // Schemas (read from schemas/ directory)
    let schemas_dir = bundle_path.join("schemas");
    if schemas_dir.is_dir() {
        let mut schemas = serde_yaml::Mapping::new();
        let input_schema = schemas_dir.join("input.schema.json");
        if input_schema.exists() {
            let text = fs::read_to_string(&input_schema)?;
            let val: JsonValue = serde_json::from_str(&text)?;
            schemas.insert(y_str("inputs"), serde_yaml::to_value(&val)?);
        }
        let output_schema = schemas_dir.join("output.schema.json");
        if output_schema.exists() {
            let text = fs::read_to_string(&output_schema)?;
            let val: JsonValue = serde_json::from_str(&text)?;
            schemas.insert(y_str("outputs"), serde_yaml::to_value(&val)?);
        }
        if !schemas.is_empty() {
            fm.insert(y_str("vh_schemas"), YamlValue::Mapping(schemas));
        }
    }

    // Inline workflow steps if <=5 steps
    if step_count <= 5 {
        if let Some(steps) = workflow_yaml.get("steps") {
            fm.insert(y_str("vh_workflow"), steps.clone());
        }
    }

    // Triggers (if present)
    if let Some(triggers) = manifest.get("triggers") {
        if triggers.is_array() && !triggers.as_array().unwrap().is_empty() {
            fm.insert(y_str("vh_triggers"), serde_yaml::to_value(triggers)?);
        }
    }

    let yaml_str = serde_yaml::to_string(&YamlValue::Mapping(fm))?;
    Ok(yaml_str)
}

fn y_str(s: &str) -> YamlValue {
    YamlValue::String(s.to_string())
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("non-utf8 path: {}", p.display()))?;
        let dst_path = dst.join(src_path.file_name().unwrap());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
