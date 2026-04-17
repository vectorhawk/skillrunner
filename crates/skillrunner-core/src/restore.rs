//! Restore logic for project-scope skills from a lockfile.
//!
//! `restore_project_skills` iterates a [`Lockfile`] and ensures every entry
//! is present in the project cache at `.vectorhawk/skills/{id}/`. Local entries
//! are compiled via [`import_local_skill_md`]; registry entries are either
//! found in cache (skip) or downloaded via the registry client.
//!
//! Partial failure is the norm for a first-clone scenario: the function never
//! aborts early. Each skill independently contributes to one of the three
//! outcome buckets in [`RestoreReport`].

use crate::import::import_local_skill_md;
use crate::lockfile::{verify_integrity, LockedSkill, Lockfile};
use camino::{Utf8Path, Utf8PathBuf};
use tracing::{info, warn};

// ── Public types ──────────────────────────────────────────────────────────────

/// Summary of a restore operation.
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// Skill IDs freshly installed during this restore.
    pub installed: Vec<String>,
    /// Skill IDs that were already present in the cache; no work needed.
    pub cached: Vec<String>,
    /// Skill IDs that could not be restored, with the human-readable reason.
    pub failed: Vec<(String, String)>,
}

// ── Core restore function ─────────────────────────────────────────────────────

/// Restore all skills recorded in `lockfile` into the project cache at
/// `project_root/.vectorhawk/skills/`.
///
/// # Arguments
///
/// - `project_root` — directory that contains (or will contain) `.vectorhawk/`
/// - `lockfile`     — the parsed lockfile describing each skill's source
/// - `registry`     — optional registry client; only needed for `Registry`
///   entries not already in cache. Pass `None` for offline-only restores.
/// - `offline`      — when `true`, an integrity mismatch is a hard error rather
///   than triggering a re-download.
///
/// # Behaviour
///
/// The function never returns `Err`. All per-skill errors are recorded in
/// `RestoreReport::failed` and processing continues with the next skill.
pub fn restore_project_skills(
    project_root: &Utf8Path,
    lockfile: &Lockfile,
    registry: Option<&crate::registry::RegistryClient>,
    offline: bool,
) -> RestoreReport {
    let mut report = RestoreReport::default();

    for (skill_id, entry) in &lockfile.skills {
        match entry {
            LockedSkill::Registry { .. } => {
                restore_registry_skill(project_root, skill_id, entry, registry, offline, &mut report);
            }
            LockedSkill::Local { local_path } => {
                restore_local_skill(project_root, skill_id, local_path, &mut report);
            }
        }
    }

    report
}

// ── Per-skill handlers ────────────────────────────────────────────────────────

fn restore_registry_skill(
    project_root: &Utf8Path,
    skill_id: &str,
    entry: &LockedSkill,
    registry: Option<&crate::registry::RegistryClient>,
    offline: bool,
    report: &mut RestoreReport,
) {
    let cache_dir = project_root
        .join(".vectorhawk")
        .join("skills")
        .join(skill_id);

    // Extract version and integrity from the entry (already matched as Registry).
    let (version, integrity) = match entry {
        LockedSkill::Registry {
            version,
            integrity,
            ..
        } => (version.as_str(), integrity.as_str()),
        LockedSkill::Local { .. } => unreachable!("caller guarantees Registry variant"),
    };

    if is_cached(&cache_dir) {
        // Verify integrity of the cached copy before accepting it.
        match verify_integrity(&cache_dir, integrity) {
            Ok(true) => {
                info!(skill_id, "registry skill already cached — skipping");
                report.cached.push(skill_id.to_string());
                return;
            }
            Ok(false) => {
                if offline {
                    // In offline mode an integrity mismatch is a hard error.
                    let reason =
                        "cached copy failed integrity check and --offline prevents re-download"
                            .to_string();
                    warn!(skill_id, %reason, "integrity mismatch in offline mode");
                    report.failed.push((skill_id.to_string(), reason));
                    return;
                }
                // Online: discard the corrupt cache and fall through to re-download.
                warn!(
                    skill_id,
                    "cached copy failed integrity check — deleting and re-downloading"
                );
                if let Err(e) =
                    std::fs::remove_dir_all(cache_dir.as_std_path())
                {
                    let reason = format!("failed to clear corrupt cache: {e:#}");
                    warn!(skill_id, %reason);
                    report.failed.push((skill_id.to_string(), reason));
                    return;
                }
            }
            Err(e) => {
                // Could not even run the check (e.g. directory unreadable).
                let reason = format!("integrity check error: {e:#}");
                warn!(skill_id, %reason);
                report.failed.push((skill_id.to_string(), reason));
                return;
            }
        }
    }

    let registry_client = match registry {
        Some(r) => r,
        None => {
            let reason = "requires registry access (not cached locally)".to_string();
            warn!(skill_id, %reason, "registry skill not cached and no registry client");
            report.failed.push((skill_id.to_string(), reason));
            return;
        }
    };

    match download_registry_skill_to_cache(
        project_root,
        skill_id,
        version,
        integrity,
        registry_client,
    ) {
        Ok(()) => {
            info!(skill_id, version, "registry skill downloaded to project cache");
            report.installed.push(skill_id.to_string());
        }
        Err(e) => {
            let reason = format!("{e:#}");
            warn!(skill_id, %reason, "failed to download registry skill");
            report.failed.push((skill_id.to_string(), reason));
        }
    }
}

fn restore_local_skill(
    project_root: &Utf8Path,
    skill_id: &str,
    local_path: &str,
    report: &mut RestoreReport,
) {
    let cache_dir = project_root
        .join(".vectorhawk")
        .join("skills")
        .join(skill_id);

    // Check the cache first: if any known bundle marker is present, skip.
    // This must come before the source-file existence check so that a cached
    // entry is reported as "cached" even when the original SKILL.md has since
    // been removed (e.g. after a first-clone restore from CI).
    if is_cached(&cache_dir) {
        info!(skill_id, "local skill already compiled in cache — skipping");
        report.cached.push(skill_id.to_string());
        return;
    }

    let source_path = resolve_local_path(project_root, local_path);

    if !source_path.exists() {
        let reason = format!("file not found: {source_path}");
        warn!(skill_id, %reason, "local skill source not found");
        report.failed.push((skill_id.to_string(), reason));
        return;
    }

    if source_path.is_dir() {
        // Source is a pre-compiled skill bundle directory — copy it directly.
        match copy_bundle_dir_to_cache(&source_path, &cache_dir) {
            Ok(()) => {
                info!(skill_id, path = %source_path, "local skill bundle copied to project cache");
                report.installed.push(skill_id.to_string());
            }
            Err(e) => {
                let reason = format!("{e:#}");
                warn!(skill_id, %reason, "failed to copy local skill bundle");
                report.failed.push((skill_id.to_string(), reason));
            }
        }
    } else {
        // Source is a SKILL.md file — compile it.
        match compile_local_skill_to_cache(&source_path, &cache_dir) {
            Ok(()) => {
                info!(skill_id, path = %source_path, "local skill compiled to project cache");
                report.installed.push(skill_id.to_string());
            }
            Err(e) => {
                let reason = format!("{e:#}");
                warn!(skill_id, %reason, "failed to compile local skill");
                report.failed.push((skill_id.to_string(), reason));
            }
        }
    }
}

// ── I/O helpers ──────────────────────────────────────────────────────────────

/// Return `true` if `cache_dir` already contains a valid bundle.
///
/// We accept either `manifest.json` (server-compiled registry bundle) or
/// `SKILL.md` (locally-scaffolded bundle, post AUTH1f pivot). Either file
/// presence is sufficient to conclude the skill is cached.
fn is_cached(cache_dir: &Utf8Path) -> bool {
    cache_dir.join("manifest.json").exists() || cache_dir.join("SKILL.md").exists()
}

/// Resolve a lockfile-relative path string against the project root.
///
/// The stored path uses forward slashes on all platforms. We parse each
/// component explicitly so it works on Windows without a `replace()` hack.
fn resolve_local_path(project_root: &Utf8Path, local_path: &str) -> Utf8PathBuf {
    // Absolute paths are used as-is (source was outside the project root)
    if local_path.starts_with('/') {
        return Utf8PathBuf::from(local_path);
    }
    let mut result = project_root.to_path_buf();
    for component in local_path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                result.pop();
            }
            seg => result.push(seg),
        }
    }
    result
}

/// Copy a pre-compiled skill bundle directory into `cache_dir`.
fn copy_bundle_dir_to_cache(source_dir: &Utf8Path, cache_dir: &Utf8Path) -> anyhow::Result<()> {
    use anyhow::Context;

    if cache_dir.exists() {
        std::fs::remove_dir_all(cache_dir.as_std_path())
            .with_context(|| format!("failed to clear stale cache dir at {cache_dir}"))?;
    }

    copy_dir_all(source_dir.as_std_path(), cache_dir.as_std_path())
        .with_context(|| format!("failed to copy bundle from {source_dir} to {cache_dir}"))?;

    Ok(())
}

/// Compile a SKILL.md to a bundle and copy it into `cache_dir`.
///
/// Uses `import_local_skill_md` to scaffold into a sibling directory next to
/// the SKILL.md, then copies the output into the project skill cache.
fn compile_local_skill_to_cache(skill_md_path: &Utf8Path, cache_dir: &Utf8Path) -> anyhow::Result<()> {
    use anyhow::Context;

    let bundle = import_local_skill_md(skill_md_path)
        .with_context(|| format!("failed to compile SKILL.md at {skill_md_path}"))?;

    // Remove any stale cache entry before copying.
    if cache_dir.exists() {
        std::fs::remove_dir_all(cache_dir.as_std_path())
            .with_context(|| format!("failed to clear stale cache dir at {cache_dir}"))?;
    }

    copy_dir_all(bundle.output_dir.as_std_path(), cache_dir.as_std_path())
        .with_context(|| {
            format!(
                "failed to copy compiled bundle from {} to {cache_dir}",
                bundle.output_dir
            )
        })?;

    // Clean up the scaffolded output directory left next to the SKILL.md.
    let _ = std::fs::remove_dir_all(bundle.output_dir.as_std_path());

    Ok(())
}

/// Download a registry skill and place the extracted bundle at
/// `project_root/.vectorhawk/skills/{skill_id}/`.
///
/// Mirrors the flow in `updater::download_and_install` but targets the
/// project cache rather than the global user install layout.
#[cfg(feature = "registry")]
fn download_registry_skill_to_cache(
    project_root: &Utf8Path,
    skill_id: &str,
    version: &str,
    _integrity: &str,
    registry: &crate::registry::RegistryClient,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use camino::Utf8PathBuf;

    // 1. Fetch artifact metadata (download URL + sha256).
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

    // 3. Extract to a staging directory.
    let staging_path = Utf8PathBuf::from_path_buf(tmp_dir.path().join("staging"))
        .map_err(|_| anyhow::anyhow!("staging path is not valid UTF-8"))?;
    std::fs::create_dir_all(&staging_path).context("failed to create staging dir")?;

    extract_cskill(&archive_path, &staging_path)
        .with_context(|| format!("failed to extract {skill_id}@{version}"))?;

    // 4. Copy the extracted bundle into the project cache.
    let cache_dir = project_root
        .join(".vectorhawk")
        .join("skills")
        .join(skill_id);

    if cache_dir.exists() {
        std::fs::remove_dir_all(cache_dir.as_std_path())
            .with_context(|| format!("failed to clear stale cache dir at {cache_dir}"))?;
    }

    copy_dir_all(staging_path.as_std_path(), cache_dir.as_std_path())
        .with_context(|| format!("failed to copy bundle to project cache at {cache_dir}"))?;

    Ok(())
}

/// Stub for non-registry builds: always returns an error explaining why.
#[cfg(not(feature = "registry"))]
fn download_registry_skill_to_cache(
    _project_root: &Utf8Path,
    skill_id: &str,
    _version: &str,
    _integrity: &str,
    _registry: &crate::registry::RegistryClient,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "registry downloads are not available in this build (skill '{skill_id}')"
    )
}

/// Extract a `.cskill` (gzipped tar) archive to `dest`.
fn extract_cskill(archive_path: &Utf8Path, dest: &Utf8Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = std::fs::File::open(archive_path.as_std_path())
        .with_context(|| format!("failed to open archive {archive_path}"))?;
    let gz = GzDecoder::new(file);
    let mut tar = Archive::new(gz);
    tar.unpack(dest.as_std_path())
        .with_context(|| format!("failed to unpack archive to {dest}"))?;
    Ok(())
}

/// Recursively copy a directory tree from `src` to `dst`.
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;

    std::fs::create_dir_all(dst)
        .with_context(|| format!("failed to create directory {}", dst.display()))?;

    for entry in std::fs::read_dir(src)
        .with_context(|| format!("failed to read directory {}", src.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read entry in {}", src.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to get file type for {}", entry.path().display()))?;
        let target = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::Lockfile;
    use camino::Utf8PathBuf;
    use std::fs;

    fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let utf8 = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .expect("tempdir path is valid UTF-8");
        (dir, utf8)
    }

    /// Minimal valid SKILL.md content for tests.
    const MINIMAL_SKILL_MD: &str = "\
---\n\
name: test-skill\n\
description: A test skill.\n\
license: Apache-2.0\n\
---\n\
\n\
You are a helpful assistant.\n";

    // ── Test 1: all cached → zero installs ────────────────────────────────────

    #[test]
    fn restore_all_cached_skips() {
        let (_dir, root) = tempdir();

        // Pre-populate the cache with two skills.
        for id in &["skill-a", "skill-b"] {
            let cache_dir = root.join(".vectorhawk").join("skills").join(id);
            fs::create_dir_all(cache_dir.as_std_path()).expect("create cache dir");
            // Write a manifest.json so the cache-check passes.
            fs::write(
                cache_dir.join("manifest.json").as_std_path(),
                r#"{"id": "skill-a", "version": "0.1.0"}"#,
            )
            .expect("write manifest");
        }

        // Compute the real integrity for skill-a from the files written above.
        let cache_a = root.join(".vectorhawk").join("skills").join("skill-a");
        let integrity_a = crate::lockfile::compute_integrity(&cache_a)
            .expect("compute integrity for skill-a");

        let mut lockfile = Lockfile::new();
        lockfile.upsert(
            "skill-a".to_string(),
            crate::lockfile::LockedSkill::Registry {
                version: "0.1.0".to_string(),
                registry_url: "https://app.vectorhawk.ai".to_string(),
                // Use the real hash so integrity verification passes.
                integrity: integrity_a,
            },
        );
        lockfile.upsert(
            "skill-b".to_string(),
            crate::lockfile::LockedSkill::Local {
                local_path: "./skill-b.md".to_string(),
            },
        );

        // Pre-populate skill-b cache too (manifest exists).
        let cache_b = root.join(".vectorhawk").join("skills").join("skill-b");
        fs::write(
            cache_b.join("manifest.json").as_std_path(),
            r#"{"id": "skill-b", "version": "0.1.0"}"#,
        )
        .expect("write manifest b");

        let report = restore_project_skills(&root, &lockfile, None, false);

        assert!(report.installed.is_empty(), "expected no installs");
        assert!(report.failed.is_empty(), "expected no failures");
        assert_eq!(report.cached.len(), 2);
    }

    // ── Test 2: local SKILL.md compiles and appears in cache ─────────────────

    #[test]
    fn restore_local_skill_compiles() {
        let (_dir, root) = tempdir();

        // Write a real SKILL.md at ./skills/test-skill.md relative to root.
        let skills_dir = root.join("skills");
        fs::create_dir_all(skills_dir.as_std_path()).expect("create skills dir");
        let skill_md = skills_dir.join("test-skill.md");
        fs::write(skill_md.as_std_path(), MINIMAL_SKILL_MD).expect("write SKILL.md");

        let mut lockfile = Lockfile::new();
        lockfile.upsert(
            "test-skill".to_string(),
            crate::lockfile::LockedSkill::Local {
                local_path: "./skills/test-skill.md".to_string(),
            },
        );

        let report = restore_project_skills(&root, &lockfile, None, false);

        assert!(report.failed.is_empty(), "expected no failures: {:?}", report.failed);
        assert_eq!(report.installed, vec!["test-skill"]);
        assert!(report.cached.is_empty());

        // The bundle must be present in the cache. Locally-scaffolded bundles
        // (post AUTH1f pivot) contain SKILL.md rather than manifest.json.
        let cache_skill_md = root
            .join(".vectorhawk")
            .join("skills")
            .join("test-skill")
            .join("SKILL.md");
        assert!(
            cache_skill_md.exists(),
            "SKILL.md should exist in project cache after local restore"
        );
    }

    // ── Test 3: missing local file → graceful failure, no panic ──────────────

    #[test]
    fn restore_missing_local_fails_gracefully() {
        let (_dir, root) = tempdir();

        let mut lockfile = Lockfile::new();
        lockfile.upsert(
            "ghost-skill".to_string(),
            crate::lockfile::LockedSkill::Local {
                local_path: "./skills/does-not-exist.md".to_string(),
            },
        );

        let report = restore_project_skills(&root, &lockfile, None, false);

        assert!(report.installed.is_empty());
        assert!(report.cached.is_empty());
        assert_eq!(report.failed.len(), 1);
        let (id, reason) = &report.failed[0];
        assert_eq!(id, "ghost-skill");
        assert!(
            reason.contains("does-not-exist.md"),
            "reason should name the missing file: {reason}"
        );
    }

    // ── Test 4: partial failure continues — one installed, one failed ─────────

    #[test]
    fn restore_partial_failure_continues() {
        let (_dir, root) = tempdir();

        // Write a real SKILL.md for the valid skill.
        let skills_dir = root.join("skills");
        fs::create_dir_all(skills_dir.as_std_path()).expect("create skills dir");
        let skill_md = skills_dir.join("real-skill.md");
        fs::write(skill_md.as_std_path(), MINIMAL_SKILL_MD).expect("write SKILL.md");

        let mut lockfile = Lockfile::new();
        lockfile.upsert(
            "real-skill".to_string(),
            crate::lockfile::LockedSkill::Local {
                local_path: "./skills/real-skill.md".to_string(),
            },
        );
        lockfile.upsert(
            "ghost-skill".to_string(),
            crate::lockfile::LockedSkill::Local {
                local_path: "./skills/ghost-skill.md".to_string(),
            },
        );

        let report = restore_project_skills(&root, &lockfile, None, false);

        // BTreeMap ordering: "ghost-skill" < "real-skill" alphabetically
        // but we only care about counts, not order.
        assert_eq!(report.installed.len(), 1, "one skill should install");
        assert_eq!(report.failed.len(), 1, "one skill should fail");
        assert!(report.cached.is_empty());

        assert!(report.installed.contains(&"real-skill".to_string()));
        let (failed_id, _) = &report.failed[0];
        assert_eq!(failed_id, "ghost-skill");
    }

    // ── Test 5: registry entry not cached, no registry client → failed ────────

    #[test]
    fn restore_registry_skill_offline_fails() {
        let (_dir, root) = tempdir();

        let mut lockfile = Lockfile::new();
        lockfile.upsert(
            "registry-skill".to_string(),
            crate::lockfile::LockedSkill::Registry {
                version: "1.0.0".to_string(),
                registry_url: "https://app.vectorhawk.ai".to_string(),
                integrity: "sha256-abc".to_string(),
            },
        );

        let report = restore_project_skills(&root, &lockfile, None, false);

        assert!(report.installed.is_empty());
        assert!(report.cached.is_empty());
        assert_eq!(report.failed.len(), 1);
        let (id, reason) = &report.failed[0];
        assert_eq!(id, "registry-skill");
        assert!(
            reason.contains("registry access"),
            "reason should mention registry: {reason}"
        );
    }

    // ── Test 6: integrity mismatch in offline mode → hard error ──────────────

    #[test]
    fn restore_offline_integrity_mismatch_fails() {
        let (_dir, root) = tempdir();

        // Pre-populate the cache with some files.
        let cache_dir = root.join(".vectorhawk").join("skills").join("skill-x");
        fs::create_dir_all(cache_dir.as_std_path()).expect("create cache dir");
        fs::write(
            cache_dir.join("manifest.json").as_std_path(),
            b"{\"id\":\"skill-x\"}",
        )
        .expect("write manifest");

        let mut lockfile = Lockfile::new();
        lockfile.upsert(
            "skill-x".to_string(),
            // Use a deliberately wrong hash so integrity check fails.
            crate::lockfile::LockedSkill::Registry {
                version: "1.0.0".to_string(),
                registry_url: "https://app.vectorhawk.ai".to_string(),
                integrity: "sha256-0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            },
        );

        // offline=true: mismatch must be a hard failure, not a re-download.
        let report = restore_project_skills(&root, &lockfile, None, true);

        assert!(report.cached.is_empty(), "should not be cached with wrong hash");
        assert!(report.installed.is_empty(), "should not be installed");
        assert_eq!(report.failed.len(), 1, "should have one failure");
        let (id, reason) = &report.failed[0];
        assert_eq!(id, "skill-x");
        assert!(
            reason.contains("offline"),
            "offline mode failure reason should mention offline: {reason}"
        );
    }

    // ── Test 7: integrity mismatch online → cache deleted, re-download attempted ─

    #[test]
    fn restore_online_integrity_mismatch_deletes_cache() {
        let (_dir, root) = tempdir();

        // Pre-populate the cache with stale/wrong content.
        let cache_dir = root.join(".vectorhawk").join("skills").join("skill-y");
        fs::create_dir_all(cache_dir.as_std_path()).expect("create cache dir");
        fs::write(
            cache_dir.join("manifest.json").as_std_path(),
            b"{\"tampered\":true}",
        )
        .expect("write manifest");

        let mut lockfile = Lockfile::new();
        lockfile.upsert(
            "skill-y".to_string(),
            crate::lockfile::LockedSkill::Registry {
                version: "1.0.0".to_string(),
                registry_url: "https://app.vectorhawk.ai".to_string(),
                // Wrong hash → mismatch.
                integrity: "sha256-0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            },
        );

        // offline=false but no registry client → should fail with "registry access"
        // after clearing the corrupt cache.
        let report = restore_project_skills(&root, &lockfile, None, false);

        // Cache directory must have been removed.
        assert!(
            !cache_dir.exists(),
            "corrupt cache should have been deleted on integrity mismatch"
        );
        // Without a registry client the re-download fails gracefully.
        assert_eq!(report.failed.len(), 1);
        let (id, reason) = &report.failed[0];
        assert_eq!(id, "skill-y");
        assert!(
            reason.contains("registry access"),
            "should fail with registry access error after cache cleared: {reason}"
        );
    }
}
