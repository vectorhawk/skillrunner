use anyhow::Result;

/// A request to generate text from a language model.
pub struct ModelRequest {
    /// System prompt (instructions / persona).
    pub system_prompt: String,
    /// User-facing content (resolved step inputs).
    pub user_message: String,
    /// When true the model is asked to return valid JSON.
    pub json_output: bool,
}

/// The raw response returned by the model, including accounting data.
#[derive(Debug)]
pub struct ModelResponse {
    /// Raw text (or JSON string) produced by the model.
    pub text: String,
    /// Number of tokens in the prompt.
    pub prompt_tokens: u64,
    /// Number of tokens in the completion.
    pub completion_tokens: u64,
    /// Wall-clock time for the call in milliseconds.
    pub latency_ms: u64,
}

/// Abstraction over any text-generation backend.
pub trait ModelClient: Send + Sync {
    fn generate(&self, request: ModelRequest) -> Result<ModelResponse>;
}

/// A mock model client that returns configurable responses. Useful for testing
/// the LLM execution path without a real model backend.
pub struct MockModelClient {
    response_text: String,
    prompt_tokens: u64,
    completion_tokens: u64,
}

impl MockModelClient {
    pub fn new(response_text: impl Into<String>) -> Self {
        Self {
            response_text: response_text.into(),
            prompt_tokens: 10,
            completion_tokens: 5,
        }
    }

    pub fn with_tokens(mut self, prompt: u64, completion: u64) -> Self {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
        self
    }
}

impl ModelClient for MockModelClient {
    fn generate(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: self.response_text.clone(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            latency_ms: 1,
        })
    }
}
