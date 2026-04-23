//! Thin HTTP client for querying scan verdicts from the registry.
//!
//! The scan client is strictly **detect-and-alert** — it never blocks an
//! operation.  Network errors and 404s are treated as "no verdict available"
//! (`Ok(None)`), so callers can always proceed.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// ── Wire types ──────────────────────────────────────────────────────────────

/// Aggregated scan verdict returned by `GET /admin/scan/{content_hash}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanVerdict {
    /// Severity level: clean, info, low, medium, high, critical, unknown.
    pub verdict: String,
    /// Scanner engine identifier.
    pub engine: String,
    /// Scanner version string.
    pub scanner_version: String,
    /// Individual findings reported by the scanner.
    #[serde(default)]
    pub findings: Vec<ScanFinding>,
}

/// A single finding within a scan report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanFinding {
    pub rule_id: String,
    pub severity: String,
    pub message: String,
    pub location: Option<String>,
}

// ── Trait ────────────────────────────────────────────────────────────────────

/// Abstraction over scan verdict fetching so tests can use mocks.
pub trait ScanClient {
    /// Check the scan verdict for a given content hash.
    ///
    /// Returns `Ok(None)` when no verdict exists (404) or the endpoint is
    /// unreachable (fail-open).  Returns `Ok(Some(verdict))` on success.
    fn check_verdict(&self, content_hash: &str) -> Result<Option<ScanVerdict>>;
}

// ── HTTP implementation ─────────────────────────────────────────────────────

/// HTTP-backed scan client that talks to the registry's `/admin/scan/` endpoint.
pub struct HttpScanClient {
    registry_url: String,
    auth_token: Option<String>,
    http: reqwest::blocking::Client,
}

impl HttpScanClient {
    /// Create a new scan client pointing at `registry_url`.
    pub fn new(registry_url: impl Into<String>) -> Self {
        Self {
            registry_url: registry_url.into(),
            auth_token: None,
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("HTTP client should build"),
        }
    }

    /// Set the Bearer auth token for authenticated requests.
    pub fn with_auth(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }
}

impl ScanClient for HttpScanClient {
    fn check_verdict(&self, content_hash: &str) -> Result<Option<ScanVerdict>> {
        let url = format!(
            "{}/admin/scan/{}",
            self.registry_url.trim_end_matches('/'),
            content_hash
        );
        debug!(url, "checking scan verdict");

        let mut req = self.http.get(&url);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }

        let resp = match req.send() {
            Ok(r) => r,
            Err(e) => {
                // Fail-open: network error → no verdict.
                warn!("scan endpoint unreachable: {e}");
                return Ok(None);
            }
        };

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // No verdict on file for this hash — expected for new content.
            debug!("no scan verdict for hash {content_hash}");
            return Ok(None);
        }

        if !resp.status().is_success() {
            // Non-success status that isn't 404 — treat as unknown (fail-open).
            let status = resp.status();
            warn!("scan endpoint returned HTTP {status} for {content_hash}");
            return Ok(None);
        }

        match resp.json::<ScanVerdict>() {
            Ok(verdict) => Ok(Some(verdict)),
            Err(e) => {
                warn!("failed to deserialize scan verdict: {e}");
                Ok(None)
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Returns `true` when the verdict severity warrants a warning.
pub fn is_risky(verdict: &ScanVerdict) -> bool {
    matches!(
        verdict.verdict.as_str(),
        "medium" | "high" | "critical"
    )
}

/// Returns `true` when the verdict severity requires explicit confirmation.
pub fn requires_confirmation(verdict: &ScanVerdict) -> bool {
    matches!(verdict.verdict.as_str(), "high" | "critical")
}

/// Compute a SHA-256 hex digest of the given bytes.
///
/// Used to derive the content hash for scan lookups.
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Check scan endpoint reachability by sending a probe request.
///
/// Returns `Ok(true)` if the endpoint responds (even with 404), `Ok(false)`
/// on network error.
pub fn check_scan_reachability(registry_url: &str) -> bool {
    let url = format!(
        "{}/admin/scan/test-hash",
        registry_url.trim_end_matches('/')
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(_) => return false,
    };
    // Any HTTP response (including 404) means the endpoint is reachable.
    client.get(&url).send().is_ok()
}
