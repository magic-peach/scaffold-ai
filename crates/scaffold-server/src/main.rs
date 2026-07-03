use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use scaffold_anthropic::{AnthropicConfig, AnthropicTriageModel};
use scaffold_autumn::{AutumnBilling, AutumnConfig};
use scaffold_github::{GithubConfig, GithubHost};
use scaffold_server::{build_router, worker, AppState};
use scaffold_store::PgStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,scaffold=debug".into()),
        )
        .init();

    let bind_addr = env_or("BIND_ADDR", "0.0.0.0:8080");
    let database_url = require_env("DATABASE_URL")?;
    let webhook_secret = require_env("GITHUB_WEBHOOK_SECRET")?;
    let app_id: u64 = require_env("GITHUB_APP_ID")?
        .parse()
        .context("GITHUB_APP_ID must be a number")?;
    let private_key_pem = match std::env::var("GITHUB_PRIVATE_KEY") {
        Ok(pem) => pem,
        Err(_) => {
            let path = require_env("GITHUB_PRIVATE_KEY_PATH")
                .context("set GITHUB_PRIVATE_KEY or GITHUB_PRIVATE_KEY_PATH")?;
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading GitHub App key from {path}"))?
        }
    };

    let mut anthropic_config = AnthropicConfig::new(require_env("ANTHROPIC_API_KEY")?);
    if let Ok(model) = std::env::var("ANTHROPIC_MODEL") {
        anthropic_config.model = model;
    }
    if let Ok(base) = std::env::var("ANTHROPIC_BASE_URL") {
        anthropic_config.base_url = base;
    }

    let mut autumn_config = AutumnConfig::new(require_env("AUTUMN_SECRET_KEY")?);
    if let Ok(base) = std::env::var("AUTUMN_BASE_URL") {
        autumn_config.base_url = base;
    }

    let store = PgStore::connect(&database_url)
        .await
        .context("connecting to Postgres")?;
    let host = GithubHost::new(GithubConfig {
        app_id,
        private_key_pem,
        base_url: std::env::var("GITHUB_BASE_URL").ok(),
    })
    .context("building GitHub client")?;

    let state = Arc::new(AppState {
        webhook_secret,
        store: Arc::new(store),
        host: Arc::new(host),
        model: Arc::new(AnthropicTriageModel::new(anthropic_config)),
        billing: Arc::new(AutumnBilling::new(autumn_config)),
    });

    tokio::spawn(worker::run_worker(state.clone(), Duration::from_secs(2)));
    tokio::spawn(worker::run_usage_retracker(
        state.clone(),
        Duration::from_secs(60),
    ));

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    tracing::info!(%bind_addr, "scaffold-ai listening");
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn require_env(key: &str) -> anyhow::Result<String> {
    std::env::var(key).with_context(|| format!("missing required env var {key}"))
}
