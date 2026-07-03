use globset::{Glob, GlobSet, GlobSetBuilder};
use scaffold_domain::{
    CheckConclusion, PolicyFinding, PolicyFindingKind, PullRequestSnapshot, RepoConfig,
};

/// Run all deterministic policy checks. These never call the model: they are
/// cheaper, faster, and more trustworthy as plain code. The model only
/// interprets findings downstream — it never detects them.
pub fn check(snapshot: &PullRequestSnapshot, config: &RepoConfig) -> Vec<PolicyFinding> {
    let mut findings = Vec::new();

    if snapshot.mergeable == Some(false) {
        findings.push(PolicyFinding {
            kind: PolicyFindingKind::MergeConflict,
            detail: format!(
                "Branch `{}` has merge conflicts with `{}`.",
                snapshot.head_branch, snapshot.base_branch
            ),
        });
    }

    let failing: Vec<&str> = snapshot
        .checks
        .iter()
        .filter(|c| {
            matches!(
                c.conclusion,
                CheckConclusion::Failure
                    | CheckConclusion::TimedOut
                    | CheckConclusion::ActionRequired
            )
        })
        .map(|c| c.name.as_str())
        .collect();
    if !failing.is_empty() {
        findings.push(PolicyFinding {
            kind: PolicyFindingKind::FailingCi,
            detail: format!("Failing checks: {}.", failing.join(", ")),
        });
    }

    if snapshot.body.trim().chars().count() < config.min_description_chars {
        findings.push(PolicyFinding {
            kind: PolicyFindingKind::UnclearDescription,
            detail: format!(
                "PR description is under {} characters; intent and context are hard to review.",
                config.min_description_chars
            ),
        });
    }

    if let Some(finding) = missing_tests(snapshot, config) {
        findings.push(finding);
    }

    findings
}

fn missing_tests(snapshot: &PullRequestSnapshot, config: &RepoConfig) -> Option<PolicyFinding> {
    let source_set = build_globset(&config.source_globs)?;
    let test_set = build_globset(&config.test_globs)?;

    let touches_source = snapshot
        .changed_files
        .iter()
        .any(|f| source_set.is_match(&f.path));
    let touches_tests = snapshot
        .changed_files
        .iter()
        .any(|f| test_set.is_match(&f.path));

    if touches_source && !touches_tests {
        Some(PolicyFinding {
            kind: PolicyFindingKind::MissingTests,
            detail: "Source files changed but no test files were added or modified.".to_string(),
        })
    } else {
        None
    }
}

/// Invalid globs are skipped with a warning rather than failing the triage —
/// a typo in `.scaffold.toml` should degrade a check, not the whole run.
fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let mut any = false;
    for pattern in patterns {
        match Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
                any = true;
            }
            Err(e) => tracing::warn!(pattern, error = %e, "skipping invalid glob in repo config"),
        }
    }
    if !any {
        return None;
    }
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scaffold_domain::{ChangedFile, CheckRun, PullRequestRef};

    fn snapshot(files: Vec<(&str, &str)>, body: &str) -> PullRequestSnapshot {
        PullRequestSnapshot {
            pr: PullRequestRef {
                installation_id: 1,
                owner: "o".into(),
                repo: "r".into(),
                number: 1,
            },
            title: "t".into(),
            body: body.into(),
            author: "a".into(),
            base_branch: "main".into(),
            head_branch: "feat".into(),
            head_sha: "abc".into(),
            draft: false,
            mergeable: Some(true),
            diff: String::new(),
            diff_truncated: false,
            changed_files: files
                .into_iter()
                .map(|(path, status)| ChangedFile {
                    path: path.into(),
                    additions: 1,
                    deletions: 0,
                    status: status.into(),
                })
                .collect(),
            checks: Vec::<CheckRun>::new(),
        }
    }

    #[test]
    fn flags_missing_tests_for_source_only_change() {
        let s = snapshot(
            vec![("src/main.rs", "modified")],
            "A long enough description of what this change does and why.",
        );
        let findings = check(&s, &RepoConfig::default());
        assert!(findings
            .iter()
            .any(|f| f.kind == PolicyFindingKind::MissingTests));
    }

    #[test]
    fn no_missing_tests_when_tests_touched() {
        let s = snapshot(
            vec![("src/main.rs", "modified"), ("tests/it.rs", "added")],
            "A long enough description of what this change does and why.",
        );
        let findings = check(&s, &RepoConfig::default());
        assert!(!findings
            .iter()
            .any(|f| f.kind == PolicyFindingKind::MissingTests));
    }

    #[test]
    fn flags_short_description() {
        let s = snapshot(vec![("docs/x.md", "modified")], "fix");
        let findings = check(&s, &RepoConfig::default());
        assert!(findings
            .iter()
            .any(|f| f.kind == PolicyFindingKind::UnclearDescription));
    }

    #[test]
    fn flags_merge_conflict() {
        let mut s = snapshot(
            vec![("docs/x.md", "modified")],
            "A long enough description of what this change does and why.",
        );
        s.mergeable = Some(false);
        let findings = check(&s, &RepoConfig::default());
        assert!(findings
            .iter()
            .any(|f| f.kind == PolicyFindingKind::MergeConflict));
    }
}
