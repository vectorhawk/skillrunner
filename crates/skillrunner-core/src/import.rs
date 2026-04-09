use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use std::fs;

/// Files written when scaffolding a bundle from a SKILL.md.
#[derive(Debug)]
pub struct ScaffoldedBundle {
    pub id: String,
    pub output_dir: Utf8PathBuf,
    pub files: Vec<String>,
}

/// YAML frontmatter block parsed from the top of a SKILL.md file.
#[derive(Debug, Deserialize)]
struct SkillMdFrontmatter {
    name: String,
    description: Option<String>,
    license: Option<String>,
    #[serde(default)]
    triggers: Vec<String>,
}

/// Read a SKILL.md, parse its frontmatter and body, and scaffold a complete
/// .skill bundle directory next to the source file.
///
/// The SKILL.md body becomes `prompts/system.txt`. A single `llm` workflow
/// step is generated that passes user requirements through the system prompt.
pub fn import_skill_md(skill_md_path: &Utf8Path) -> Result<ScaffoldedBundle> {
    let content = fs::read_to_string(skill_md_path)
        .with_context(|| format!("failed to read {skill_md_path}"))?;

    let (frontmatter, body) = parse_frontmatter(&content)
        .with_context(|| format!("failed to parse frontmatter in {skill_md_path}"))?;

    let output_dir = skill_md_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("SKILL.md has no parent directory"))?
        .to_path_buf();

    let id = to_skill_id(&frontmatter.name);
    let files = scaffold_bundle(&output_dir, &id, &frontmatter, body.trim())?;

    Ok(ScaffoldedBundle {
        id,
        output_dir,
        files,
    })
}

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

fn parse_frontmatter(content: &str) -> Result<(SkillMdFrontmatter, &str)> {
    let after_open = content
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md must begin with a --- frontmatter block"))?;

    let close = after_open
        .find("\n---\n")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md frontmatter closing --- not found"))?;

    let yaml_str = &after_open[..close];
    let body = &after_open[close + 5..]; // skip "\n---\n"

    let frontmatter: SkillMdFrontmatter =
        serde_yaml::from_str(yaml_str).context("SKILL.md frontmatter is not valid YAML")?;

    Ok((frontmatter, body))
}

// ---------------------------------------------------------------------------
// ID derivation
// ---------------------------------------------------------------------------

fn to_skill_id(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

// ---------------------------------------------------------------------------
// Bundle scaffolding
// ---------------------------------------------------------------------------

fn scaffold_bundle(
    dir: &Utf8Path,
    id: &str,
    frontmatter: &SkillMdFrontmatter,
    system_prompt: &str,
) -> Result<Vec<String>> {
    fs::create_dir_all(dir.join("prompts"))?;
    fs::create_dir_all(dir.join("schemas"))?;

    let mut written: Vec<String> = Vec::new();

    // System prompt body
    write_file(dir, "prompts/system.txt", system_prompt, &mut written)?;

    // Input schema: a single required `requirements` string
    write_file(dir, "schemas/input.schema.json", INPUT_SCHEMA, &mut written)?;

    // Output schema: required `code` string, optional `notes`
    write_file(
        dir,
        "schemas/output.schema.json",
        OUTPUT_SCHEMA,
        &mut written,
    )?;

    // Workflow: single llm step driven by the system prompt
    let workflow_name = id.replace('-', "_");
    let workflow = format!(
        "name: {workflow_name}\nsteps:\n\
         \x20 - id: generate\n\
         \x20   type: llm\n\
         \x20   prompt: prompts/system.txt\n\
         \x20   inputs:\n\
         \x20     requirements: input.requirements\n\
         \x20   output_schema: schemas/output.schema.json\n"
    );
    write_file(dir, "workflow.yaml", &workflow, &mut written)?;

    // Manifest
    let manifest = build_manifest_json(id, frontmatter);
    write_file(dir, "manifest.json", &manifest, &mut written)?;

    Ok(written)
}

fn write_file(dir: &Utf8Path, rel: &str, content: &str, log: &mut Vec<String>) -> Result<()> {
    let path = dir.join(rel);
    fs::write(&path, content).with_context(|| format!("failed to write {path}"))?;
    log.push(rel.to_string());
    Ok(())
}

fn build_manifest_json(id: &str, fm: &SkillMdFrontmatter) -> String {
    let description = fm.description.as_deref().unwrap_or("");
    let description = if description.is_empty() {
        format!("A skill that helps with {}", fm.name.to_lowercase())
    } else {
        description.to_string()
    };
    let license_line = fm
        .license
        .as_deref()
        .map(|l| {
            format!(
                "\n  \"license\": {},",
                serde_json::to_string(l).expect("license is valid JSON string")
            )
        })
        .unwrap_or_default();

    // Auto-generate triggers from description when none are provided
    let triggers = if fm.triggers.is_empty() {
        generate_triggers_from_description(fm.description.as_deref().unwrap_or(""), &fm.name)
    } else {
        fm.triggers.clone()
    };

    let triggers_line = if triggers.is_empty() {
        String::new()
    } else {
        format!(
            "\n  \"triggers\": {},",
            serde_json::to_string(&triggers).expect("triggers are valid JSON")
        )
    };

    // Prompt-only skills (single LLM step, no network) are offload-eligible
    let offload_line = "\n  \"offload_eligible\": true,";

    format!(
        r#"{{
  "schema_version": "1.0",
  "id": "{id}",
  "name": {name},
  "version": "0.1.0",
  "publisher": "skillclub",
  "description": {description},{license_line}{triggers_line}{offload_line}
  "entrypoint": "workflow.yaml",
  "inputs_schema": "schemas/input.schema.json",
  "outputs_schema": "schemas/output.schema.json",
  "permissions": {{
    "filesystem": "none",
    "network": "none",
    "clipboard": false
  }},
  "execution": {{
    "sandbox_profile": "strict",
    "timeout_seconds": 120,
    "memory_mb": 512
  }},
  "update": {{
    "channel": "stable",
    "auto_update": true
  }}
}}"#,
        name = serde_json::to_string(&fm.name).expect("name is valid JSON string"),
        description =
            serde_json::to_string(&description).expect("description is valid JSON string"),
    )
}

// ---------------------------------------------------------------------------
// Trigger auto-generation
// ---------------------------------------------------------------------------

/// Generate trigger phrases from a skill's description and name.
///
/// This is a simple keyword-extraction approach: split the description on
/// commas/conjunctions into clauses, lowercase them, strip leading articles
/// and filler, and use each clause as a trigger phrase. The skill name
/// (kebab-cased to spaces) is always included as a fallback trigger.
pub fn generate_triggers_from_description(description: &str, name: &str) -> Vec<String> {
    let mut triggers = Vec::new();

    // Add the skill name as a natural-language trigger
    let name_trigger = name.to_lowercase().replace('-', " ");
    if !name_trigger.is_empty() {
        triggers.push(name_trigger);
    }

    if description.is_empty() {
        return triggers;
    }

    // Split description on commas and "and" to extract clause-level phrases
    let clauses: Vec<&str> = description
        .split([',', '.'])
        .flat_map(|s| s.split(" and "))
        .collect();

    for clause in clauses {
        let trimmed = clause.trim().to_lowercase();
        if trimmed.is_empty() || trimmed.len() < 5 {
            continue;
        }

        // Strip leading articles and filler words
        let cleaned = trimmed
            .trim_start_matches("a ")
            .trim_start_matches("an ")
            .trim_start_matches("the ")
            .trim_start_matches("this skill ")
            .trim_start_matches("will ")
            .trim_start_matches("can ")
            .trim_start_matches("helps ")
            .trim_start_matches("that ")
            .trim();

        if cleaned.len() >= 5 && !triggers.contains(&cleaned.to_string()) {
            triggers.push(cleaned.to_string());
        }
    }

    // Cap at 5 triggers to avoid noise
    triggers.truncate(5);
    triggers
}

// ---------------------------------------------------------------------------
// Embedded schemas
// ---------------------------------------------------------------------------

const INPUT_SCHEMA: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "required": ["requirements"],
  "properties": {
    "requirements": {
      "type": "string",
      "description": "Description of the frontend component, page, or application to build."
    }
  },
  "additionalProperties": false
}"#;

const OUTPUT_SCHEMA: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "required": ["code"],
  "properties": {
    "code": {
      "type": "string",
      "description": "Generated frontend code."
    },
    "notes": {
      "type": "string",
      "description": "Optional design rationale or implementation notes."
    }
  },
  "additionalProperties": false
}"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("forge-tests-import-{label}-{nanos}")),
        )
        .unwrap()
    }

    fn write_skill_md(dir: &Utf8Path, content: &str) -> Utf8PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    const SAMPLE_SKILL_MD: &str = "\
---
name: my-skill
description: Does something cool.
license: MIT
---

This is the system prompt body.
It can span multiple lines.
";

    #[test]
    fn import_creates_expected_bundle_files() {
        let dir = temp_dir("full");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        let result = import_skill_md(&path).expect("import should succeed");

        assert_eq!(result.id, "my-skill");
        assert!(dir.join("manifest.json").exists());
        assert!(dir.join("workflow.yaml").exists());
        assert!(dir.join("prompts/system.txt").exists());
        assert!(dir.join("schemas/input.schema.json").exists());
        assert!(dir.join("schemas/output.schema.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_writes_system_prompt_body_to_prompts_system_txt() {
        let dir = temp_dir("prompt");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        import_skill_md(&path).unwrap();

        let body = fs::read_to_string(dir.join("prompts/system.txt")).unwrap();
        assert!(body.contains("This is the system prompt body."));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_manifest_contains_correct_id_and_metadata() {
        let dir = temp_dir("manifest");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        import_skill_md(&path).unwrap();

        let manifest_text = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest_text.contains("\"id\": \"my-skill\""));
        assert!(manifest_text.contains("\"description\": \"Does something cool.\""));
        assert!(manifest_text.contains("\"license\": \"MIT\""));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_bundle_loads_cleanly_with_skill_package() {
        use skillrunner_manifest::SkillPackage;

        let dir = temp_dir("roundtrip");
        let path = write_skill_md(&dir, SAMPLE_SKILL_MD);

        import_skill_md(&path).unwrap();

        let pkg = SkillPackage::load_from_dir(&dir)
            .expect("generated bundle should pass SkillPackage validation");
        assert_eq!(pkg.manifest.id, "my-skill");
        assert_eq!(pkg.workflow.steps.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn to_skill_id_handles_spaces_and_mixed_case() {
        assert_eq!(to_skill_id("Frontend Design"), "frontend-design");
        assert_eq!(to_skill_id("my_skill"), "my-skill");
        assert_eq!(to_skill_id("already-kebab"), "already-kebab");
    }

    #[test]
    fn generate_triggers_from_description_extracts_clauses() {
        let triggers = generate_triggers_from_description(
            "Compare two contracts, summarize changes, and assess risk level.",
            "contract-compare",
        );
        assert!(triggers.contains(&"contract compare".to_string()));
        assert!(
            triggers.len() >= 2,
            "should have at least name + one clause, got: {triggers:?}"
        );
    }

    #[test]
    fn generate_triggers_uses_name_as_fallback() {
        let triggers = generate_triggers_from_description("", "my-skill");
        assert_eq!(triggers, vec!["my skill"]);
    }

    #[test]
    fn generate_triggers_strips_filler() {
        let triggers = generate_triggers_from_description(
            "This skill will help with code review and find bugs",
            "code-review",
        );
        assert!(triggers.contains(&"code review".to_string()));
        // "This skill will help with code review" should be cleaned
        let has_help = triggers.iter().any(|t| t.contains("help with"));
        assert!(
            has_help,
            "should extract 'help with code review', got: {triggers:?}"
        );
    }

    #[test]
    fn generate_triggers_caps_at_five() {
        let triggers = generate_triggers_from_description(
            "one thing, two thing, three thing, four thing, five thing, six thing, seven thing",
            "many-triggers",
        );
        assert!(
            triggers.len() <= 5,
            "should cap at 5, got: {}",
            triggers.len()
        );
    }

    #[test]
    fn import_auto_generates_triggers_when_missing() {
        let dir = temp_dir("auto-triggers");
        let skill_md = "\
---
name: Contract Compare
description: Compare two contracts and summarize changes.
---

You are a contract analysis expert.
";
        let path = write_skill_md(&dir, skill_md);
        import_skill_md(&path).unwrap();

        let manifest_text = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(
            manifest_text.contains("\"triggers\""),
            "manifest should contain auto-generated triggers, got:\n{manifest_text}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_preserves_explicit_triggers() {
        let dir = temp_dir("explicit-triggers");
        let skill_md = "\
---
name: My Skill
description: Does things.
triggers:
  - do the thing
  - make it happen
---

System prompt here.
";
        let path = write_skill_md(&dir, skill_md);
        import_skill_md(&path).unwrap();

        let manifest_text = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest_text.contains("do the thing"));
        assert!(manifest_text.contains("make it happen"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_missing_frontmatter() {
        let dir = temp_dir("bad-frontmatter");
        let path = write_skill_md(&dir, "No frontmatter here.");

        let err = import_skill_md(&path).expect_err("missing frontmatter should fail");
        assert!(err.to_string().contains("frontmatter"), "got: {err}");

        let _ = fs::remove_dir_all(&dir);
    }
}
