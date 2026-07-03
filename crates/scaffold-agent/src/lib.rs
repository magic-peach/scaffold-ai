//! The triage orchestrator: an explicit, typed loop of
//! gather → classify → policy-check → decide → act.
//!
//! Depends only on `scaffold-domain` traits, so it can run against fakes in
//! tests and never touches HTTP, GitHub, or the database directly.

pub mod policy;

use chrono::{DateTime, Utc};
use scaffold_domain::{
    Classification, ModelDecision, PrCategory, PullRequestHost, PullRequestRef, RepoConfig,
    TriageDecision, TriageError, TriageModel, TriageReport, TriageTrigger,
};

/// Outcome of one agent run. Skipping (drafts, config opt-outs) is a normal
/// result, distinct from both success and failure.
#[derive(Debug)]
pub enum AgentOutcome {
    Completed(TriageReport),
    Skipped { reason: String },
}

pub struct TriageAgent<'a> {
    model: &'a dyn TriageModel,
    host: &'a dyn PullRequestHost,
}

impl<'a> TriageAgent<'a> {
    pub fn new(model: &'a dyn TriageModel, host: &'a dyn PullRequestHost) -> Self {
        Self { model, host }
    }

    /// `last_triaged_at` is the previous successful triage of this PR (from
    /// the store); it drives the debounce on `synchronized` events.
    pub async fn run(
        &self,
        pr: &PullRequestRef,
        trigger: TriageTrigger,
        last_triaged_at: Option<DateTime<Utc>>,
    ) -> Result<AgentOutcome, TriageError> {
        // 1. Gather.
        let config = self.load_config(pr).await?;
        if trigger == TriageTrigger::Synchronized {
            if !config.triage_on_synchronize {
                return Ok(AgentOutcome::Skipped {
                    reason: "triage_on_synchronize disabled in .scaffold.toml".into(),
                });
            }
            if let Some(last) = last_triaged_at {
                let cooldown = chrono::Duration::minutes(config.cooldown_minutes as i64);
                if Utc::now().signed_duration_since(last) < cooldown {
                    return Ok(AgentOutcome::Skipped {
                        reason: format!(
                            "within {}-minute re-triage cooldown",
                            config.cooldown_minutes
                        ),
                    });
                }
            }
        }

        let snapshot = self.host.fetch_snapshot(pr).await?;
        if snapshot.draft && config.skip_drafts {
            return Ok(AgentOutcome::Skipped {
                reason: "draft PR".into(),
            });
        }

        // 2. Deterministic policy checks (no model involved).
        let findings = policy::check(&snapshot, &config);

        // 3. Classify (typed model call #1).
        let classification = self.model.classify(&snapshot).await?;
        validate_confidence(classification.confidence, "classification")?;

        // 4. Decide (typed model call #2), then resolve against the repo's
        //    confidence threshold. Low confidence always escalates.
        let model_decision = self
            .model
            .decide(&snapshot, &classification, &findings, &config)
            .await?;
        validate_confidence(model_decision.confidence, "decision")?;
        let decision = resolve_decision(model_decision, &classification, &config);

        // 5. Act via the host.
        self.act(pr, &classification, &decision, &config).await?;

        Ok(AgentOutcome::Completed(TriageReport {
            pr: pr.clone(),
            trigger,
            classification,
            findings,
            decision,
            finished_at: Utc::now(),
        }))
    }

    async fn load_config(&self, pr: &PullRequestRef) -> Result<RepoConfig, TriageError> {
        match self.host.fetch_repo_config(pr).await? {
            Some(raw) => match RepoConfig::from_toml_str(&raw) {
                Ok(cfg) => Ok(cfg),
                Err(e) => {
                    // A broken config file must not silence triage; fall back
                    // to defaults and keep going.
                    tracing::warn!(pr = %pr, error = %e, "invalid .scaffold.toml, using defaults");
                    Ok(RepoConfig::default())
                }
            },
            None => Ok(RepoConfig::default()),
        }
    }

    async fn act(
        &self,
        pr: &PullRequestRef,
        classification: &Classification,
        decision: &TriageDecision,
        config: &RepoConfig,
    ) -> Result<(), TriageError> {
        let actions = decision.actions();

        let mut labels: Vec<String> = vec![
            classification.category.label().to_string(),
            actions.priority.label().to_string(),
        ];
        labels.extend(actions.labels.iter().cloned());

        let mut comment = actions.comment_markdown.clone();
        if let TriageDecision::Escalate { reason, .. } = decision {
            labels.push(config.escalation_label.clone());
            let mentions = if config.maintainers.is_empty() {
                "the maintainers".to_string()
            } else {
                config
                    .maintainers
                    .iter()
                    .map(|m| format!("@{m}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            comment.push_str(&format!(
                "\n\n---\n\n⚠️ **Flagged for maintainer review** — cc {mentions}\n\n> {reason}"
            ));
        }
        labels.sort();
        labels.dedup();

        self.host.upsert_triage_comment(pr, &comment).await?;
        self.host.add_labels(pr, &labels).await?;

        if decision.is_escalation() && !config.maintainers.is_empty() {
            // Reviewer requests can fail for org-permission reasons; the
            // label + mention already guarantee visibility, so log and move on.
            if let Err(e) = self.host.request_reviewers(pr, &config.maintainers).await {
                tracing::warn!(pr = %pr, error = %e, "could not request reviewers");
            }
        }

        Ok(())
    }
}

/// A confidence outside 0..=1 means the model ignored the schema's intent;
/// treat it as invalid output rather than clamping.
fn validate_confidence(value: f64, step: &str) -> Result<(), TriageError> {
    if !(0.0..=1.0).contains(&value) || value.is_nan() {
        return Err(TriageError::InvalidModelOutput(format!(
            "{step} confidence {value} outside 0.0..=1.0"
        )));
    }
    Ok(())
}

/// Convert the model's flat decision into the domain decision, forcing
/// escalation whenever confidence is below the repo threshold or the PR was
/// classified as needing discussion.
fn resolve_decision(
    decision: ModelDecision,
    classification: &Classification,
    config: &RepoConfig,
) -> TriageDecision {
    let below_threshold = decision.confidence < config.confidence_threshold
        || classification.confidence < config.confidence_threshold;
    let needs_discussion = classification.category == PrCategory::NeedsDiscussion;

    let actions = scaffold_domain::TriageActions {
        comment_markdown: decision.comment_markdown,
        labels: decision.labels,
        priority: decision.priority,
    };

    if decision.escalate || below_threshold || needs_discussion {
        let reason = decision
            .escalation_reason
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| {
                if needs_discussion {
                    "PR classified as needing discussion before code review.".to_string()
                } else {
                    format!(
                        "Confidence below the configured threshold ({:.2}).",
                        config.confidence_threshold
                    )
                }
            });
        TriageDecision::Escalate { reason, actions }
    } else {
        TriageDecision::Proceed { actions }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scaffold_domain::{Priority, RiskLevel};

    fn classification(confidence: f64, category: PrCategory) -> Classification {
        Classification {
            category,
            confidence,
            summary: "s".into(),
            risk: RiskLevel::Low,
        }
    }

    fn model_decision(escalate: bool, confidence: f64) -> ModelDecision {
        ModelDecision {
            escalate,
            escalation_reason: None,
            comment_markdown: "c".into(),
            labels: vec![],
            priority: Priority::Medium,
            confidence,
        }
    }

    #[test]
    fn low_confidence_forces_escalation() {
        let d = resolve_decision(
            model_decision(false, 0.5),
            &classification(0.9, PrCategory::Feature),
            &RepoConfig::default(),
        );
        assert!(d.is_escalation());
    }

    #[test]
    fn needs_discussion_forces_escalation() {
        let d = resolve_decision(
            model_decision(false, 0.95),
            &classification(0.95, PrCategory::NeedsDiscussion),
            &RepoConfig::default(),
        );
        assert!(d.is_escalation());
    }

    #[test]
    fn confident_decision_proceeds() {
        let d = resolve_decision(
            model_decision(false, 0.9),
            &classification(0.9, PrCategory::BugFix),
            &RepoConfig::default(),
        );
        assert!(!d.is_escalation());
    }

    #[test]
    fn out_of_range_confidence_is_invalid_output() {
        assert!(validate_confidence(1.2, "decision").is_err());
        assert!(validate_confidence(f64::NAN, "decision").is_err());
        assert!(validate_confidence(0.0, "decision").is_ok());
    }
}
