use serde::{Deserialize, Serialize};

/// Identifies one pull request under one GitHub App installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRef {
    pub installation_id: u64,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl std::fmt::Display for PullRequestRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    /// GitHub's file status: "added", "modified", "removed", "renamed", ...
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    TimedOut,
    ActionRequired,
    Skipped,
    /// Check run exists but has not finished.
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub conclusion: CheckConclusion,
}

/// Everything the agent needs to triage a PR, gathered up front in one step
/// so the rest of the loop is pure with respect to GitHub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestSnapshot {
    pub pr: PullRequestRef,
    pub title: String,
    pub body: String,
    pub author: String,
    pub base_branch: String,
    pub head_branch: String,
    pub head_sha: String,
    pub draft: bool,
    /// None while GitHub is still computing mergeability.
    pub mergeable: Option<bool>,
    /// Unified diff, capped at a byte budget by the host adapter.
    pub diff: String,
    pub diff_truncated: bool,
    pub changed_files: Vec<ChangedFile>,
    pub checks: Vec<CheckRun>,
}
