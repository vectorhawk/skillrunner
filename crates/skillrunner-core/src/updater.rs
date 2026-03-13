use crate::{
    install::install_unpacked_skill,
    policy::Policy,
    registry::RegistryClient,
    state::AppState,
};
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use flate2::read::GzDecoder;
use rusqlite::{Connection, OptionalExtension};
use semver::Version;
use skillrunner_manifest::SkillPackage;
use tar::Archive;
use tracing::info;

/// Silently update `skill_id` to `policy.target_version` if the currently
/// installed version is below `policy.minimum_allowed_version`.
///
/// Returns `true` if an update was performed, `false` if none was needed.
pub fn auto_update_if_needed(
    state: &AppState,
    registry: &RegistryClient,
    skill_id: &str,
    policy: &Policy,
) -> Result<bool> {
    let (Some(min_ver), Some(target_ver)) = (&policy.minimum_allowed_version, &policy.target_version)
    else {
        return Ok(false);
    };

    let installed_str = query_installed_version(state, skill_id)?;
    let Some(installed_str) = installed_str else {
        return Ok(false);
    };

    let installed = Version::parse(&installed_str).with_context(|| {
        format!("installed version '{installed_str}' is not valid semver")
    })?;

    if &installed >= min_ver {
        return Ok(false); // Already at or above the minimum.
    }

    info!(
        skill_id,
        installed = %installed,
        target = %target_ver,
        "installed version below minimum; auto-updating"
    );

    let target_str = target_ver.to_string();
    download_and_install(state, registry, skill_id, &target_str)?;

    info!(skill_id, version = %target_ver, "auto-update complete");
    Ok(true)
}

// ── Download + install flow ───────────────────────────────────────────────────

fn download_and_install(
    state: &AppState,
    registry: &RegistryClient,
    skill_id: &str,
    version: &str,
) -> Result<()> {
    // 1. Fetch artifact metadata (download URL + expected hash).
    let metadata = registry
        .fetch_artifact_metadata(skill_id, version)
        .with_context(|| format!("failed to fetch metadata for {skill_id}@{version}"))?;

    // 2. Download the .cskill archive to a temp file.
    let tmp_dir = tempfile::TempDir::new().context("failed to create temp dir for download")?;
    let archive_path = Utf8PathBuf::from_path_buf(tmp_dir.path().join("bundle.cskill"))
        .map_err(|_| anyhow::anyhow!("temp dir path is not valid UTF-8"))?;

    registry
        .download_artifact(&metadata.download_url, &metadata.sha256, &archive_path)
        .with_context(|| format!("failed to download {skill_id}@{version}"))?;

    // 3. Extract the tar.gz archive to a staging directory.
    let staging_path = Utf8PathBuf::from_path_buf(tmp_dir.path().join("staging"))
        .map_err(|_| anyhow::anyhow!("staging path is not valid UTF-8"))?;
    std::fs::create_dir_all(&staging_path).context("failed to create staging dir")?;

    extract_skill(&archive_path, &staging_path)
        .with_context(|| format!("failed to extract {skill_id}@{version}"))?;

    // 4. Validate the extracted bundle.
    let pkg = SkillPackage::load_from_dir(&staging_path).with_context(|| {
        format!("downloaded bundle for {skill_id}@{version} failed validation")
    })?;

    // 5. Install via the standard installer.
    install_unpacked_skill(state, &pkg)
        .with_context(|| format!("failed to install updated {skill_id}@{version}"))?;

    Ok(())
}

/// Extract a `.cskill` (tar.gz) archive into `dest`.
fn extract_skill(archive_path: &Utf8PathBuf, dest: &Utf8PathBuf) -> Result<()> {
    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive {archive_path}"))?;
    let gz = GzDecoder::new(file);
    let mut tar = Archive::new(gz);
    tar.unpack(dest.as_std_path())
        .with_context(|| format!("failed to unpack archive to {dest}"))?;
    Ok(())
}

fn query_installed_version(state: &AppState, skill_id: &str) -> Result<Option<String>> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let ver: Option<String> = conn
        .query_row(
            "SELECT active_version FROM installed_skills WHERE skill_id = ?1",
            [skill_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(ver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        install::install_unpacked_skill,
        policy::{Policy, PolicyStatus},
        state::AppState,
    };
    use camino::Utf8PathBuf;
    use semver::Version;
    use std::{fs, time::{SystemTime, UNIX_EPOCH}};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("forge-tests-updater-{label}-{nanos}")),
        )
        .unwrap()
    }

    fn write_skill_bundle(root: &Utf8PathBuf, version: &str) {
        fs::create_dir_all(root.join("schemas")).unwrap();
        fs::create_dir_all(root.join("prompts")).unwrap();
        fs::write(
            root.join("manifest.json"),
            format!(r#"{{
  "schema_version": "1.0",
  "id": "test-skill",
  "name": "Test Skill",
  "version": "{version}",
  "publisher": "skillclub",
  "entrypoint": "workflow.yaml",
  "inputs_schema": "schemas/input.schema.json",
  "outputs_schema": "schemas/output.schema.json",
  "permissions": {{ "filesystem": "none", "network": "none", "clipboard": false }},
  "execution": {{ "sandbox_profile": "strict", "timeout_seconds": 30, "memory_mb": 256 }}
}}"#),
        )
        .unwrap();
        fs::write(
            root.join("workflow.yaml"),
            "name: test_skill\nsteps:\n  - id: run\n    type: llm\n    prompt: prompts/system.txt\n    inputs: {}\n",
        )
        .unwrap();
        fs::write(root.join("prompts/system.txt"), "Do the thing.").unwrap();
        fs::write(root.join("schemas/input.schema.json"), "{}").unwrap();
        fs::write(root.join("schemas/output.schema.json"), "{}").unwrap();
    }

    #[test]
    fn auto_update_skips_when_no_minimum_version() {
        let state_root = temp_root("no-min");
        let skill_root = temp_root("no-min-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "1.0.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let policy = Policy {
            skill_id: "test-skill".to_string(),
            status: PolicyStatus::Active,
            target_version: None,
            minimum_allowed_version: None,
            blocked_message: None,
        };
        // RegistryClient pointing at a non-existent URL; should not be called.
        let registry = RegistryClient::new("http://localhost:0");
        let updated = auto_update_if_needed(&state, &registry, "test-skill", &policy).unwrap();
        assert!(!updated, "should not update when no minimum_allowed_version");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn auto_update_skips_when_version_already_meets_minimum() {
        let state_root = temp_root("meets-min");
        let skill_root = temp_root("meets-min-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        write_skill_bundle(&skill_root, "1.1.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        let policy = Policy {
            skill_id: "test-skill".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("1.1.0").unwrap()),
            minimum_allowed_version: Some(Version::parse("1.1.0").unwrap()),
            blocked_message: None,
        };
        let registry = RegistryClient::new("http://localhost:0");
        let updated = auto_update_if_needed(&state, &registry, "test-skill", &policy).unwrap();
        assert!(!updated, "should not update when installed version meets minimum");

        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn auto_update_skips_when_skill_not_installed() {
        let state_root = temp_root("not-installed");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        let policy = Policy {
            skill_id: "ghost-skill".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("1.1.0").unwrap()),
            minimum_allowed_version: Some(Version::parse("1.1.0").unwrap()),
            blocked_message: None,
        };
        let registry = RegistryClient::new("http://localhost:0");
        let updated = auto_update_if_needed(&state, &registry, "ghost-skill", &policy).unwrap();
        assert!(!updated, "should not update when skill is not installed");

        let _ = fs::remove_dir_all(&state_root);
    }
}
