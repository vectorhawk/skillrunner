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

/// A single skill result from `GET /portal/skills?search=<query>`.
#[derive(Debug, Deserialize, Serialize)]
pub struct SearchResult {
    pub skill_id: String,
    pub name: String,
    pub latest_version: Option<String>,
    pub publisher_name: Option<String>,
    pub description: Option<String>,
}

/// Wire format returned by the search endpoint.
#[derive(Debug, Deserialize)]
struct SearchApiResponse {
    items: Vec<SearchResult>,
}

/// Skill detail returned by `GET /portal/skills/{skill_id}`.
#[derive(Debug, Deserialize)]
pub struct SkillDetail {
    pub skill_id: String,
    pub name: String,
    pub latest_version: Option<String>,
    pub publisher_name: Option<String>,
    pub description: Option<String>,
}

// ── RegistryClient ────────────────────────────────────────────────────────────

/// Pure HTTP client for the SkillClub registry.
///
/// Handles policy lookup, artifact metadata, and package downloads.
/// Has no local state — use [`HttpPolicyClient`] for cached policy.
pub struct RegistryClient {
    pub base_url: String,
    pub http: reqwest::blocking::Client,
    auth_token: Option<String>,
}

impl RegistryClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client should build"),
            auth_token: None,
        }
    }

    /// Set the Bearer auth token for authenticated requests.
    pub fn with_auth(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Set the auth token on an existing client (non-consuming).
    pub fn set_auth(&mut self, token: impl Into<String>) {
        self.auth_token = Some(token.into());
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

    /// Check if the registry is reachable by hitting `GET /health`.
    ///
    /// Returns `Ok(true)` if the registry responds with a success status,
    /// `Ok(false)` if it responds with a non-success status, and `Err` only
    /// on connection/timeout failures.
    pub fn health_check(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;
        Ok(resp.status().is_success())
    }

    /// Search the registry for skills matching `query`.
    pub fn search_skills(&self, query: &str) -> Result<Vec<SearchResult>> {
        let url = format!(
            "{}/portal/skills?search={}",
            self.base_url.trim_end_matches('/'),
            urlencoding::encode(query)
        );
        debug!(url, "searching skills");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for search: {body}");
        }

        let wire: SearchApiResponse = resp
            .json()
            .context("failed to deserialize search response")?;
        Ok(wire.items)
    }

    /// Fetch skill detail including latest version info.
    pub fn fetch_skill_detail(&self, skill_id: &str) -> Result<SkillDetail> {
        let url = format!(
            "{}/portal/skills/{}",
            self.base_url.trim_end_matches('/'),
            skill_id
        );
        debug!(url, "fetching skill detail");

        let resp = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("registry returned HTTP {status} for skill {skill_id}: {body}");
        }

        resp.json()
            .context("failed to deserialize skill detail response")
    }

    /// Upload a `.cskill` archive to the registry.
    ///
    /// Requires auth token to be set via [`with_auth`].
    pub fn publish_skill(&self, archive_path: &Utf8Path) -> Result<serde_json::Value> {
        let token = self
            .auth_token
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not authenticated; run `skillrunner auth login` first"))?;

        let url = format!(
            "{}/portal/skills",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, archive = %archive_path, "uploading skill");

        let file_bytes = std::fs::read(archive_path)
            .with_context(|| format!("failed to read archive {archive_path}"))?;

        let file_name = archive_path
            .file_name()
            .unwrap_or("bundle.cskill")
            .to_string();

        let form = reqwest::blocking::multipart::Form::new().part(
            "file",
            reqwest::blocking::multipart::Part::bytes(file_bytes)
                .file_name(file_name)
                .mime_str("application/octet-stream")
                .context("invalid MIME type")?,
        );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("publish failed (HTTP {status}): {body}");
        }

        resp.json().context("failed to deserialize publish response")
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

        // 1. Always try to fetch fresh from registry first so that policy
        //    changes (e.g. blocking a skill) take effect immediately.
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
                // 2. Fallback: use cached policy within 7-day offline grace window.
                let cached = conn
                    .query_row(
                        "SELECT policy_json, fetched_at FROM policy_cache WHERE skill_id = ?1",
                        [skill_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?;

                if let Some((json, fetched_at)) = cached {
                    const GRACE_SECONDS: u64 = 7 * 86400;
                    let within_grace = now < fetched_at as u64 + GRACE_SECONDS;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use camino::Utf8PathBuf;
    use mockito::{Matcher, Server};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("forge-tests-registry-{label}-{nanos}")),
        )
        .unwrap()
    }

    // ── RegistryClient: fetch_policy_remote ────────────────────────────────

    #[test]
    fn fetch_policy_remote_parses_active_policy() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/my-skill/policy")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "skill_id": "my-skill",
                    "status": "active",
                    "target_version": "1.2.0",
                    "minimum_allowed_version": "1.0.0",
                    "policy_ttl_seconds": 3600
                }"#,
            )
            .create();

        let client = RegistryClient::new(server.url());
        let (policy, ttl) = client.fetch_policy_remote("my-skill").unwrap();

        assert_eq!(policy.skill_id, "my-skill");
        assert_eq!(policy.status, PolicyStatus::Active);
        assert_eq!(
            policy.target_version,
            Some(Version::parse("1.2.0").unwrap())
        );
        assert_eq!(
            policy.minimum_allowed_version,
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(ttl, 3600);
        mock.assert();
    }

    #[test]
    fn fetch_policy_remote_parses_blocked_policy() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/bad-skill/policy")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "skill_id": "bad-skill",
                    "status": "blocked",
                    "blocked_message": "security vulnerability"
                }"#,
            )
            .create();

        let client = RegistryClient::new(server.url());
        let (policy, ttl) = client.fetch_policy_remote("bad-skill").unwrap();

        assert_eq!(policy.status, PolicyStatus::Blocked);
        assert_eq!(
            policy.blocked_message.as_deref(),
            Some("security vulnerability")
        );
        assert_eq!(ttl, 86400); // default TTL
        mock.assert();
    }

    #[test]
    fn fetch_policy_remote_returns_error_on_http_failure() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/my-skill/policy")
            .with_status(500)
            .with_body("internal error")
            .create();

        let client = RegistryClient::new(server.url());
        let err = client.fetch_policy_remote("my-skill").unwrap_err();
        assert!(err.to_string().contains("500"));
        mock.assert();
    }

    #[test]
    fn fetch_policy_remote_returns_error_on_malformed_json() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/my-skill/policy")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not json at all")
            .create();

        let client = RegistryClient::new(server.url());
        let err = client.fetch_policy_remote("my-skill").unwrap_err();
        assert!(err.to_string().contains("deserialize"));
        mock.assert();
    }

    // ── RegistryClient: fetch_artifact_metadata ────────────────────────────

    #[test]
    fn fetch_artifact_metadata_parses_response() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/my-skill/versions/1.0.0")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "skill_id": "my-skill",
                    "version": "1.0.0",
                    "download_url": "https://cdn.example.com/my-skill-1.0.0.cskill",
                    "sha256": "abcdef1234567890",
                    "size_bytes": 12345
                }"#,
            )
            .create();

        let client = RegistryClient::new(server.url());
        let meta = client.fetch_artifact_metadata("my-skill", "1.0.0").unwrap();

        assert_eq!(meta.skill_id, "my-skill");
        assert_eq!(meta.version, "1.0.0");
        assert_eq!(meta.sha256, "abcdef1234567890");
        assert_eq!(meta.size_bytes, Some(12345));
        mock.assert();
    }

    #[test]
    fn fetch_artifact_metadata_returns_error_on_http_failure() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/my-skill/versions/1.0.0")
            .with_status(404)
            .with_body("not found")
            .create();

        let client = RegistryClient::new(server.url());
        let err = client
            .fetch_artifact_metadata("my-skill", "1.0.0")
            .unwrap_err();
        assert!(err.to_string().contains("404"));
        mock.assert();
    }

    // ── RegistryClient: download_artifact ──────────────────────────────────

    #[test]
    fn download_artifact_verifies_sha256() {
        let content = b"hello world skill bundle";
        let expected_hash = hex::encode(sha2::Sha256::digest(content));

        let mut server = Server::new();
        let mock = server
            .mock("GET", "/download/bundle.cskill")
            .with_status(200)
            .with_body(content.as_slice())
            .create();

        let tmp = tempfile::TempDir::new().unwrap();
        let dest = Utf8PathBuf::from_path_buf(tmp.path().join("out.cskill")).unwrap();

        let client = RegistryClient::new(server.url());
        let download_url = format!("{}/download/bundle.cskill", server.url());
        client
            .download_artifact(&download_url, &expected_hash, &dest)
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), content);
        mock.assert();
    }

    #[test]
    fn download_artifact_rejects_hash_mismatch() {
        let content = b"hello world skill bundle";

        let mut server = Server::new();
        let mock = server
            .mock("GET", "/download/bundle.cskill")
            .with_status(200)
            .with_body(content.as_slice())
            .create();

        let tmp = tempfile::TempDir::new().unwrap();
        let dest = Utf8PathBuf::from_path_buf(tmp.path().join("out.cskill")).unwrap();

        let client = RegistryClient::new(server.url());
        let download_url = format!("{}/download/bundle.cskill", server.url());
        let err = client
            .download_artifact(&download_url, "badhash000", &dest)
            .unwrap_err();

        assert!(err.to_string().contains("hash mismatch"));
        assert!(!dest.exists(), "file should be cleaned up on mismatch");
        mock.assert();
    }

    // ── RegistryClient: health_check ───────────────────────────────────────

    #[test]
    fn health_check_returns_true_when_healthy() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/health")
            .with_status(200)
            .with_body("ok")
            .create();

        let client = RegistryClient::new(server.url());
        assert!(client.health_check().unwrap());
        mock.assert();
    }

    #[test]
    fn health_check_returns_false_on_server_error() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/health")
            .with_status(503)
            .create();

        let client = RegistryClient::new(server.url());
        assert!(!client.health_check().unwrap());
        mock.assert();
    }

    #[test]
    fn health_check_returns_error_on_connection_failure() {
        let client = RegistryClient::new("http://127.0.0.1:1");
        assert!(client.health_check().is_err());
    }

    // ── RegistryClient: search_skills ──────────────────────────────────────

    #[test]
    fn search_skills_parses_results() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/portal/skills")
            .match_query(Matcher::UrlEncoded("search".into(), "contract".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "items": [
                        { "skill_id": "contract-compare", "name": "Contract Compare", "latest_version": "0.1.0", "publisher_name": "skillclub" },
                        { "skill_id": "contract-review", "name": "Contract Review", "latest_version": "1.0.0", "publisher_name": "acme" }
                    ],
                    "total": 2,
                    "page": 1,
                    "page_size": 20
                }"#,
            )
            .create();

        let client = RegistryClient::new(server.url());
        let results = client.search_skills("contract").unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].skill_id, "contract-compare");
        assert_eq!(results[1].publisher_name.as_deref(), Some("acme"));
        mock.assert();
    }

    #[test]
    fn search_skills_returns_error_on_http_failure() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/portal/skills")
            .match_query(Matcher::UrlEncoded("search".into(), "test".into()))
            .with_status(500)
            .with_body("internal error")
            .create();

        let client = RegistryClient::new(server.url());
        let err = client.search_skills("test").unwrap_err();
        assert!(err.to_string().contains("500"));
        mock.assert();
    }

    // ── HttpPolicyClient: cache behaviour ──────────────────────────────────

    #[test]
    fn http_policy_always_fetches_fresh_from_network() {
        let mut server = Server::new();
        // Both calls should hit the network so that policy changes (e.g.
        // blocking a skill) are reflected immediately without waiting for TTL.
        let mock = server
            .mock("GET", "/skills/cached-skill/policy")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "skill_id": "cached-skill",
                    "status": "active",
                    "policy_ttl_seconds": 86400
                }"#,
            )
            .expect(2)
            .create();

        let root = temp_root("cache-hit");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let registry = RegistryClient::new(server.url());
        let client = HttpPolicyClient::new(registry, &state);

        let p1 = client.fetch_policy("cached-skill").unwrap();
        assert_eq!(p1.status, PolicyStatus::Active);

        // Second call also hits the network — picks up any policy changes.
        let p2 = client.fetch_policy("cached-skill").unwrap();
        assert_eq!(p2.status, PolicyStatus::Active);

        mock.assert(); // exactly 2 calls
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn http_policy_cache_miss_fetches_and_stores() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/skills/new-skill/policy")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "skill_id": "new-skill",
                    "status": "active",
                    "target_version": "2.0.0",
                    "policy_ttl_seconds": 600
                }"#,
            )
            .expect(1)
            .create();

        let root = temp_root("cache-miss");
        let state = AppState::bootstrap_in(root.clone()).unwrap();
        let registry = RegistryClient::new(server.url());
        let client = HttpPolicyClient::new(registry, &state);

        let policy = client.fetch_policy("new-skill").unwrap();
        assert_eq!(policy.target_version, Some(Version::parse("2.0.0").unwrap()));

        // Verify cache row was written
        let conn = Connection::open(&state.db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM policy_cache WHERE skill_id = 'new-skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        mock.assert();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn http_policy_stale_cache_fallback_within_grace_window() {
        let root = temp_root("stale-grace");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        // Manually insert a stale cache entry (expired but within 7-day grace)
        let now = unix_now();
        let wire_json = serde_json::to_string(&PolicyApiResponse {
            skill_id: "stale-skill".to_string(),
            status: "active".to_string(),
            channel: None,
            target_version: Some("1.0.0".to_string()),
            minimum_allowed_version: None,
            blocked_message: None,
            policy_ttl_seconds: Some(60),
        })
        .unwrap();

        let conn = Connection::open(&state.db_path).unwrap();
        conn.execute(
            "INSERT INTO policy_cache (skill_id, policy_json, expires_at, fetched_at) VALUES (?1, ?2, ?3, ?4)",
            params!["stale-skill", wire_json, (now - 10) as i64, now as i64],
        )
        .unwrap();
        drop(conn);

        // Point at an unreachable server to simulate network failure
        let registry = RegistryClient::new("http://127.0.0.1:1");
        let client = HttpPolicyClient::new(registry, &state);

        let policy = client.fetch_policy("stale-skill").unwrap();
        assert_eq!(policy.status, PolicyStatus::Active);
        assert_eq!(
            policy.target_version,
            Some(Version::parse("1.0.0").unwrap())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn http_policy_error_when_no_cache_and_network_fails() {
        let root = temp_root("no-cache-fail");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let registry = RegistryClient::new("http://127.0.0.1:1");
        let client = HttpPolicyClient::new(registry, &state);

        let err = client.fetch_policy("ghost-skill").unwrap_err();
        assert!(err.to_string().contains("failed to fetch policy"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
