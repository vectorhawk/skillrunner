use crate::{
    policy::{Policy, PolicyClient, PolicyStatus},
    state::AppState,
};
use anyhow::{Context, Result};
use camino::Utf8Path;
use rusqlite::{params, Connection, OptionalExtension};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, warn};

// ── Registry wire types ───────────────────────────────────────────────────────

/// Wire format returned by `GET /skills/{id}/policy`.
#[derive(Debug, Deserialize, Serialize)]
struct PolicyApiResponse {
    skill_id: String,
    status: String, // "active" | "blocked"
    channel: Option<String>,
    target_version: Option<String>,
    minimum_allowed_version: Option<String>,
    blocked_message: Option<String>,
    policy_ttl_seconds: Option<u64>,
}

/// Wire format returned by `GET /skills/{id}/versions/{version}`.
#[derive(Debug, Deserialize)]
pub struct ArtifactMetadata {
    pub skill_id: String,
    pub version: String,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: Option<u64>,
}

// ── RegistryClient ────────────────────────────────────────────────────────────

/// Pure HTTP client for the SkillClub registry.
///
/// Handles policy lookup, artifact metadata, and package downloads.
/// Has no local state — use [`HttpPolicyClient`] for cached policy.
pub struct RegistryClient {
    pub base_url: String,
    http: reqwest::blocking::Client,
}

impl RegistryClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client should build"),
        }
    }

    /// Fetch policy from the registry.
    ///
    /// Returns the parsed `Policy` and the TTL in seconds to use for caching.
    pub fn fetch_policy_remote(&self, skill_id: &str) -> Result<(Policy, u64)> {
        let url = format!(
            "{}/skills/{}/policy",
            self.base_url.trim_end_matches('/'),
            skill_id
        );
        debug!(url, "fetching policy from registry");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for policy {skill_id}: {body}");
        }

        let wire: PolicyApiResponse = resp
            .json()
            .context("failed to deserialize policy response")?;

        let ttl = wire.policy_ttl_seconds.unwrap_or(86400);
        let policy = policy_from_wire(wire)?;
        Ok((policy, ttl))
    }

    /// Fetch artifact metadata for a specific skill version.
    pub fn fetch_artifact_metadata(&self, skill_id: &str, version: &str) -> Result<ArtifactMetadata> {
        let url = format!(
            "{}/skills/{}/versions/{}",
            self.base_url.trim_end_matches('/'),
            skill_id,
            version
        );
        debug!(url, "fetching artifact metadata");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for artifact {skill_id}@{version}: {body}");
        }

        resp.json().context("failed to deserialize artifact metadata")
    }

    /// Download an artifact to `dest`, verifying its SHA-256 hash.
    ///
    /// The file at `dest` will be created (or overwritten). On hash mismatch
    /// the download is discarded and an error is returned.
    pub fn download_artifact(
        &self,
        download_url: &str,
        expected_sha256: &str,
        dest: &Utf8Path,
    ) -> Result<()> {
        debug!(url = download_url, dest = %dest, "downloading artifact");

        let mut resp = self
            .http
            .get(download_url)
            .send()
            .with_context(|| format!("failed to download artifact from {download_url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            anyhow::bail!("artifact download returned HTTP {status}");
        }

        let mut hasher = Sha256::new();
        let mut out = std::fs::File::create(dest)
            .with_context(|| format!("failed to create {dest}"))?;

        let mut buf = [0u8; 65536];
        loop {
            let n = resp.read(&mut buf).context("error reading download stream")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).context("error writing download to disk")?;
        }
        drop(out);

        let actual = hex::encode(hasher.finalize());
        if actual != expected_sha256 {
            let _ = std::fs::remove_file(dest);
            anyhow::bail!(
                "artifact hash mismatch: expected {expected_sha256}, got {actual}"
            );
        }

        debug!("artifact hash verified");
        Ok(())
    }
}

// ── HttpPolicyClient ──────────────────────────────────────────────────────────

/// A `PolicyClient` that fetches from the registry and caches results in
/// the local SQLite `policy_cache` table.
///
/// On network failure it falls back to the cached policy if one exists,
/// implementing the spec's 7-day offline grace window.
pub struct HttpPolicyClient {
    registry: RegistryClient,
    db_path: camino::Utf8PathBuf,
}

impl HttpPolicyClient {
    pub fn new(registry: RegistryClient, state: &AppState) -> Self {
        Self {
            registry,
            db_path: state.db_path.clone(),
        }
    }
}

impl PolicyClient for HttpPolicyClient {
    fn fetch_policy(&self, skill_id: &str) -> Result<Policy> {
        let now = unix_now();
        let conn = Connection::open(&self.db_path).context("failed to open state DB")?;

        // 1. Check for a fresh cache entry.
        let cached: Option<(String, i64)> = conn
            .query_row(
                "SELECT policy_json, expires_at FROM policy_cache WHERE skill_id = ?1",
                [skill_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((json, expires_at)) = &cached {
            if *expires_at as u64 > now {
                debug!(skill_id, "policy cache hit");
                let wire: PolicyApiResponse = serde_json::from_str(json)
                    .context("failed to deserialize cached policy")?;
                return policy_from_wire(wire);
            }
            debug!(skill_id, "policy cache expired, fetching fresh");
        }

        // 2. Try to fetch from registry.
        match self.registry.fetch_policy_remote(skill_id) {
            Ok((policy, ttl)) => {
                // Serialize policy back to wire form for cache storage.
                let wire = policy_to_wire(&policy, ttl);
                let json = serde_json::to_string(&wire).context("failed to serialize policy")?;
                let expires_at = now + ttl;

                conn.execute(
                    "INSERT INTO policy_cache (skill_id, policy_json, expires_at, fetched_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(skill_id) DO UPDATE SET
                         policy_json = excluded.policy_json,
                         expires_at  = excluded.expires_at,
                         fetched_at  = excluded.fetched_at",
                    params![skill_id, json, expires_at as i64, now as i64],
                )
                .context("failed to write policy cache")?;

                Ok(policy)
            }
            Err(fetch_err) => {
                // 3. Fallback: use stale cache within 7-day grace window.
                if let Some((json, _)) = cached {
                    let fetched_at: Option<i64> = conn
                        .query_row(
                            "SELECT fetched_at FROM policy_cache WHERE skill_id = ?1",
                            [skill_id],
                            |row| row.get(0),
                        )
                        .optional()?;

                    const GRACE_SECONDS: u64 = 7 * 86400;
                    let within_grace = fetched_at
                        .map(|t| now < t as u64 + GRACE_SECONDS)
                        .unwrap_or(false);

                    if within_grace {
                        warn!(
                            skill_id,
                            error = %fetch_err,
                            "policy fetch failed, using stale cache within grace window"
                        );
                        let wire: PolicyApiResponse = serde_json::from_str(&json)
                            .context("failed to deserialize stale cached policy")?;
                        return policy_from_wire(wire);
                    }
                }
                Err(fetch_err.context(format!("failed to fetch policy for '{skill_id}'")))
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn policy_from_wire(wire: PolicyApiResponse) -> Result<Policy> {
    let status = match wire.status.as_str() {
        "active" => PolicyStatus::Active,
        "blocked" => PolicyStatus::Blocked,
        other => anyhow::bail!("unknown policy status '{other}'"),
    };

    let target_version = wire
        .target_version
        .as_deref()
        .map(Version::parse)
        .transpose()
        .with_context(|| format!("invalid target_version in policy for '{}'", wire.skill_id))?;

    let minimum_allowed_version = wire
        .minimum_allowed_version
        .as_deref()
        .map(Version::parse)
        .transpose()
        .with_context(|| format!("invalid minimum_allowed_version in policy for '{}'", wire.skill_id))?;

    Ok(Policy {
        skill_id: wire.skill_id,
        status,
        target_version,
        minimum_allowed_version,
        blocked_message: wire.blocked_message,
    })
}

fn policy_to_wire(policy: &Policy, ttl: u64) -> PolicyApiResponse {
    PolicyApiResponse {
        skill_id: policy.skill_id.clone(),
        status: match policy.status {
            PolicyStatus::Active => "active".to_string(),
            PolicyStatus::Blocked => "blocked".to_string(),
        },
        channel: None,
        target_version: policy.target_version.as_ref().map(|v| v.to_string()),
        minimum_allowed_version: policy.minimum_allowed_version.as_ref().map(|v| v.to_string()),
        blocked_message: policy.blocked_message.clone(),
        policy_ttl_seconds: Some(ttl),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_secs()
}
