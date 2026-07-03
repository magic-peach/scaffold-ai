//! Client for Autumn's hosted billing API (check / track / attach).
//!
//! Autumn is treated strictly as an external API — tier limits live in
//! Autumn's dashboard, and this crate only knows feature ids. The billing
//! customer is the GitHub App installation.

use async_trait::async_trait;
use scaffold_domain::{BillingGate, BillingVerdict, PullRequestRef, TriageError};
use serde::Deserialize;
use serde_json::json;

pub const DEFAULT_BASE_URL: &str = "https://api.useautumn.com/v1";

/// Metered feature: one unit per executed triage.
pub const FEATURE_PR_TRIAGE: &str = "pr_triage";
/// Metered feature: number of repos with Scaffold AI enabled.
pub const FEATURE_REPOS: &str = "repos";

pub fn customer_id(installation_id: u64) -> String {
    format!("gh-install-{installation_id}")
}

#[derive(Debug, Clone)]
pub struct AutumnConfig {
    pub secret_key: String,
    pub base_url: String,
}

impl AutumnConfig {
    pub fn new(secret_key: impl Into<String>) -> Self {
        Self {
            secret_key: secret_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }
}

pub struct AutumnBilling {
    http: reqwest::Client,
    config: AutumnConfig,
}

#[derive(Deserialize)]
struct CheckResponse {
    allowed: bool,
}

#[derive(Deserialize)]
struct AttachResponse {
    checkout_url: Option<String>,
}

impl AutumnBilling {
    pub fn new(config: AutumnConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
        }
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<reqwest::Response, TriageError> {
        let url = format!("{}{}", self.config.base_url, path);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.secret_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| TriageError::Billing(format!("autumn unreachable: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(TriageError::Billing(format!(
                "autumn returned {status}: {text}"
            )));
        }
        Ok(resp)
    }

    /// Distinguishes "billing said no" from "billing is down". Callers apply
    /// the fail-open policy on `Unavailable`; a real denial always blocks.
    async fn check_feature(&self, installation_id: u64, feature_id: &str) -> BillingVerdict {
        let body = json!({
            "customer_id": customer_id(installation_id),
            "feature_id": feature_id,
        });
        match self.post("/check", body).await {
            Ok(resp) => match resp.json::<CheckResponse>().await {
                Ok(check) if check.allowed => BillingVerdict::Allowed,
                Ok(_) => BillingVerdict::Denied {
                    reason: format!("plan limit reached for feature `{feature_id}`"),
                },
                Err(e) => {
                    tracing::error!(error = %e, feature_id, "unparseable Autumn check response");
                    BillingVerdict::Unavailable
                }
            },
            Err(e) => {
                tracing::error!(error = %e, feature_id, "Autumn check failed — treating as unavailable");
                BillingVerdict::Unavailable
            }
        }
    }

    async fn track_feature(
        &self,
        installation_id: u64,
        feature_id: &str,
        value: i64,
    ) -> Result<(), TriageError> {
        let body = json!({
            "customer_id": customer_id(installation_id),
            "feature_id": feature_id,
            "value": value,
        });
        self.post("/track", body).await.map(|_| ())
    }

    /// Start a subscription/upgrade flow; returns Autumn's checkout URL when
    /// payment is needed (surfaced in the quota-exceeded comment).
    pub async fn attach(
        &self,
        installation_id: u64,
        product_id: &str,
    ) -> Result<Option<String>, TriageError> {
        let body = json!({
            "customer_id": customer_id(installation_id),
            "product_id": product_id,
        });
        let resp = self.post("/attach", body).await?;
        let attach: AttachResponse = resp
            .json()
            .await
            .map_err(|e| TriageError::Billing(format!("unparseable attach response: {e}")))?;
        Ok(attach.checkout_url)
    }
}

#[async_trait]
impl BillingGate for AutumnBilling {
    async fn check_triage(&self, installation_id: u64) -> BillingVerdict {
        self.check_feature(installation_id, FEATURE_PR_TRIAGE).await
    }

    async fn check_repo(&self, installation_id: u64) -> BillingVerdict {
        self.check_feature(installation_id, FEATURE_REPOS).await
    }

    async fn track_triage(
        &self,
        installation_id: u64,
        pr: &PullRequestRef,
    ) -> Result<(), TriageError> {
        tracing::debug!(pr = %pr, "tracking triage usage");
        self.track_feature(installation_id, FEATURE_PR_TRIAGE, 1)
            .await
    }

    async fn track_repo(&self, installation_id: u64, delta: i64) -> Result<(), TriageError> {
        self.track_feature(installation_id, FEATURE_REPOS, delta)
            .await
    }
}
