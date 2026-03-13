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
