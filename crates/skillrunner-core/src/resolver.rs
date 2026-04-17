use crate::{
    install::InstallScope,
    policy::{PolicyClient, PolicyStatus},
    state::AppState,
};
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{Connection, OptionalExtension};
use semver::Version;
use skillrunner_manifest::SkillPackage;

#[derive(Debug, PartialEq)]
pub enum ResolveOutcome {
    /// Skill is installed and policy permits execution.
    Active {
        skill_id: String,
        version: String,
        install_path: String,
        scope: InstallScope,
    },
    /// Policy has blocked this skill (or no valid replacement exists).
    Blocked { skill_id: String, reason: String },
    /// Skill has never been installed locally.
    NotInstalled { skill_id: String },
}

/// Resolve `skill_id` to a runnable path or a block/not-installed reason.
///
/// Resolution order:
/// 1. If `project_root` is `Some`, check `.vectorhawk/skills/{skill_id}/SKILL.md`.
///    If present, load the bundle and return `Active` with `scope = Project(...)` — no policy check.
/// 2. Check user-scope SQLite — not found → `NotInstalled`.
/// 3. Fetch policy — `Blocked` status → `Blocked`.
/// 4. If `minimum_allowed_version` is set and the installed version is below
///    it, execution is blocked until an update installs the target version.
/// 5. Otherwise → `Active` with `scope = User`.
pub fn resolve_skill(
    state: &AppState,
    policy_client: &dyn PolicyClient,
    skill_id: &str,
    project_root: Option<&Utf8Path>,
) -> Result<ResolveOutcome> {
    // --- Phase 4: project-scope check (shadows user scope) ---
    if let Some(root) = project_root {
        let project_skill_dir = root
            .join(".vectorhawk")
            .join("skills")
            .join(skill_id);
        // SKILL.md is the canonical presence marker for project-scope bundles
        // (the compile step always produces this file). manifest.json is not
        // written for SKILL.md-based bundles — SkillPackage::load_from_dir
        // reads SKILL.md directly.
        if project_skill_dir.join("SKILL.md").exists() {
            let pkg = SkillPackage::load_from_dir(&project_skill_dir).with_context(|| {
                format!("failed to load project-scope skill '{skill_id}' at {project_skill_dir}")
            })?;
            return Ok(ResolveOutcome::Active {
                skill_id: skill_id.to_string(),
                version: pkg.manifest.version.to_string(),
                install_path: project_skill_dir.to_string(),
                scope: InstallScope::Project(Utf8PathBuf::from(root)),
            });
        }
    }
    // --- End project-scope check ---
    let conn = Connection::open(&state.db_path)?;

    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT active_version, install_root \
             FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let (active_version_str, install_root) = match row {
        None => {
            return Ok(ResolveOutcome::NotInstalled {
                skill_id: skill_id.to_string(),
            })
        }
        Some(r) => r,
    };

    let policy = policy_client.fetch_policy(skill_id)?;

    if policy.status == PolicyStatus::Blocked {
        return Ok(ResolveOutcome::Blocked {
            skill_id: skill_id.to_string(),
            reason: policy
                .blocked_message
                .unwrap_or_else(|| "This skill is temporarily unavailable.".to_string()),
        });
    }

    if let Some(min_ver) = policy.minimum_allowed_version {
        let installed = Version::parse(&active_version_str).map_err(|e| {
            anyhow::anyhow!(
                "installed version '{}' is not valid semver: {e}",
                active_version_str
            )
        })?;
        if installed < min_ver {
            return Ok(ResolveOutcome::Blocked {
                skill_id: skill_id.to_string(),
                reason: format!(
                    "Installed version {installed} is below the minimum allowed version \
                     {min_ver}. Run `skillrunner skill install` to update.",
                ),
            });
        }
    }

    Ok(ResolveOutcome::Active {
        skill_id: skill_id.to_string(),
        version: active_version_str,
        install_path: format!("{}/active", install_root),
        scope: InstallScope::User,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        policy::{MockPolicyClient, Policy, PolicyStatus},
        state::AppState,
    };
    use camino::Utf8PathBuf;
    use rusqlite::{params, Connection};
    use semver::Version;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("forge-tests-resolver-{label}-{nanos}"));
        Utf8PathBuf::from_path_buf(path).expect("temporary test path should be utf-8")
    }

    fn seed_installed(conn: &Connection, skill_id: &str, version: &str, install_root: &str) {
        conn.execute(
            "INSERT INTO installed_skills \
             (skill_id, active_version, install_root, channel, current_status) \
             VALUES (?1, ?2, ?3, 'stable', 'active')",
            params![skill_id, version, install_root],
        )
        .expect("seed row should insert");
    }

    #[test]
    fn resolve_returns_active_for_installed_skill_with_permissive_policy() {
        let root = temp_root("active");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let conn = Connection::open(&state.db_path).unwrap();
        seed_installed(
            &conn,
            "contract-compare",
            "1.0.0",
            "/fake/skills/contract-compare",
        );

        let client = MockPolicyClient::new(); // default active, no constraints
        let outcome = resolve_skill(&state, &client, "contract-compare", None).unwrap();

        assert_eq!(
            outcome,
            ResolveOutcome::Active {
                skill_id: "contract-compare".to_string(),
                version: "1.0.0".to_string(),
                install_path: "/fake/skills/contract-compare/active".to_string(),
                scope: InstallScope::User,
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_returns_not_installed_for_unknown_skill() {
        let root = temp_root("missing");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let client = MockPolicyClient::new();
        let outcome = resolve_skill(&state, &client, "no-such-skill", None).unwrap();

        assert_eq!(
            outcome,
            ResolveOutcome::NotInstalled {
                skill_id: "no-such-skill".to_string(),
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_returns_blocked_when_policy_says_blocked() {
        let root = temp_root("policy-blocked");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let conn = Connection::open(&state.db_path).unwrap();
        seed_installed(
            &conn,
            "contract-compare",
            "1.0.0",
            "/fake/skills/contract-compare",
        );

        let blocked_policy = Policy {
            skill_id: "contract-compare".to_string(),
            status: PolicyStatus::Blocked,
            target_version: None,
            minimum_allowed_version: None,
            blocked_message: Some("Revoked by publisher.".to_string()),
        };
        let client = MockPolicyClient::new().with_policy(blocked_policy);
        let outcome = resolve_skill(&state, &client, "contract-compare", None).unwrap();

        assert_eq!(
            outcome,
            ResolveOutcome::Blocked {
                skill_id: "contract-compare".to_string(),
                reason: "Revoked by publisher.".to_string(),
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_returns_blocked_when_installed_version_below_minimum() {
        let root = temp_root("below-minimum");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let conn = Connection::open(&state.db_path).unwrap();
        // Installed: 1.0.0 — policy requires >= 1.1.0.
        seed_installed(
            &conn,
            "contract-compare",
            "1.0.0",
            "/fake/skills/contract-compare",
        );

        let policy = Policy {
            skill_id: "contract-compare".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("1.1.0").unwrap()),
            minimum_allowed_version: Some(Version::parse("1.1.0").unwrap()),
            blocked_message: None,
        };
        let client = MockPolicyClient::new().with_policy(policy);
        let outcome = resolve_skill(&state, &client, "contract-compare", None).unwrap();

        match outcome {
            ResolveOutcome::Blocked { skill_id, reason } => {
                assert_eq!(skill_id, "contract-compare");
                assert!(
                    reason.contains("1.0.0"),
                    "reason should mention installed version"
                );
                assert!(
                    reason.contains("1.1.0"),
                    "reason should mention minimum version"
                );
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_returns_active_when_installed_version_meets_minimum() {
        let root = temp_root("meets-minimum");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let conn = Connection::open(&state.db_path).unwrap();
        // Installed: 1.1.0 — meets minimum of 1.1.0.
        seed_installed(
            &conn,
            "contract-compare",
            "1.1.0",
            "/fake/skills/contract-compare",
        );

        let policy = Policy {
            skill_id: "contract-compare".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("1.1.0").unwrap()),
            minimum_allowed_version: Some(Version::parse("1.1.0").unwrap()),
            blocked_message: None,
        };
        let client = MockPolicyClient::new().with_policy(policy);
        let outcome = resolve_skill(&state, &client, "contract-compare", None).unwrap();

        assert_eq!(
            outcome,
            ResolveOutcome::Active {
                skill_id: "contract-compare".to_string(),
                version: "1.1.0".to_string(),
                install_path: "/fake/skills/contract-compare/active".to_string(),
                scope: InstallScope::User,
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Write a minimal but valid SKILL.md-based skill bundle at `dir`.
    fn write_project_skill_bundle(dir: &Utf8PathBuf) {
        std::fs::create_dir_all(dir.join("schemas")).expect("schemas dir");
        std::fs::create_dir_all(dir.join("prompts")).expect("prompts dir");
        // Mirrors the fixture from skillrunner-manifest's write_example_skill test helper.
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: Test Skill\ndescription: A test skill.\nlicense: MIT\n\
             vh_version: 0.2.0\nvh_publisher: test\n\
             vh_permissions:\n  filesystem: none\n  network: none\n  clipboard: none\n\
             vh_workflow_ref: ./workflow.yaml\n\
             vh_schemas:\n  inputs:\n    type: object\n  outputs:\n    type: object\n\
             ---\nTest body.\n",
        )
        .expect("SKILL.md");
        std::fs::write(
            dir.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    output_schema: schemas/output.schema.json\n    inputs: {}\n",
        )
        .expect("workflow.yaml");
        std::fs::write(dir.join("schemas/output.schema.json"), "{}")
            .expect("output schema");
        std::fs::write(dir.join("prompts/system.txt"), "Test.").expect("prompt");
    }

    #[test]
    fn resolve_project_scope_shadows_user() {
        let root = temp_root("proj-shadows-user");

        // Set up user-scope AppState and seed the same skill in SQLite.
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let conn = Connection::open(&state.db_path).unwrap();
        seed_installed(
            &conn,
            "test-skill",
            "0.1.0",
            "/fake/skills/test-skill",
        );

        // Create a project root with a project-scope bundle for the same skill.
        let project_root = root.join("project");
        let skill_dir = project_root.join(".vectorhawk").join("skills").join("test-skill");
        write_project_skill_bundle(&skill_dir);

        let client = MockPolicyClient::new();
        let outcome = resolve_skill(&state, &client, "test-skill", Some(project_root.as_path()))
            .unwrap();

        match outcome {
            ResolveOutcome::Active { install_path, scope, .. } => {
                // Must resolve to the project-scope path, not the user-scope path.
                assert!(
                    install_path.contains(".vectorhawk"),
                    "install_path should be the project-scope dir, got: {install_path}"
                );
                assert_eq!(
                    scope,
                    InstallScope::Project(project_root.clone()),
                    "scope should be Project"
                );
            }
            other => panic!("expected Active, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_falls_through_to_user_when_no_project_skill() {
        let root = temp_root("proj-fallthrough");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let conn = Connection::open(&state.db_path).unwrap();
        seed_installed(
            &conn,
            "test-skill",
            "0.1.0",
            "/fake/skills/test-skill",
        );

        // Project root exists but the skill is NOT in .vectorhawk/skills/.
        let project_root = root.join("project");
        std::fs::create_dir_all(project_root.join(".vectorhawk").join("skills"))
            .expect("project dir");

        let client = MockPolicyClient::new();
        let outcome = resolve_skill(&state, &client, "test-skill", Some(project_root.as_path()))
            .unwrap();

        match outcome {
            ResolveOutcome::Active { install_path, scope, .. } => {
                assert_eq!(
                    scope,
                    InstallScope::User,
                    "should fall through to user scope"
                );
                assert!(
                    install_path.ends_with("/active"),
                    "user-scope path should end with /active, got: {install_path}"
                );
            }
            other => panic!("expected user-scope Active, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_project_root_none_unchanged() {
        // Passing project_root = None must behave identically to pre-Phase-4.
        let root = temp_root("proj-none");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let conn = Connection::open(&state.db_path).unwrap();
        seed_installed(
            &conn,
            "contract-compare",
            "1.0.0",
            "/fake/skills/contract-compare",
        );

        let client = MockPolicyClient::new();
        let outcome = resolve_skill(&state, &client, "contract-compare", None).unwrap();

        assert_eq!(
            outcome,
            ResolveOutcome::Active {
                skill_id: "contract-compare".to_string(),
                version: "1.0.0".to_string(),
                install_path: "/fake/skills/contract-compare/active".to_string(),
                scope: InstallScope::User,
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
