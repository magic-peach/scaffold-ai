use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::pr::PullRequestRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrCategory {
    BugFix,
    Feature,
    Docs,
    Chore,
    NeedsDiscussion,
}

impl PrCategory {
    /// Label applied for this category.
    pub fn label(&self) -> &'static str {
        match self {
            PrCategory::BugFix => "bug-fix",
            PrCategory::Feature => "feature",
            PrCategory::Docs => "docs",
            PrCategory::Chore => "chore",
            PrCategory::NeedsDiscussion => "needs-discussion",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// Output of the classify step. Shape matches the JSON schema sent to the
/// model, so a response deserializes directly into this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Classification {
    pub category: PrCategory,
    /// Model's own confidence in the classification, 0.0..=1.0.
    pub confidence: f64,
    /// One-paragraph summary of what the PR does.
    pub summary: String,
    pub risk: RiskLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyFindingKind {
    MissingTests,
    UnclearDescription,
    MergeConflict,
    FailingCi,
}

/// A deterministic policy-check result. These are produced by plain Rust,
/// never by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyFinding {
    pub kind: PolicyFindingKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
}

impl Priority {
    pub fn label(&self) -> &'static str {
        match self {
            Priority::Low => "priority:low",
            Priority::Medium => "priority:medium",
            Priority::High => "priority:high",
        }
    }
}

/// Output of the decide step, shaped for the model's JSON schema (flat,
/// no enum variants with payloads). The agent converts this into a
/// [`TriageDecision`], applying the repo's confidence threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDecision {
    /// Model recommends routing to a human instead of standing behind the triage.
    pub escalate: bool,
    pub escalation_reason: Option<String>,
    /// Markdown body for the sticky triage comment.
    pub comment_markdown: String,
    /// Extra labels beyond category/priority (e.g. "needs-tests").
    pub labels: Vec<String>,
    pub priority: Priority,
    /// Model's confidence in this decision, 0.0..=1.0.
    pub confidence: f64,
}

/// What the agent will do on GitHub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageActions {
    pub comment_markdown: String,
    pub labels: Vec<String>,
    pub priority: Priority,
}

/// Final, validated outcome of a triage run. Escalation is a first-class
/// variant, not an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriageDecision {
    Proceed {
        actions: TriageActions,
    },
    Escalate {
        reason: String,
        actions: TriageActions,
    },
}

impl TriageDecision {
    pub fn is_escalation(&self) -> bool {
        matches!(self, TriageDecision::Escalate { .. })
    }

    pub fn actions(&self) -> &TriageActions {
        match self {
            TriageDecision::Proceed { actions } => actions,
            TriageDecision::Escalate { actions, .. } => actions,
        }
    }
}

/// Complete record of one triage run, persisted to the audit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageReport {
    pub pr: PullRequestRef,
    pub trigger: TriageTrigger,
    pub classification: Classification,
    pub findings: Vec<PolicyFinding>,
    pub decision: TriageDecision,
    pub finished_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageTrigger {
    Opened,
    Reopened,
    Synchronized,
}

/// A queued unit of work created by the webhook handler and claimed by the
/// worker loop.
#[derive(Debug, Clone)]
pub struct TriageJob {
    pub id: i64,
    /// GitHub delivery GUID, used for idempotency.
    pub delivery_id: String,
    pub pr: PullRequestRef,
    pub trigger: TriageTrigger,
    pub attempts: i32,
}

/// What the store decided after a job failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobDisposition {
    /// Job will be retried later.
    Retry,
    /// Attempts exhausted; worker must degrade to human escalation.
    Dead,
}
