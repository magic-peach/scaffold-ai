//! Prompts and JSON schemas for the two typed model calls.

use scaffold_domain::{
    CheckConclusion, Classification, PolicyFinding, PullRequestSnapshot, RepoConfig,
};
use serde_json::json;

pub const CLASSIFY_SYSTEM: &str = "\
You are Scaffold AI, a pull-request triage assistant for open-source maintainers.
Classify the PR from its metadata and diff. Judge only what is in front of you; \
if the intent is genuinely unclear or the change mixes unrelated concerns, use \
needs_discussion rather than guessing. Set confidence honestly: it is used to \
decide whether a human reviews your work, and an overconfident wrong answer is \
worse than a low-confidence escalation.";

pub const DECIDE_SYSTEM: &str = "\
You are Scaffold AI, writing the triage comment a maintainer and the PR author \
will read. You receive a classification and a list of policy findings that were \
detected by deterministic checks — do not re-litigate them, and do not invent \
findings of your own. Write comment_markdown as a concise, friendly review \
comment: one-line summary, then the findings as actionable checklist items \
(omit the section if there are none), then any concrete suggestions. Never \
scold. Set escalate=true when the PR is high-risk, touches security-sensitive \
areas, or you are not confident a bot should decide — escalating to a human is \
a good outcome, not a failure. Do not @-mention anyone; mentions are added by \
the system.";

/// Cap on diff bytes included in a prompt. The host adapter also caps at fetch
/// time; this is a second line of defense.
const DIFF_PROMPT_BUDGET: usize = 120_000;

pub fn render_snapshot(s: &PullRequestSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!("# PR {}: {}\n\n", s.pr, s.title));
    out.push_str(&format!(
        "Author: {}\nBranch: {} -> {}\nDraft: {}\nMergeable: {}\n\n",
        s.author,
        s.head_branch,
        s.base_branch,
        s.draft,
        match s.mergeable {
            Some(true) => "yes",
            Some(false) => "NO (conflicts)",
            None => "unknown",
        }
    ));
    out.push_str("## Description\n\n");
    out.push_str(if s.body.trim().is_empty() {
        "(empty)"
    } else {
        &s.body
    });

    out.push_str("\n\n## Changed files\n\n");
    for f in &s.changed_files {
        out.push_str(&format!(
            "- {} ({}, +{} -{})\n",
            f.path, f.status, f.additions, f.deletions
        ));
    }

    if !s.checks.is_empty() {
        out.push_str("\n## CI checks\n\n");
        for c in &s.checks {
            let state = match c.conclusion {
                CheckConclusion::Success => "success",
                CheckConclusion::Failure => "FAILURE",
                CheckConclusion::Neutral => "neutral",
                CheckConclusion::Cancelled => "cancelled",
                CheckConclusion::TimedOut => "TIMED OUT",
                CheckConclusion::ActionRequired => "ACTION REQUIRED",
                CheckConclusion::Skipped => "skipped",
                CheckConclusion::Pending => "pending",
            };
            out.push_str(&format!("- {}: {}\n", c.name, state));
        }
    }

    out.push_str("\n## Diff\n\n```diff\n");
    let diff = truncate_to_budget(&s.diff, DIFF_PROMPT_BUDGET);
    out.push_str(diff);
    out.push_str("\n```\n");
    if s.diff_truncated || diff.len() < s.diff.len() {
        out.push_str("\n(NOTE: diff truncated — judge from what is shown and lower confidence if the omitted part matters.)\n");
    }
    out
}

pub fn render_decision_input(
    snapshot: &PullRequestSnapshot,
    classification: &Classification,
    findings: &[PolicyFinding],
    config: &RepoConfig,
) -> String {
    let mut out = render_snapshot(snapshot);
    out.push_str("\n## Classification (from previous step)\n\n");
    out.push_str(&format!(
        "category: {:?}\nconfidence: {:.2}\nrisk: {:?}\nsummary: {}\n",
        classification.category,
        classification.confidence,
        classification.risk,
        classification.summary
    ));

    out.push_str("\n## Policy findings (deterministic checks — authoritative)\n\n");
    if findings.is_empty() {
        out.push_str("(none)\n");
    } else {
        for f in findings {
            out.push_str(&format!("- [{:?}] {}\n", f.kind, f.detail));
        }
    }

    out.push_str(&format!(
        "\n## Repo policy\n\nDecisions below {:.2} confidence are escalated to a human automatically.\n",
        config.confidence_threshold
    ));
    out
}

/// Truncate at a char boundary at or below `budget` bytes.
fn truncate_to_budget(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub fn classification_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "category": {
                "type": "string",
                "enum": ["bug_fix", "feature", "docs", "chore", "needs_discussion"],
                "description": "What kind of change this PR is. Use needs_discussion when intent is unclear or scope should be debated first."
            },
            "confidence": {
                "type": "number",
                "description": "Your confidence in this classification, between 0.0 and 1.0."
            },
            "summary": {
                "type": "string",
                "description": "One paragraph: what the PR does and how."
            },
            "risk": {
                "type": "string",
                "enum": ["low", "medium", "high"],
                "description": "Blast radius if this change is wrong (core logic/security = high, docs = low)."
            }
        },
        "required": ["category", "confidence", "summary", "risk"],
        "additionalProperties": false
    })
}

pub fn decision_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "escalate": {
                "type": "boolean",
                "description": "True when a human maintainer should review this triage instead of trusting it."
            },
            "escalation_reason": {
                "type": ["string", "null"],
                "description": "Why a human is needed. Required in spirit when escalate is true."
            },
            "comment_markdown": {
                "type": "string",
                "description": "The triage comment body: summary line, findings checklist, suggestions. No @-mentions."
            },
            "labels": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Extra labels beyond category/priority, e.g. needs-tests. Empty if none."
            },
            "priority": {
                "type": "string",
                "enum": ["low", "medium", "high"],
                "description": "Review priority for maintainers."
            },
            "confidence": {
                "type": "number",
                "description": "Your confidence in this decision, between 0.0 and 1.0."
            }
        },
        "required": ["escalate", "escalation_reason", "comment_markdown", "labels", "priority", "confidence"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        let s = "héllo wörld".repeat(100);
        let t = truncate_to_budget(&s, 37);
        assert!(t.len() <= 37);
        assert!(s.starts_with(t));
    }
}
