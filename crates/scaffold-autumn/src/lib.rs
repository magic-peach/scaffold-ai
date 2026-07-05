//! Client for Autumn's hosted billing API (check / track / attach / customers).
//!
//! Autumn is treated strictly as an external API — tier limits live in
//! Autumn's dashboard, and this crate only knows feature ids. The billing
//! customer is the GitHub App installation. The free plan (`scaffold_free`)
//! has auto-enable ON, so creating a customer is sufficient onboarding:
//! `POST /customers` is an idempotent upsert (verified live) and the plan
//! attaches automatically at creation time.
//!
//! Failure taxonomy (three-way split):
//! - real `allowed: false`            -> `Denied` (blocks triage)
//! - 404 `customer_not_found`         -> self-heal: create customer, retry once
//! - 404 `feature_not_found`          -> loud config error, `Unavailable` (fail open)
//! - network / 5xx / anything else    -> `Unavailable` (fail open)

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

/// Classified failure from one API call, so callers can branch on Autumn's
/// error `code` without string-matching messages.
#[derive(Debug)]
enum ApiError {
    /// Non-2xx with the parsed Autumn error code when the body had one.
    Http {
        status: u16,
        code: Option<String>,
        body: String,
    },
    Network(String),
}

impl ApiError {
    fn code(&self) -> Option<&str> {
        match self {
            ApiError::Http { code, .. } => code.as_deref(),
            ApiError::Network(_) => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Http { status, body, .. } => write!(f, "autumn returned {status}: {body}"),
            ApiError::Network(e) => write!(f, "autumn unreachable: {e}"),
        }
    }
}

impl From<ApiError> for TriageError {
    fn from(e: ApiError) -> Self {
        TriageError::Billing(e.to_string())
    }
}

#[derive(Deserialize)]
struct CheckResponse {
    allowed: bool,
}

#[derive(Deserialize)]
struct AttachResponse {
    checkout_url: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: Option<String>,
}

impl AutumnBilling {
    pub fn new(config: AutumnConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
        }
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<reqwest::Response, ApiError> {
        let url = format!("{}{}", self.config.base_url, path);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.secret_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let code = serde_json::from_str::<ErrorBody>(&text)
                .ok()
                .and_then(|e| e.code);
            return Err(ApiError::Http {
                status: status.as_u16(),
                code,
                body: text,
            });
        }
        Ok(resp)
    }

    /// Idempotent onboarding: `POST /customers` is an upsert in Autumn, and
    /// auto-enable attaches `scaffold_free` at creation. Safe to call on
    /// webhook retries and for customers that already exist.
    pub async fn ensure_customer_exists(&self, installation_id: u64) -> Result<(), TriageError> {
        let body = json!({
            "id": customer_id(installation_id),
            "name": format!("GitHub installation {installation_id}"),
        });
        self.post("/customers", body).await?;
        tracing::info!(installation_id, "Autumn customer ensured (free plan auto-enables)");
        Ok(())
    }

    /// One /check call, no healing. Ok(verdict) only for a 2xx response.
    async fn raw_check(
        &self,
        installation_id: u64,
        feature_id: &str,
    ) -> Result<BillingVerdict, ApiError> {
        let body = json!({
            "customer_id": customer_id(installation_id),
            "feature_id": feature_id,
        });
        let resp = self.post("/check", body).await?;
        match resp.json::<CheckResponse>().await {
            Ok(check) if check.allowed => Ok(BillingVerdict::Allowed),
            Ok(_) => Ok(BillingVerdict::Denied {
                reason: format!("plan limit reached for feature `{feature_id}`"),
            }),
            Err(e) => Err(ApiError::Network(format!(
                "unparseable check response: {e}"
            ))),
        }
    }

    /// Check with the three-way failure split. Never returns Err — outages
    /// and config errors are encoded as `Unavailable` so the caller applies
    /// the fail-open policy explicitly.
    async fn check_feature(&self, installation_id: u64, feature_id: &str) -> BillingVerdict {
        match self.raw_check(installation_id, feature_id).await {
            Ok(verdict) => verdict,
            Err(e) if e.code() == Some("customer_not_found") => {
                tracing::warn!(
                    installation_id,
                    "customer missing in Autumn — onboarding now (self-heal)"
                );
                if let Err(create_err) = self.ensure_customer_exists(installation_id).await {
                    tracing::error!(error = %create_err, installation_id, "self-heal customer creation failed");
                    return BillingVerdict::Unavailable;
                }
                match self.raw_check(installation_id, feature_id).await {
                    Ok(verdict) => verdict,
                    Err(e) => {
                        tracing::error!(error = %e, feature_id, "check still failing after customer creation");
                        BillingVerdict::Unavailable
                    }
                }
            }
            Err(e) if e.code() == Some("feature_not_found") => {
                tracing::error!(
                    feature_id,
                    "BILLING MISCONFIGURED: feature does not exist in Autumn — failing open until fixed"
                );
                BillingVerdict::Unavailable
            }
            Err(e) => {
                tracing::error!(error = %e, feature_id, "Autumn check failed — treating as unavailable");
                BillingVerdict::Unavailable
            }
        }
    }

    async fn raw_track(
        &self,
        installation_id: u64,
        feature_id: &str,
        value: i64,
    ) -> Result<(), ApiError> {
        let body = json!({
            "customer_id": customer_id(installation_id),
            "feature_id": feature_id,
            "value": value,
        });
        self.post("/track", body).await.map(|_| ())
    }

    /// Track with one self-heal retry if the customer doesn't exist yet
    /// (e.g. journal entries from before onboarding existed).
    async fn track_feature(
        &self,
        installation_id: u64,
        feature_id: &str,
        value: i64,
    ) -> Result<(), TriageError> {
        match self.raw_track(installation_id, feature_id, value).await {
            Ok(()) => Ok(()),
            Err(e) if e.code() == Some("customer_not_found") => {
                tracing::warn!(
                    installation_id,
                    "customer missing on track — onboarding then retrying"
                );
                self.ensure_customer_exists(installation_id).await?;
                self.raw_track(installation_id, feature_id, value)
                    .await
                    .map_err(Into::into)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Start a subscription/upgrade flow; returns Autumn's checkout URL when
    /// payment is needed. (Not used for free-tier onboarding — auto-enable
    /// covers that. Kept for future paid upgrades.)
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

    async fn ensure_customer(&self, installation_id: u64) -> Result<(), TriageError> {
        self.ensure_customer_exists(installation_id).await
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
