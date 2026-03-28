use camino::Utf8PathBuf;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ManagedConfig {
    pub managed: bool,
    pub org: Option<String>,
    pub registry_url: Option<String>,
    pub api_key: Option<String>,
    #[serde(default = "default_true")]
    pub allow_user_installs: bool,
}

fn default_true() -> bool {
    true
}

fn try_load(path: &Utf8PathBuf) -> Option<ManagedConfig> {
    let contents = std::fs::read_to_string(path).ok()?;
    let config: ManagedConfig = serde_json::from_str(&contents).ok()?;
    if !config.managed {
        return None;
    }
    Some(config)
}

/// Load managed deployment config.
/// Checks /etc/skillclub/managed.json first (IT override), then app data dir's managed.json.
/// Returns None if no managed config is active (file missing or managed=false).
pub fn load_managed_config(state: &crate::state::AppState) -> Option<ManagedConfig> {
    let system_path = Utf8PathBuf::from("/etc/skillclub/managed.json");
    if let Some(config) = try_load(&system_path) {
        return Some(config);
    }

    let app_path = state.root_dir.join("managed.json");
    try_load(&app_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> Utf8PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("managed-test-{name}-{nanos}")),
        )
        .unwrap()
    }

    #[test]
    fn test_load_managed_config_from_file() {
        let root = temp_root("load");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let config_json = r#"{"managed": true, "org": "acme", "registry_url": "https://registry.acme.com", "api_key": "secret"}"#;
        fs::write(state.root_dir.join("managed.json"), config_json).unwrap();

        let config = load_managed_config(&state);
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.org.as_deref(), Some("acme"));
        assert_eq!(
            config.registry_url.as_deref(),
            Some("https://registry.acme.com")
        );
        assert_eq!(config.api_key.as_deref(), Some("secret"));
        assert!(config.allow_user_installs);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_managed_false_returns_none() {
        let root = temp_root("false");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let config_json = r#"{"managed": false, "org": "acme"}"#;
        fs::write(state.root_dir.join("managed.json"), config_json).unwrap();

        let config = load_managed_config(&state);
        assert!(config.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_missing_file_returns_none() {
        let root = temp_root("missing");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let config = load_managed_config(&state);
        assert!(config.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_invalid_json_returns_none() {
        let root = temp_root("invalid");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        fs::write(state.root_dir.join("managed.json"), "not valid json {{{").unwrap();

        let config = load_managed_config(&state);
        assert!(config.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_allow_user_installs_defaults_to_true() {
        let root = temp_root("defaults");
        let state = AppState::bootstrap_in(root.clone()).unwrap();

        let config_json = r#"{"managed": true}"#;
        fs::write(state.root_dir.join("managed.json"), config_json).unwrap();

        let config = load_managed_config(&state).unwrap();
        assert!(config.allow_user_installs);

        let _ = fs::remove_dir_all(&root);
    }
}
