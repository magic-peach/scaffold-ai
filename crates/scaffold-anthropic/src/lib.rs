//! Thin reqwest client for the Anthropic Messages API.
//!
//! No SDK dependency: Rust has no official Anthropic SDK, so this crate speaks
//! raw HTTP. Every model call uses structured outputs
//! (`output_config.format` with a JSON schema), so responses deserialize
//! directly into `scaffold-domain` structs and malformed output fails loudly.

mod prompts;
mod wire;

use async_trait::async_trait;
use scaffold_domain::{
    Classification, ModelDecision, PolicyFinding, PullRequestSnapshot, RepoConfig, TriageError,
    TriageModel,
};
use serde::de::DeserializeOwned;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-opus-4-8";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 8192;
const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl AnthropicConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
        }
    }
}

pub struct AnthropicTriageModel {
    http: reqwest::Client,
    config: AnthropicConfig,
}

impl AnthropicTriageModel {
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
        }
    }

    /// One structured-output call: send system + user text with a JSON
    /// schema, get back a `T` or a hard error. Retries 429/5xx honoring
    /// `retry-after`.
    async fn structured<T: DeserializeOwned>(
        &self,
        system: &str,
        user: String,
        schema: serde_json::Value,
    ) -> Result<T, TriageError> {
        let body = wire::MessagesRequest {
            model: &self.config.model,
            max_tokens: MAX_TOKENS,
            system,
            messages: vec![wire::Message {
                role: "user",
                content: user,
            }],
            output_config: wire::OutputConfig {
                format: wire::OutputFormat {
                    kind: "json_schema",
                    schema,
                },
            },
        };
        let url = format!("{}/v1/messages", self.config.base_url);

        let mut attempt = 0;
        let response = loop {
            attempt += 1;
            let result = self
                .http
                .post(&url)
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        break resp;
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if retryable && attempt < MAX_ATTEMPTS {
                        let delay = retry_after_secs(&resp).unwrap_or(attempt as u64);
                        tracing::warn!(status = %status, attempt, delay, "retrying Anthropic request");
                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                        continue;
                    }
                    let text = resp.text().await.unwrap_or_default();
                    return Err(TriageError::Model(format!(
                        "Anthropic API returned {status}: {text}"
                    )));
                }
                Err(e) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(error = %e, attempt, "Anthropic request failed, retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
                }
                Err(e) => return Err(TriageError::Model(e.to_string())),
            }
        };

        let parsed: wire::MessagesResponse = response
            .json()
            .await
            .map_err(|e| TriageError::Model(format!("unreadable API response: {e}")))?;

        match parsed.stop_reason.as_deref() {
            Some("refusal") => {
                return Err(TriageError::Model(
                    "model refused the request (stop_reason: refusal)".into(),
                ))
            }
            Some("max_tokens") => {
                return Err(TriageError::InvalidModelOutput(
                    "response truncated at max_tokens; structured output incomplete".into(),
                ))
            }
            _ => {}
        }

        let text = parsed
            .content
            .iter()
            .find_map(|block| match block {
                wire::ContentBlock::Text { text } => Some(text.as_str()),
                wire::ContentBlock::Other => None,
            })
            .ok_or_else(|| {
                TriageError::InvalidModelOutput("no text block in model response".into())
            })?;

        serde_json::from_str(text).map_err(|e| {
            TriageError::InvalidModelOutput(format!("schema deserialization failed: {e}"))
        })
    }
}

fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

#[async_trait]
impl TriageModel for AnthropicTriageModel {
    async fn classify(
        &self,
        snapshot: &PullRequestSnapshot,
    ) -> Result<Classification, TriageError> {
        self.structured(
            prompts::CLASSIFY_SYSTEM,
            prompts::render_snapshot(snapshot),
            prompts::classification_schema(),
        )
        .await
    }

    async fn decide(
        &self,
        snapshot: &PullRequestSnapshot,
        classification: &Classification,
        findings: &[PolicyFinding],
        config: &RepoConfig,
    ) -> Result<ModelDecision, TriageError> {
        self.structured(
            prompts::DECIDE_SYSTEM,
            prompts::render_decision_input(snapshot, classification, findings, config),
            prompts::decision_schema(),
        )
        .await
    }
}
