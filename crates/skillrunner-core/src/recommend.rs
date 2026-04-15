use serde::Serialize;

/// Heuristic recommendations derived from a skill's name, description, and system prompt.
/// All fields reflect smart defaults inferred by static pattern matching — no I/O or LLM calls.
#[derive(Debug, Clone, Serialize)]
pub struct Recommendations {
    pub triggers: Vec<String>,
    pub permissions: RecommendedPermissions,
    pub model: RecommendedModel,
    pub execution: RecommendedExecution,
    pub confidence: RecommendationConfidence,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecommendedPermissions {
    /// "none", "registry", or "full"
    pub network: String,
    /// "none", "read-only", or "full"
    pub filesystem: String,
    /// "none", "read", or "read-write"
    pub clipboard: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecommendedModel {
    pub min_params_b: f64,
    pub recommended: Vec<String>,
    /// "error" or "mcp_sampling"
    pub fallback: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecommendedExecution {
    pub timeout_ms: u64,
    pub memory_mb: u64,
    /// "strict" or "relaxed"
    pub sandbox: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum RecommendationConfidence {
    High,
    Medium,
    Low,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Analyse a skill's metadata using static heuristics and return recommended
/// `vh_*` defaults.  Pure and synchronous — safe to call from any context.
pub fn recommend_from_prompt(
    name: &str,
    description: &str,
    system_prompt: &str,
) -> Recommendations {
    let combined = build_combined_text(description, system_prompt);

    let permissions = infer_permissions(&combined);
    let model = infer_model(&combined);
    let execution = infer_execution(&combined, &permissions, model.min_params_b);
    let triggers = build_triggers(name, description);
    let confidence = compute_confidence(&permissions, &model, &execution);

    Recommendations {
        triggers,
        permissions,
        model,
        execution,
        confidence,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn build_combined_text(description: &str, system_prompt: &str) -> String {
    format!("{} {}", description, system_prompt).to_lowercase()
}

// --- Triggers ---------------------------------------------------------------

fn build_triggers(name: &str, description: &str) -> Vec<String> {
    let mut triggers: Vec<String> = Vec::new();

    // Trigger 1: lowercased/rearranged name.
    let name_trigger = rearrange_name(name);
    if name_trigger.len() >= 3 {
        triggers.push(name_trigger);
    }

    // Trigger 2: first sentence of description stripped to a short phrase.
    if !description.is_empty() {
        let first_sentence = extract_first_sentence(description);
        if first_sentence.len() >= 3 && !triggers.contains(&first_sentence) {
            triggers.push(first_sentence);
        }
    }

    // Triggers 3-4: simple help/assist variations.
    let name_lower = name.to_lowercase();
    let help_variant = format!("help with {}", name_lower);
    let assist_variant = format!("assist with {}", name_lower);

    if help_variant.len() >= 3 && !triggers.contains(&help_variant) {
        triggers.push(help_variant);
    }
    if assist_variant.len() >= 3 && !triggers.contains(&assist_variant) {
        triggers.push(assist_variant);
    }

    // Cap at 5.
    triggers.truncate(5);
    triggers
}

/// If the name has exactly two words, swap them (noun-verb rearrangement).
/// Otherwise just return the lowercased name.
fn rearrange_name(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    if parts.len() == 2 {
        format!("{} {}", parts[1].to_lowercase(), parts[0].to_lowercase())
    } else {
        name.to_lowercase()
    }
}

fn extract_first_sentence(text: &str) -> String {
    let sentence = match text.find('.') {
        Some(idx) => &text[..idx],
        None => text,
    };
    sentence.trim().to_lowercase()
}

// --- Permissions ------------------------------------------------------------

fn infer_permissions(combined: &str) -> RecommendedPermissions {
    RecommendedPermissions {
        network: infer_network(combined),
        filesystem: infer_filesystem(combined),
        clipboard: infer_clipboard(combined),
    }
}

fn infer_network(text: &str) -> String {
    // "full" signals take precedence.
    let full_keywords = ["download", "upload", "send request", "external service"];
    for kw in full_keywords {
        if text.contains(kw) {
            return "full".to_string();
        }
    }

    let registry_keywords = [
        "api", "fetch", "http", "url", "endpoint", "webhook", "rest", "graphql", "request",
    ];
    for kw in registry_keywords {
        if text.contains(kw) {
            return "registry".to_string();
        }
    }

    "none".to_string()
}

fn infer_filesystem(text: &str) -> String {
    // "full" signals take precedence.
    let write_keywords = [
        "write file",
        "save",
        "create file",
        "output file",
        "write to",
        "generate file",
    ];
    for kw in write_keywords {
        if text.contains(kw) {
            return "full".to_string();
        }
    }

    let read_keywords = [
        "read file",
        "reads file",
        "load file",
        "parse file",
        "analyze file",
        "open file",
        "read the file",
    ];
    for kw in read_keywords {
        if text.contains(kw) {
            return "read-only".to_string();
        }
    }

    "none".to_string()
}

fn infer_clipboard(text: &str) -> String {
    let rw_keywords = ["clipboard", "paste", "copy to clipboard"];
    for kw in rw_keywords {
        if text.contains(kw) {
            return "read-write".to_string();
        }
    }
    "none".to_string()
}

// --- Model ------------------------------------------------------------------

/// Keywords that push min_params_b to 7.0.
const COMPLEXITY_7: &[&str] = &[
    "code",
    "programming",
    "analysis",
    "reasoning",
    "analyze",
    "evaluate",
    "compare",
    "review",
];

/// Keywords that push min_params_b to 14.0.
const COMPLEXITY_14: &[&str] = &[
    "complex",
    "multi-step",
    "research",
    "comprehensive",
    "detailed analysis",
    "architecture",
];

fn has_complexity_7(text: &str) -> bool {
    COMPLEXITY_7.iter().any(|kw| text.contains(kw))
}

fn has_complexity_14(text: &str) -> bool {
    COMPLEXITY_14.iter().any(|kw| text.contains(kw))
}

fn infer_min_params_b(text: &str, prompt_len: usize) -> f64 {
    // 14.0 threshold checked first (highest wins).
    if has_complexity_14(text) {
        return 14.0;
    }

    // 7.0 threshold.
    if has_complexity_7(text) {
        return 7.0;
    }

    // Short prompt with no complexity → 1.0.
    if prompt_len < 500 {
        return 1.0;
    }

    // Long prompt default.
    if prompt_len > 2000 {
        return 7.0;
    }

    3.0
}

fn recommended_models(min_params_b: f64) -> Vec<String> {
    if min_params_b <= 3.0 {
        vec!["gemma3:4b".to_string()]
    } else if min_params_b <= 8.0 {
        vec!["llama3.2:8b".to_string(), "gemma3:4b".to_string()]
    } else if min_params_b <= 14.0 {
        vec!["llama3.1:8b".to_string(), "mistral:7b".to_string()]
    } else {
        vec!["llama3.1:70b".to_string()]
    }
}

fn infer_model(combined: &str) -> RecommendedModel {
    let prompt_len = combined.len();
    let min_params_b = infer_min_params_b(combined, prompt_len);
    let recommended = recommended_models(min_params_b);
    let fallback = if min_params_b > 7.0 {
        "mcp_sampling".to_string()
    } else {
        "error".to_string()
    };

    RecommendedModel {
        min_params_b,
        recommended,
        fallback,
    }
}

// --- Execution --------------------------------------------------------------

fn infer_timeout_ms(combined: &str, prompt_len: usize) -> u64 {
    let long_keywords = ["multi-step", "research", "comprehensive", "detailed"];
    for kw in long_keywords {
        if combined.contains(kw) {
            return 120_000;
        }
    }

    if prompt_len < 500 && !has_complexity_7(combined) {
        return 30_000;
    }

    60_000
}

fn infer_memory_mb(min_params_b: f64) -> u64 {
    if min_params_b <= 3.0 {
        256
    } else if min_params_b <= 8.0 {
        512
    } else {
        1024
    }
}

fn infer_execution(
    combined: &str,
    permissions: &RecommendedPermissions,
    min_params_b: f64,
) -> RecommendedExecution {
    let prompt_len = combined.len();
    let timeout_ms = infer_timeout_ms(combined, prompt_len);
    let memory_mb = infer_memory_mb(min_params_b);
    let sandbox = if permissions.network != "none" || permissions.filesystem != "none" {
        "relaxed".to_string()
    } else {
        "strict".to_string()
    };

    RecommendedExecution {
        timeout_ms,
        memory_mb,
        sandbox,
    }
}

// --- Confidence -------------------------------------------------------------

/// Count the number of non-default signal values across all recommendation fields.
fn count_signals(
    permissions: &RecommendedPermissions,
    model: &RecommendedModel,
    execution: &RecommendedExecution,
) -> usize {
    let mut count = 0;

    if permissions.network != "none" {
        count += 1;
    }
    if permissions.filesystem != "none" {
        count += 1;
    }
    if permissions.clipboard != "none" {
        count += 1;
    }
    // Default min_params_b is 1.0 (short prompt, no keywords); anything else is a signal.
    if model.min_params_b > 1.0 {
        count += 1;
    }
    if model.fallback != "error" {
        count += 1;
    }
    if execution.timeout_ms != 30_000 {
        count += 1;
    }
    if execution.memory_mb != 256 {
        count += 1;
    }
    if execution.sandbox != "strict" {
        count += 1;
    }

    count
}

fn compute_confidence(
    permissions: &RecommendedPermissions,
    model: &RecommendedModel,
    execution: &RecommendedExecution,
) -> RecommendationConfidence {
    let signals = count_signals(permissions, model, execution);
    if signals >= 4 {
        RecommendationConfidence::High
    } else if signals >= 2 {
        RecommendationConfidence::Medium
    } else {
        RecommendationConfidence::Low
    }
}

// ---------------------------------------------------------------------------
// LLM-assisted trigger generation
// ---------------------------------------------------------------------------

/// Generate richer trigger phrases using a local LLM.
/// Falls back to heuristic triggers if the LLM call fails or returns unparseable output.
///
/// The `model_client` is called synchronously (the trait is not async). Callers
/// in async contexts should use `tokio::task::spawn_blocking` if blocking is a concern.
pub fn recommend_triggers_with_llm(
    name: &str,
    description: &str,
    system_prompt: &str,
    model_client: &dyn crate::model::ModelClient,
) -> Vec<String> {
    let analysis_prompt = format!(
        "You are analyzing an AI skill to generate trigger phrases. \
         Trigger phrases are short natural-language descriptions of situations \
         where this skill should be invoked.\n\n\
         Skill name: {name}\n\
         Description: {description}\n\
         System prompt: {system_prompt}\n\n\
         Generate 3-5 trigger phrases. Each should be 3-10 words, lowercase, \
         describing when a user would want this skill.\n\n\
         Respond with ONLY a JSON array of strings. Example:\n\
         [\"compare two contracts\", \"find differences in legal docs\", \"review agreement changes\"]\n\n\
         JSON array:"
    );

    let request = crate::model::ModelRequest {
        system_prompt: String::new(),
        user_message: analysis_prompt,
        json_output: false,
        prefer_local: false,
    };

    match model_client.generate(request) {
        Ok(response) => parse_trigger_response(&response.text)
            .unwrap_or_else(|| build_triggers(name, description)),
        Err(_) => {
            // LLM unavailable — fall back to heuristic triggers.
            build_triggers(name, description)
        }
    }
}

/// Try to extract a JSON array of strings from an LLM response.
/// Returns None if parsing fails.
fn parse_trigger_response(response: &str) -> Option<Vec<String>> {
    let trimmed = response.trim();

    // Try direct parse first.
    if let Ok(arr) = serde_json::from_str::<Vec<String>>(trimmed) {
        return Some(filter_triggers(arr));
    }

    // Try to find a JSON array within the response (handles markdown fences and prose wrappers).
    if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            if end > start {
                let slice = &trimmed[start..=end];
                if let Ok(arr) = serde_json::from_str::<Vec<String>>(slice) {
                    return Some(filter_triggers(arr));
                }
            }
        }
    }

    None
}

fn filter_triggers(raw: Vec<String>) -> Vec<String> {
    raw.into_iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| s.len() >= 3 && s.len() <= 200)
        .take(10)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test Vector 1: API Fetcher
    //   Combined text is short (~130 chars), contains REST/API/endpoint keywords.
    //   No complexity keywords, so min_params_b = 1.0.
    //   Signals: network="registry"(1), sandbox="relaxed"(2) → Medium.
    // -----------------------------------------------------------------------
    #[test]
    fn tv1_api_fetcher() {
        let rec = recommend_from_prompt(
            "API Fetcher",
            "Fetches data from REST APIs and formats the response",
            "You are a helper that fetches data from REST API endpoints. Parse the JSON response and format it for the user.",
        );

        assert_eq!(rec.permissions.network, "registry", "TV1: network");
        assert_eq!(rec.permissions.filesystem, "none", "TV1: filesystem");
        assert_eq!(rec.permissions.clipboard, "none", "TV1: clipboard");
        assert_eq!(rec.model.min_params_b, 1.0, "TV1: min_params_b");
        assert_eq!(
            rec.model.recommended,
            vec!["gemma3:4b"],
            "TV1: recommended models"
        );
        assert_eq!(rec.model.fallback, "error", "TV1: fallback");
        assert_eq!(rec.execution.timeout_ms, 30_000, "TV1: timeout_ms");
        assert_eq!(rec.execution.memory_mb, 256, "TV1: memory_mb");
        assert_eq!(rec.execution.sandbox, "relaxed", "TV1: sandbox");
        assert_eq!(
            rec.confidence,
            RecommendationConfidence::Medium,
            "TV1: confidence"
        );
    }

    // -----------------------------------------------------------------------
    // Test Vector 2: Simple Text Formatter
    //   Short prompt, no signal keywords → all defaults.
    //   Signals: 0 → Low.
    // -----------------------------------------------------------------------
    #[test]
    fn tv2_text_formatter() {
        let rec = recommend_from_prompt(
            "Text Formatter",
            "Formats text into bullet points",
            "You format the user's text into clean bullet points.",
        );

        assert_eq!(rec.permissions.network, "none", "TV2: network");
        assert_eq!(rec.permissions.filesystem, "none", "TV2: filesystem");
        assert_eq!(rec.permissions.clipboard, "none", "TV2: clipboard");
        assert_eq!(rec.model.min_params_b, 1.0, "TV2: min_params_b");
        assert_eq!(
            rec.model.recommended,
            vec!["gemma3:4b"],
            "TV2: recommended models"
        );
        assert_eq!(rec.model.fallback, "error", "TV2: fallback");
        assert_eq!(rec.execution.timeout_ms, 30_000, "TV2: timeout_ms");
        assert_eq!(rec.execution.memory_mb, 256, "TV2: memory_mb");
        assert_eq!(rec.execution.sandbox, "strict", "TV2: sandbox");
        assert_eq!(
            rec.confidence,
            RecommendationConfidence::Low,
            "TV2: confidence"
        );
    }

    // -----------------------------------------------------------------------
    // Test Vector 3: Code Reviewer
    //   Contains "detailed analysis" → min_params_b = 14.0.
    //   Contains "detailed" → timeout = 120_000.
    //   No network/filesystem keywords.
    //   Signals: min_params_b(1), fallback="mcp_sampling"(2),
    //            timeout(3), memory(4) → High.
    // -----------------------------------------------------------------------
    #[test]
    fn tv3_code_reviewer() {
        let rec = recommend_from_prompt(
            "Code Reviewer",
            "Reviews code for bugs, security issues, and best practices",
            "You are an expert code reviewer. Analyze the provided source code for bugs, \
             security vulnerabilities, performance issues, and adherence to best practices. \
             Provide detailed analysis with line-by-line feedback. Consider edge cases, \
             error handling, and architectural concerns.",
        );

        assert_eq!(rec.permissions.network, "none", "TV3: network");
        assert_eq!(rec.permissions.filesystem, "none", "TV3: filesystem");
        assert_eq!(rec.model.min_params_b, 14.0, "TV3: min_params_b");
        assert_eq!(
            rec.model.recommended,
            vec!["llama3.1:8b", "mistral:7b"],
            "TV3: recommended models"
        );
        assert_eq!(rec.model.fallback, "mcp_sampling", "TV3: fallback");
        assert_eq!(rec.execution.timeout_ms, 120_000, "TV3: timeout_ms");
        assert_eq!(rec.execution.memory_mb, 1024, "TV3: memory_mb");
        assert_eq!(rec.execution.sandbox, "strict", "TV3: sandbox");
        assert_eq!(
            rec.confidence,
            RecommendationConfidence::High,
            "TV3: confidence"
        );
    }

    // -----------------------------------------------------------------------
    // Test Vector 4: File Analyzer
    //   Description "reads files" → matches "reads file" keyword → filesystem="read-only".
    //   "analyze" keyword → min_params_b = 7.0.
    //   7.0 is NOT > 7.0 → fallback = "error".
    //   sandbox = "relaxed" (filesystem != "none").
    //   Signals: filesystem(1), min_params_b(2), sandbox(3), memory=512(4) → High.
    // -----------------------------------------------------------------------
    #[test]
    fn tv4_file_analyzer() {
        let rec = recommend_from_prompt(
            "File Analyzer",
            "Reads files and generates summaries",
            "You read the provided file contents and create a concise summary. Analyze the structure and key points.",
        );

        assert_eq!(rec.permissions.network, "none", "TV4: network");
        assert_eq!(rec.permissions.filesystem, "read-only", "TV4: filesystem");
        assert_eq!(rec.model.min_params_b, 7.0, "TV4: min_params_b");
        assert_eq!(rec.model.fallback, "error", "TV4: fallback");
        assert_eq!(rec.execution.timeout_ms, 60_000, "TV4: timeout_ms");
        assert_eq!(rec.execution.memory_mb, 512, "TV4: memory_mb");
        assert_eq!(rec.execution.sandbox, "relaxed", "TV4: sandbox");
        assert_eq!(
            rec.confidence,
            RecommendationConfidence::High,
            "TV4: confidence"
        );
    }

    // -----------------------------------------------------------------------
    // Additional unit tests for individual heuristics
    // -----------------------------------------------------------------------

    #[test]
    fn network_full_wins_over_registry() {
        let combined = "download files from an api endpoint";
        assert_eq!(infer_network(combined), "full");
    }

    #[test]
    fn network_registry_when_no_full() {
        let combined = "calls a rest api to fetch weather data";
        assert_eq!(infer_network(combined), "registry");
    }

    #[test]
    fn filesystem_write_wins_over_read() {
        let combined = "read file contents and write file output";
        assert_eq!(infer_filesystem(combined), "full");
    }

    #[test]
    fn filesystem_read_only_explicit_phrase() {
        let combined = "parse file contents from disk";
        assert_eq!(infer_filesystem(combined), "read-only");
    }

    #[test]
    fn clipboard_detected() {
        let combined = "paste text from clipboard and summarize";
        assert_eq!(infer_clipboard(combined), "read-write");
    }

    #[test]
    fn min_params_b_14_on_detailed_analysis() {
        let text = "provide a detailed analysis of the codebase";
        assert_eq!(infer_min_params_b(text, text.len()), 14.0);
    }

    #[test]
    fn min_params_b_7_on_code_keyword() {
        let text = "review code and find bugs";
        assert_eq!(infer_min_params_b(text, text.len()), 7.0);
    }

    #[test]
    fn min_params_b_1_short_no_keywords() {
        let text = "format this text nicely";
        assert_eq!(infer_min_params_b(text, text.len()), 1.0);
    }

    #[test]
    fn rearrange_name_two_words() {
        assert_eq!(rearrange_name("Contract Compare"), "compare contract");
    }

    #[test]
    fn rearrange_name_single_word() {
        assert_eq!(rearrange_name("Summarizer"), "summarizer");
    }

    #[test]
    fn rearrange_name_three_words() {
        assert_eq!(rearrange_name("PDF File Processor"), "pdf file processor");
    }

    #[test]
    fn triggers_capped_at_five() {
        let rec = recommend_from_prompt(
            "A B",
            "A very long first sentence about processing things and doing stuff in detail.",
            "Does many things.",
        );
        assert!(rec.triggers.len() <= 5, "triggers must be capped at 5");
    }

    #[test]
    fn triggers_min_length_three() {
        let rec = recommend_from_prompt("AB", "", "Short.");
        for t in &rec.triggers {
            assert!(t.len() >= 3, "trigger '{}' is shorter than 3 chars", t);
        }
    }

    // -----------------------------------------------------------------------
    // LLM trigger generation tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_trigger_response_valid_json() {
        let response = r#"["compare contracts", "diff legal docs", "review changes"]"#;
        let result = parse_trigger_response(response).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "compare contracts");
    }

    #[test]
    fn parse_trigger_response_with_markdown_fences() {
        let response = "Here are the triggers:\n```json\n[\"trigger one\", \"trigger two\"]\n```";
        let result = parse_trigger_response(response).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn parse_trigger_response_with_extra_text() {
        let response = "Based on the skill, here are triggers: [\"do something\", \"help with task\"] I hope this helps!";
        let result = parse_trigger_response(response).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn parse_trigger_response_invalid_returns_none() {
        let result = parse_trigger_response("This is not JSON at all");
        assert!(result.is_none());
    }

    #[test]
    fn filter_triggers_removes_short_and_long() {
        let raw = vec![
            "ab".to_string(),
            "valid trigger".to_string(),
            "x".repeat(201),
        ];
        let result = filter_triggers(raw);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "valid trigger");
    }

    #[test]
    fn recommend_triggers_with_llm_uses_llm_response() {
        let mock = crate::model::MockModelClient::new(
            r#"["analyze the contract", "compare legal docs", "review agreement"]"#,
        );
        let triggers = recommend_triggers_with_llm(
            "Contract Compare",
            "Compares two contracts",
            "You compare contracts.",
            &mock,
        );
        assert_eq!(triggers.len(), 3);
        assert_eq!(triggers[0], "analyze the contract");
    }

    #[test]
    fn recommend_triggers_with_llm_falls_back_on_bad_response() {
        use anyhow::anyhow;

        struct FailingClient;
        impl crate::model::ModelClient for FailingClient {
            fn generate(
                &self,
                _request: crate::model::ModelRequest,
            ) -> anyhow::Result<crate::model::ModelResponse> {
                Err(anyhow!("Ollama unavailable"))
            }
        }

        let triggers = recommend_triggers_with_llm(
            "Contract Compare",
            "Compares two contracts",
            "You compare contracts.",
            &FailingClient,
        );
        // Falls back to heuristic — must produce at least one trigger.
        assert!(!triggers.is_empty());
    }

    #[test]
    fn recommend_triggers_with_llm_falls_back_on_unparseable_response() {
        let mock = crate::model::MockModelClient::new("This is not valid JSON at all.");
        let triggers = recommend_triggers_with_llm(
            "Contract Compare",
            "Compares two contracts",
            "You compare contracts.",
            &mock,
        );
        // Falls back to heuristic — should produce the rearranged name trigger.
        assert!(triggers
            .iter()
            .any(|t| t.contains("contract") || t.contains("compare")));
    }

    #[test]
    fn confidence_high_threshold() {
        // Force many non-default values.
        let perms = RecommendedPermissions {
            network: "full".to_string(),
            filesystem: "read-only".to_string(),
            clipboard: "read-write".to_string(),
            // 3 signals already → will be High after model adds more
        };
        let model = RecommendedModel {
            min_params_b: 14.0,
            recommended: vec!["llama3.1:70b".to_string()],
            fallback: "mcp_sampling".to_string(),
        };
        let exec = RecommendedExecution {
            timeout_ms: 120_000,
            memory_mb: 1024,
            sandbox: "relaxed".to_string(),
        };
        assert_eq!(
            compute_confidence(&perms, &model, &exec),
            RecommendationConfidence::High
        );
    }
}
