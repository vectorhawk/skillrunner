use crate::state::AppState;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tracing::debug;

// ── Wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "bearer".to_string()
}

#[derive(Debug, Serialize)]
struct RefreshRequest {
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub email: String,
    pub display_name: String,
}

// ── Stored tokens ────────────────────────────────────────────────────────────

pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub registry_url: String,
}

// ── AuthClient ───────────────────────────────────────────────────────────────

pub struct AuthClient {
    base_url: String,
    http: reqwest::blocking::Client,
}

impl AuthClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client should build"),
        }
    }

    pub fn login(&self, email: &str, password: &str) -> Result<TokenResponse> {
        let url = format!(
            "{}/portal/auth/login",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, "logging in");

        let resp = self
            .http
            .post(&url)
            .json(&LoginRequest {
                email: email.to_string(),
                password: password.to_string(),
            })
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("login failed (HTTP {status}): {body}");
        }

        resp.json().context("failed to deserialize login response")
    }

    pub fn refresh(&self, refresh_token: &str) -> Result<TokenResponse> {
        let url = format!(
            "{}/portal/auth/refresh",
            self.base_url.trim_end_matches('/')
        );
        debug!(url, "refreshing token");

        let resp = self
            .http
            .post(&url)
            .json(&RefreshRequest {
                refresh_token: refresh_token.to_string(),
            })
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("token refresh failed (HTTP {status}): {body}");
        }

        resp.json()
            .context("failed to deserialize refresh response")
    }

    pub fn me(&self, access_token: &str) -> Result<UserInfo> {
        let url = format!(
            "{}/portal/auth/me",
            self.base_url.trim_end_matches('/')
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .with_context(|| format!("failed to reach registry at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("auth check failed (HTTP {status}): {body}");
        }

        resp.json().context("failed to deserialize user info")
    }
}

// ── Token storage ────────────────────────────────────────────────────────────

pub fn save_tokens(
    state: &AppState,
    registry_url: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    conn.execute(
        "INSERT INTO auth_tokens (registry_url, access_token, refresh_token)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(registry_url) DO UPDATE SET
             access_token = excluded.access_token,
             refresh_token = excluded.refresh_token,
             saved_at = CURRENT_TIMESTAMP",
        params![registry_url, access_token, refresh_token],
    )
    .context("failed to save auth tokens")?;
    Ok(())
}

pub fn load_tokens(state: &AppState, registry_url: &str) -> Result<Option<StoredTokens>> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    let result = conn
        .query_row(
            "SELECT access_token, refresh_token FROM auth_tokens WHERE registry_url = ?1",
            [registry_url],
            |row| {
                Ok(StoredTokens {
                    access_token: row.get(0)?,
                    refresh_token: row.get(1)?,
                    registry_url: registry_url.to_string(),
                })
            },
        )
        .optional()?;
    Ok(result)
}

pub fn clear_tokens(state: &AppState, registry_url: &str) -> Result<()> {
    let conn = Connection::open(&state.db_path).context("failed to open state DB")?;
    conn.execute(
        "DELETE FROM auth_tokens WHERE registry_url = ?1",
        [registry_url],
    )
    .context("failed to clear auth tokens")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use camino::Utf8PathBuf;
    use mockito::Server;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("forge-tests-auth-{label}-{nanos}")),
        )
        .unwrap()
    }

    #[test]
    fn login_returns_tokens_on_success() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/portal/auth/login")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"access_token":"acc123","refresh_token":"ref456","token_type":"bearer"}"#,
            )
            .create();

        let client = AuthClient::new(server.url());
        let resp = client.login("test@example.com", "password").unwrap();
        assert_eq!(resp.access_token, "acc123");
        assert_eq!(resp.refresh_token, "ref456");
        mock.assert();
    }

    #[test]
    fn login_returns_error_on_401() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/portal/auth/login")
            .with_status(401)
            .with_body(r#"{"detail":"Invalid credentials"}"#)
            .create();

        let client = AuthClient::new(server.url());
        let err = client.login("bad@example.com", "wrong").unwrap_err();
        assert!(err.to_string().contains("401"));
        mock.assert();
    }

    #[test]
    fn refresh_returns_new_tokens() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/portal/auth/refresh")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"access_token":"new_acc","refresh_token":"new_ref","token_type":"bearer"}"#,
            )
            .create();

        let client = AuthClient::new(server.url());
        let resp = client.refresh("old_ref").unwrap();
        assert_eq!(resp.access_token, "new_acc");
        mock.assert();
    }

    #[test]
    fn save_and_load_tokens_roundtrip() {
        let root = temp_root("token-roundtrip");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        save_tokens(&state, "http://localhost:8000", "acc", "ref").unwrap();
        let loaded = load_tokens(&state, "http://localhost:8000").unwrap().unwrap();
        assert_eq!(loaded.access_token, "acc");
        assert_eq!(loaded.refresh_token, "ref");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_tokens_removes_entry() {
        let root = temp_root("token-clear");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        save_tokens(&state, "http://localhost:8000", "acc", "ref").unwrap();
        clear_tokens(&state, "http://localhost:8000").unwrap();
        let loaded = load_tokens(&state, "http://localhost:8000").unwrap();
        assert!(loaded.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }
}
