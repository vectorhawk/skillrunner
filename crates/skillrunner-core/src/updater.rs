use crate::{
    install::install_unpacked_skill,
    policy::Policy,
    registry::RegistryClient,
    state::AppState,
};
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use rusqlite::{Connection, OptionalExtension};
use semver::Version;
use sha2::{Digest, Sha256};
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

/// Install a skill from the registry by ID.
///
/// If `version` is `None`, resolves the latest published version.
/// Returns the installed version string.
pub fn install_from_registry(
    state: &AppState,
    registry: &RegistryClient,
    skill_id: &str,
    version: Option<&str>,
) -> Result<String> {
    let version = match version {
        Some(v) => v.to_string(),
        None => {
            let detail = registry
                .fetch_skill_detail(skill_id)
                .with_context(|| format!("failed to look up '{skill_id}' in the registry"))?;
            detail
                .latest_version
                .ok_or_else(|| anyhow::anyhow!("skill '{skill_id}' has no published versions"))?
        }
    };

    info!(skill_id, version, "installing from registry");
    download_and_install(state, registry, skill_id, &version)?;
    Ok(version)
}

/// Package a skill directory into a `.cskill` tar.gz archive.
///
/// Validates the bundle first, then creates the archive in a temp directory.
/// Returns `(archive_path, sha256_hex)`.
pub fn package_skill(skill_dir: &Utf8Path) -> Result<(Utf8PathBuf, String)> {
    let pkg = SkillPackage::load_from_dir(skill_dir)
        .with_context(|| format!("skill at {skill_dir} failed validation"))?;

    let filename = format!("{}-{}.cskill", pkg.manifest.id, pkg.manifest.version);
    let archive_path = Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(&filename),
    )
    .map_err(|_| anyhow::anyhow!("temp dir path is not valid UTF-8"))?;

    let file = std::fs::File::create(&archive_path)
        .with_context(|| format!("failed to create {archive_path}"))?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(".", skill_dir.as_std_path())
        .with_context(|| format!("failed to build archive from {skill_dir}"))?;
    tar.finish().context("failed to finalize archive")?;
    drop(tar);

    let archive_bytes = std::fs::read(&archive_path)
        .with_context(|| format!("failed to read archive {archive_path}"))?;
    let sha = hex::encode(Sha256::digest(&archive_bytes));

    info!(
        skill_id = %pkg.manifest.id,
        version = %pkg.manifest.version,
        path = %archive_path,
        sha256 = %sha,
        "packaged skill"
    );

    Ok((archive_path, sha))
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
        registry::RegistryClient,
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

    /// Create a tar.gz archive of a skill bundle and return (archive_path, sha256_hex).
    fn create_skill_archive(version: &str) -> (tempfile::TempDir, String, String) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use sha2::{Digest, Sha256};

        let tmp = tempfile::TempDir::new().unwrap();
        let bundle_dir = tmp.path().join("bundle");
        let bundle_utf8 = Utf8PathBuf::from_path_buf(bundle_dir.clone()).unwrap();
        write_skill_bundle(&bundle_utf8, version);

        let archive_path = tmp.path().join("bundle.cskill");
        let file = fs::File::create(&archive_path).unwrap();
        let enc = GzEncoder::new(file, Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all(".", &bundle_dir).unwrap();
        tar.finish().unwrap();
        drop(tar);

        let archive_bytes = fs::read(&archive_path).unwrap();
        let sha = hex::encode(Sha256::digest(&archive_bytes));
        let archive_path_str = archive_path.to_string_lossy().to_string();

        (tmp, archive_path_str, sha)
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

    #[test]
    fn auto_update_downloads_extracts_and_installs_new_version() {
        use mockito::Server;

        let state_root = temp_root("auto-update-happy");
        let skill_root = temp_root("auto-update-happy-skill");
        let state = AppState::bootstrap_in(state_root.clone()).unwrap();

        // Install v1.0.0 (below minimum)
        write_skill_bundle(&skill_root, "1.0.0");
        let pkg = SkillPackage::load_from_dir(&skill_root).unwrap();
        install_unpacked_skill(&state, &pkg).unwrap();

        // Create a v2.0.0 archive to serve
        let (_tmp, archive_path, sha) = create_skill_archive("2.0.0");
        let archive_bytes = fs::read(&archive_path).unwrap();

        let mut server = Server::new();
        let download_path = "/download/test-skill-2.0.0.cskill";

        // Mock metadata endpoint
        let meta_mock = server
            .mock("GET", "/skills/test-skill/versions/2.0.0")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{
                    "skill_id": "test-skill",
                    "version": "2.0.0",
                    "download_url": "{}{download_path}",
                    "sha256": "{sha}",
                    "size_bytes": {}
                }}"#,
                server.url(),
                archive_bytes.len()
            ))
            .create();

        // Mock download endpoint
        let dl_mock = server
            .mock("GET", download_path)
            .with_status(200)
            .with_body(&archive_bytes)
            .create();

        let policy = Policy {
            skill_id: "test-skill".to_string(),
            status: PolicyStatus::Active,
            target_version: Some(Version::parse("2.0.0").unwrap()),
            minimum_allowed_version: Some(Version::parse("2.0.0").unwrap()),
            blocked_message: None,
        };

        let registry = RegistryClient::new(server.url());
        let updated = auto_update_if_needed(&state, &registry, "test-skill", &policy).unwrap();
        assert!(updated, "should have performed the update");

        // Verify the new version is now installed
        let conn = rusqlite::Connection::open(&state.db_path).unwrap();
        let active_ver: String = conn
            .query_row(
                "SELECT active_version FROM installed_skills WHERE skill_id = 'test-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_ver, "2.0.0");

        // Verify files on disk
        let install_path = state
            .root_dir
            .join("skills/test-skill/versions/2.0.0/manifest.json");
        assert!(install_path.exists(), "manifest.json should exist for v2.0.0");

        meta_mock.assert();
        dl_mock.assert();
        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&skill_root);
    }
}
