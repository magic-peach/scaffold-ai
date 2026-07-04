//! Standalone check: validate config, then exercise the real GitHub App
//! auth chain — private key → JWT → installation access token — against
//! api.github.com.
//!
//!   cargo run -p scaffold-server --bin check-github-auth
//!
//! Prints installation ids/accounts and the minted token's EXPIRY ONLY.
//! Never prints key material, JWTs, or token values. Set
//! GITHUB_INSTALLATION_ID to target a specific installation (default: first).

use anyhow::Context;
use scaffold_github::{check_app_auth, GithubConfig};
use scaffold_server::config::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();

    // Step 1: config preflight (same validation the server runs at startup).
    let config = ServerConfig::load()?;
    println!(
        "[1/2] config OK: app id {}, private key loaded and PEM-shaped",
        config.github_app_id
    );

    let installation_override = match std::env::var("GITHUB_INSTALLATION_ID") {
        Ok(raw) => Some(
            raw.parse::<u64>()
                .context("GITHUB_INSTALLATION_ID must be numeric")?,
        ),
        Err(_) => None,
    };

    // Step 2: live auth chain.
    let report = check_app_auth(
        GithubConfig {
            app_id: config.github_app_id,
            private_key_pem: config.github_private_key_pem.clone(),
            base_url: config.github_base_url.clone(),
        },
        installation_override,
    )
    .await?;

    println!("[2/2] JWT accepted by GitHub. Installations:");
    for (id, account) in &report.installations {
        println!("      - installation {id} on account {account}");
    }
    println!(
        "      installation token minted for {} (value redacted), expires at: {}",
        report.installation_id_used,
        report.token_expires_at.as_deref().unwrap_or("unknown")
    );
    println!("      accessible repos:");
    for repo in &report.accessible_repos {
        println!("      - {repo}");
    }
    println!("\nSUCCESS: private key -> JWT -> installation token chain works against the real GitHub API.");
    Ok(())
}
