use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::config::RepoConfig;
use crate::error::TriageError;
use crate::pr::{PullRequestRef, PullRequestSnapshot};
use crate::triage::{
    Classification, JobDisposition, ModelDecision, PolicyFinding, TriageJob, TriageReport,
    TriageTrigger,
};

/// The LLM boundary. Implementations own prompts, model ids, and transport;
/// callers only see typed, validated outputs.
#[async_trait]
pub trait TriageModel: Send + Sync {
    async fn classify(
        &self,
        snapshot: &PullRequestSnapshot,
    ) -> Result<Classification, TriageError>;

    async fn decide(
        &self,
        snapshot: &PullRequestSnapshot,
        classification: &Classification,
        findings: &[PolicyFinding],
        config: &RepoConfig,
    ) -> Result<ModelDecision, TriageError>;
}

/// The pull-request host boundary (GitHub in production).
#[async_trait]
pub trait PullRequestHost: Send + Sync {
    async fn fetch_snapshot(
        &self,
        pr: &PullRequestRef,
    ) -> Result<PullRequestSnapshot, TriageError>;

    /// Raw `.scaffold.toml` contents from the default branch, or None if absent.
    async fn fetch_repo_config(&self, pr: &PullRequestRef) -> Result<Option<String>, TriageError>;

    /// Create or update the single sticky triage comment on the PR.
    async fn upsert_triage_comment(
        &self,
        pr: &PullRequestRef,
        body: &str,
    ) -> Result<(), TriageError>;

    async fn add_labels(&self, pr: &PullRequestRef, labels: &[String]) -> Result<(), TriageError>;

    async fn request_reviewers(
        &self,
        pr: &PullRequestRef,
        reviewers: &[String],
    ) -> Result<(), TriageError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BillingVerdict {
    Allowed,
    /// A real denial from the billing API (quota exceeded, no plan). Blocks triage.
    Denied { reason: String },
    /// Billing API unreachable — policy is fail-open: proceed, journal usage.
    Unavailable,
}

/// The billing boundary (Autumn in production). `check_*` methods never
/// return Err: an outage is encoded as `Unavailable` so the caller applies
/// the fail-open policy explicitly.
#[async_trait]
pub trait BillingGate: Send + Sync {
    async fn check_triage(&self, installation_id: u64) -> BillingVerdict;

    async fn check_repo(&self, installation_id: u64) -> BillingVerdict;

    /// Idempotent onboarding: ensure the billing customer for this
    /// installation exists (the free plan auto-enables at creation).
    /// Safe to call repeatedly and on webhook retries.
    async fn ensure_customer(&self, installation_id: u64) -> Result<(), TriageError>;

    async fn track_triage(
        &self,
        installation_id: u64,
        pr: &PullRequestRef,
    ) -> Result<(), TriageError>;

    /// delta: +1 on repo enabled, -1 on repo removed.
    async fn track_repo(&self, installation_id: u64, delta: i64) -> Result<(), TriageError>;
}

/// One journaled usage event that couldn't be tracked while billing was down.
#[derive(Debug, Clone)]
pub struct UntrackedUsage {
    pub id: i64,
    pub installation_id: u64,
    pub pr: PullRequestRef,
}

/// The persistence boundary: job queue, audit log, cooldown state, and the
/// fail-open usage journal.
#[async_trait]
pub trait TriageStore: Send + Sync {
    /// Returns false if this delivery id was already enqueued (idempotency).
    async fn enqueue_job(
        &self,
        delivery_id: &str,
        pr: &PullRequestRef,
        trigger: TriageTrigger,
    ) -> Result<bool, TriageError>;

    /// Claim the next runnable job, if any. Must be safe under concurrent workers.
    async fn claim_next_job(&self) -> Result<Option<TriageJob>, TriageError>;

    async fn complete_job(&self, job_id: i64) -> Result<(), TriageError>;

    async fn fail_job(&self, job_id: i64, error: &str) -> Result<JobDisposition, TriageError>;

    /// When the same PR finished its last successful triage (cooldown check).
    async fn last_triaged_at(
        &self,
        pr: &PullRequestRef,
    ) -> Result<Option<DateTime<Utc>>, TriageError>;

    async fn record_audit(&self, report: &TriageReport) -> Result<(), TriageError>;

    /// Fail-open journal: usage that must be re-tracked once billing recovers.
    async fn journal_untracked_usage(
        &self,
        installation_id: u64,
        pr: &PullRequestRef,
    ) -> Result<(), TriageError>;

    async fn drain_untracked_usage(
        &self,
        limit: i64,
    ) -> Result<Vec<UntrackedUsage>, TriageError>;

    async fn mark_usage_tracked(&self, usage_id: i64) -> Result<(), TriageError>;
}
