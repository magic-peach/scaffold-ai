//! Request/response shapes for the Anthropic Messages API.

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct MessagesRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub system: &'a str,
    pub messages: Vec<Message<'a>>,
    pub output_config: OutputConfig,
}

#[derive(Serialize)]
pub struct Message<'a> {
    pub role: &'a str,
    pub content: String,
}

#[derive(Serialize)]
pub struct OutputConfig {
    pub format: OutputFormat,
}

#[derive(Serialize)]
pub struct OutputFormat {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub schema: serde_json::Value,
}

#[derive(Deserialize)]
pub struct MessagesResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}
