//! Diagnose webhook delivery: print the webhook URL configured on the
//! GitHub App and the outcome of recent deliveries.
//!
//!   cargo run -p scaffold-server --bin check_webhook
//!
//! Never prints the webhook secret.

use scaffold_github::{set_webhook_url, webhook_diagnostics, GithubConfig};
use scaffold_server::config::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let config = ServerConfig::load()?;
    let gh = GithubConfig {
        app_id: config.github_app_id,
        private_key_pem: config.github_private_key_pem.clone(),
        base_url: config.github_base_url.clone(),
    };

    // Optional: `--set-url <url>` updates the App's webhook URL first.
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--set-url") {
        let new_url = args
            .get(pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--set-url requires a URL argument"))?;
        set_webhook_url(gh.clone(), new_url).await?;
        println!("webhook URL updated to: {new_url}\n");
    }

    let (url, deliveries) = webhook_diagnostics(gh).await?;

    println!("configured webhook URL: {url}");
    println!("\nrecent deliveries (newest first):");
    if deliveries.is_empty() {
        println!("  (none recorded)");
    }
    for d in deliveries {
        println!(
            "  - {} guid={} action={} status={} ({}) at {}",
            d.event, d.guid, d.action, d.status, d.status_code, d.delivered_at
        );
    }
    Ok(())
}
