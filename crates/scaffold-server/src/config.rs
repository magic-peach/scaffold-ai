//! Fail-fast configuration loading.
//!
//! Loads `.env` if present, then validates *everything* up front and reports
//! all problems in one error — a clear "missing GITHUB_APP_ID" beats a
//! cryptic JWT signing failure three layers deep.

use anyhow::{anyhow, Result};

pub struct ServerConfig {
    pub bind_addr: String,
    pub database_url: String,
    pub webhook_secret: String,
    pub github_app_id: u64,
    pub github_private_key_pem: String,
    pub github_base_url: Option<String>,
    pub anthropic_api_key: String,
    pub anthropic_model: Option<String>,
    pub anthropic_base_url: Option<String>,
    pub autumn_secret_key: String,
    pub autumn_base_url: Option<String>,
}

impl ServerConfig {
    /// Load and validate. Collects every problem before failing so one run
    /// surfaces the complete list.
    pub fn load() -> Result<Self> {
        // Best-effort .env loading; a missing file is fine (production sets
        // real env vars), but a malformed one should be surfaced.
        match dotenvy::dotenv() {
            Ok(path) => tracing::info!(path = %path.display(), "loaded .env"),
            Err(e) if e.not_found() => {}
            Err(e) => return Err(anyhow!(".env exists but could not be parsed: {e}")),
        }

        let mut problems: Vec<String> = Vec::new();
        let mut var = |key: &str| -> Option<String> {
            match std::env::var(key) {
                Ok(v) if !v.trim().is_empty() => Some(v),
                _ => {
                    problems.push(format!("missing required env var {key}"));
                    None
                }
            }
        };

        let database_url = var("DATABASE_URL");
        let webhook_secret = var("GITHUB_WEBHOOK_SECRET");
        let anthropic_api_key = var("ANTHROPIC_API_KEY");
        let autumn_secret_key = var("AUTUMN_SECRET_KEY");

        let github_app_id = match std::env::var("GITHUB_APP_ID") {
            Ok(raw) => match raw.trim().parse::<u64>() {
                Ok(id) => Some(id),
                Err(_) => {
                    problems.push(format!(
                        "GITHUB_APP_ID must be the numeric App ID, got {raw:?}"
                    ));
                    None
                }
            },
            Err(_) => {
                problems.push("missing required env var GITHUB_APP_ID".into());
                None
            }
        };

        let github_private_key_pem = load_private_key(&mut problems);

        if !problems.is_empty() {
            return Err(anyhow!(
                "configuration invalid:\n  - {}",
                problems.join("\n  - ")
            ));
        }

        Ok(Self {
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url: database_url.unwrap(),
            webhook_secret: webhook_secret.unwrap(),
            github_app_id: github_app_id.unwrap(),
            github_private_key_pem: github_private_key_pem.unwrap(),
            github_base_url: optional("GITHUB_BASE_URL"),
            anthropic_api_key: anthropic_api_key.unwrap(),
            anthropic_model: optional("ANTHROPIC_MODEL"),
            anthropic_base_url: optional("ANTHROPIC_BASE_URL"),
            autumn_secret_key: autumn_secret_key.unwrap(),
            autumn_base_url: optional("AUTUMN_BASE_URL"),
        })
    }
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Resolve the GitHub App private key from GITHUB_PRIVATE_KEY (inline PEM,
/// wins if set) or GITHUB_PRIVATE_KEY_PATH (tilde-expanded, must be a
/// readable PEM file).
fn load_private_key(problems: &mut Vec<String>) -> Option<String> {
    if let Some(inline) = optional("GITHUB_PRIVATE_KEY") {
        if !inline.contains("PRIVATE KEY") {
            problems.push(
                "GITHUB_PRIVATE_KEY is set but does not look like a PEM private key".into(),
            );
            return None;
        }
        return Some(inline);
    }

    let Some(raw_path) = optional("GITHUB_PRIVATE_KEY_PATH") else {
        problems.push("set GITHUB_PRIVATE_KEY (inline PEM) or GITHUB_PRIVATE_KEY_PATH".into());
        return None;
    };
    let path = expand_tilde(&raw_path);

    match std::fs::read_to_string(&path) {
        Ok(pem) if pem.contains("PRIVATE KEY") => Some(pem),
        Ok(_) => {
            problems.push(format!(
                "GITHUB_PRIVATE_KEY_PATH ({path}) is readable but does not contain a PEM private key"
            ));
            None
        }
        Err(e) => {
            problems.push(format!(
                "GITHUB_PRIVATE_KEY_PATH ({path}) is not readable: {e}"
            ));
            None
        }
    }
}

/// `fs::read_to_string` does not expand `~` — a `.env` entry like
/// `GITHUB_PRIVATE_KEY_PATH=~/.secrets/key.pem` would otherwise fail as a
/// relative path literally named `~`.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/x/y.pem"), format!("{home}/x/y.pem"));
        assert_eq!(expand_tilde("/abs/path.pem"), "/abs/path.pem");
        assert_eq!(expand_tilde("rel/path.pem"), "rel/path.pem");
    }
}
