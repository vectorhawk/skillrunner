use anyhow::Result;

/// A request to generate text from a language model.
#[derive(Clone)]
pub struct ModelRequest {
    /// System prompt (instructions / persona).
    pub system_prompt: String,
    /// User-facing content (resolved step inputs).
    pub user_message: String,
    /// When true the model is asked to return valid JSON.
    pub json_output: bool,
}

/// Identifies which backend produced a model response.
///
/// This is carried through `ModelResponse` so callers can surface
/// "local model" vs "remote model via MCP sampling" to the user.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelSource {
    /// A locally-running Ollama instance. Contains the resolved model name
    /// (e.g. `"llama3.2:8b"`).
    Local(String),
    /// The AI client handled the request via MCP `sampling/createMessage`.
    McpSampling,
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
    /// Which backend produced this response.
    pub source: ModelSource,
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
            source: ModelSource::Local("mock-model".to_string()),
        })
    }
}
