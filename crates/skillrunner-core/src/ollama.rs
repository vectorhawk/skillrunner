use crate::model::{ModelClient, ModelRequest, ModelResponse};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Calls a locally-running Ollama instance via its REST API.
pub struct OllamaClient {
    pub base_url: String,
    pub model: String,
    http: reqwest::blocking::Client,
}

impl OllamaClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            http: reqwest::blocking::Client::new(),
        }
    }
}

// ── Ollama wire types ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct OllamaGenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    system: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'a str>,
}

#[derive(Deserialize)]
struct OllamaGenerateResponse {
    response: String,
    #[serde(default)]
    prompt_eval_count: u64,
    #[serde(default)]
    eval_count: u64,
    // total_duration is nanoseconds; we measure wall-clock ourselves
}

// ── ModelClient impl ──────────────────────────────────────────────────────────

impl ModelClient for OllamaClient {
    fn generate(&self, request: ModelRequest) -> Result<ModelResponse> {
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));

        let body = OllamaGenerateRequest {
            model: &self.model,
            prompt: &request.user_message,
            system: &request.system_prompt,
            stream: false,
            format: if request.json_output { Some("json") } else { None },
        };

        let start = Instant::now();
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .with_context(|| format!("failed to reach Ollama at {url}"))?;

        let latency_ms = start.elapsed().as_millis() as u64;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            anyhow::bail!("Ollama returned HTTP {status}: {text}");
        }

        let ollama_resp: OllamaGenerateResponse = resp
            .json()
            .context("failed to deserialize Ollama response")?;

        Ok(ModelResponse {
            text: ollama_resp.response,
            prompt_tokens: ollama_resp.prompt_eval_count,
            completion_tokens: ollama_resp.eval_count,
            latency_ms,
        })
    }
}
