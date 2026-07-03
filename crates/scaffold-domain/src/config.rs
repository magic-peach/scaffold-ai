use serde::Deserialize;

use crate::error::TriageError;

/// Per-repo configuration, read from `.scaffold.toml` on the default branch.
/// Every field has a default so an absent or partial file still works.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RepoConfig {
    /// GitHub logins @-mentioned and requested as reviewers on escalation.
    pub maintainers: Vec<String>,
    /// Label applied when the agent routes a PR to a human.
    pub escalation_label: String,
    /// Globs identifying test files (used by the missing-tests check).
    pub test_globs: Vec<String>,
    /// Globs identifying source files whose changes should come with tests.
    pub source_globs: Vec<String>,
    /// Below this model confidence, the agent escalates instead of deciding.
    pub confidence_threshold: f64,
    /// Minimum minutes between re-triages of the same PR on `synchronized`.
    pub cooldown_minutes: u64,
    /// Whether pushes to an open PR re-trigger triage at all.
    pub triage_on_synchronize: bool,
    /// Descriptions shorter than this raise an `unclear_description` finding.
    pub min_description_chars: usize,
    /// Skip draft PRs entirely.
    pub skip_drafts: bool,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            maintainers: Vec::new(),
            escalation_label: "needs-maintainer-review".to_string(),
            test_globs: vec![
                "tests/**".into(),
                "test/**".into(),
                "**/*_test.*".into(),
                "**/*.test.*".into(),
                "**/*.spec.*".into(),
                "**/test_*.py".into(),
            ],
            source_globs: vec!["src/**".into(), "lib/**".into(), "app/**".into()],
            confidence_threshold: 0.7,
            cooldown_minutes: 10,
            triage_on_synchronize: true,
            min_description_chars: 40,
            skip_drafts: true,
        }
    }
}

impl RepoConfig {
    pub fn from_toml_str(raw: &str) -> Result<Self, TriageError> {
        toml::from_str(raw).map_err(|e| TriageError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_toml_fills_defaults() {
        let cfg = RepoConfig::from_toml_str("maintainers = [\"alice\"]\ncooldown_minutes = 30\n")
            .unwrap();
        assert_eq!(cfg.maintainers, vec!["alice"]);
        assert_eq!(cfg.cooldown_minutes, 30);
        assert_eq!(cfg.escalation_label, "needs-maintainer-review");
        assert!(cfg.triage_on_synchronize);
    }

    #[test]
    fn empty_toml_is_default() {
        let cfg = RepoConfig::from_toml_str("").unwrap();
        assert_eq!(cfg.confidence_threshold, 0.7);
    }
}
