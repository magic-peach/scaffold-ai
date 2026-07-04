use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use scaffold_anthropic::{AnthropicConfig, AnthropicTriageModel};
use scaffold_autumn::{AutumnBilling, AutumnConfig};
use scaffold_github::{GithubConfig, GithubHost};
use scaffold_server::config::ServerConfig;
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

    // Fail fast with a complete list of config problems.
    let config = ServerConfig::load()?;

    let mut anthropic_config = AnthropicConfig::new(config.anthropic_api_key.clone());
    if let Some(model) = &config.anthropic_model {
        anthropic_config.model = model.clone();
    }
    if let Some(base) = &config.anthropic_base_url {
        anthropic_config.base_url = base.clone();
    }

    let mut autumn_config = AutumnConfig::new(config.autumn_secret_key.clone());
    if let Some(base) = &config.autumn_base_url {
        autumn_config.base_url = base.clone();
    }

    let store = PgStore::connect(&config.database_url)
        .await
        .context("connecting to Postgres")?;
    let host = GithubHost::new(GithubConfig {
        app_id: config.github_app_id,
        private_key_pem: config.github_private_key_pem.clone(),
        base_url: config.github_base_url.clone(),
    })
    .context("building GitHub client")?;

    let state = Arc::new(AppState {
        webhook_secret: config.webhook_secret.clone(),
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

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .with_context(|| format!("binding {}", config.bind_addr))?;
    tracing::info!(bind_addr = %config.bind_addr, "scaffold-ai listening");
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}
