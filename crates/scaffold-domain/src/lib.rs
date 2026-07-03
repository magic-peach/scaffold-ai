//! Core domain types and traits for Scaffold AI.
//!
//! This crate has no I/O dependencies. Every other crate either implements
//! or consumes the traits defined here, which keeps the agent logic swappable
//! and testable against fakes.

pub mod config;
pub mod error;
pub mod pr;
pub mod traits;
pub mod triage;

pub use config::RepoConfig;
pub use error::TriageError;
pub use pr::{ChangedFile, CheckConclusion, CheckRun, PullRequestRef, PullRequestSnapshot};
pub use traits::{
    BillingGate, BillingVerdict, PullRequestHost, TriageModel, TriageStore, UntrackedUsage,
};
pub use triage::{
    Classification, JobDisposition, ModelDecision, PolicyFinding, PolicyFindingKind, PrCategory,
    Priority, RiskLevel, TriageActions, TriageDecision, TriageJob, TriageReport, TriageTrigger,
};
