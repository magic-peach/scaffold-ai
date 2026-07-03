//! GitHub adapter: App authentication, PR data fetching, and triage actions.

pub mod webhook;

use async_trait::async_trait;
use octocrab::models::{AppId, InstallationId};
use octocrab::Octocrab;
use scaffold_domain::{
    ChangedFile, CheckConclusion, CheckRun, PullRequestHost, PullRequestRef, PullRequestSnapshot,
    TriageError,
};

/// Marker identifying the sticky triage comment across updates.
pub const COMMENT_MARKER: &str = "<!-- scaffold-ai:triage -->";

/// Byte cap on the fetched diff; beyond this the snapshot is marked truncated.
const DIFF_BYTE_BUDGET: usize = 300_000;

#[derive(Clone)]
pub struct GithubConfig {
    pub app_id: u64,
    /// PEM-encoded RSA private key for the GitHub App.
    pub private_key_pem: String,
    /// Override for tests (wiremock); None means api.github.com.
    pub base_url: Option<String>,
}

pub struct GithubHost {
    app_client: Octocrab,
}

impl GithubHost {
    pub fn new(config: GithubConfig) -> Result<Self, TriageError> {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(config.private_key_pem.as_bytes())
            .map_err(|e| TriageError::Host(format!("invalid GitHub App private key: {e}")))?;
        let mut builder = Octocrab::builder().app(AppId(config.app_id), key);
        if let Some(base) = &config.base_url {
            builder = builder
                .base_uri(base)
                .map_err(|e| TriageError::Host(format!("invalid GitHub base url: {e}")))?;
        }
        let app_client = builder
            .build()
            .map_err(|e| TriageError::Host(format!("octocrab build failed: {e}")))?;
        Ok(Self { app_client })
    }

    /// Installation-scoped client. octocrab mints and caches the installation
    /// token internally for the lifetime of the returned instance.
    fn installation(&self, installation_id: u64) -> Result<Octocrab, TriageError> {
        self.app_client
            .installation(InstallationId(installation_id))
            .map_err(|e| TriageError::Host(format!("installation client: {e}")))
    }
}

fn host_err(context: &str, e: impl std::fmt::Display) -> TriageError {
    TriageError::Host(format!("{context}: {e}"))
}

fn map_check_conclusion(status: &str, conclusion: Option<&str>) -> CheckConclusion {
    match conclusion {
        Some("success") => CheckConclusion::Success,
        Some("failure") => CheckConclusion::Failure,
        Some("neutral") => CheckConclusion::Neutral,
        Some("cancelled") => CheckConclusion::Cancelled,
        Some("timed_out") => CheckConclusion::TimedOut,
        Some("action_required") => CheckConclusion::ActionRequired,
        Some("skipped") => CheckConclusion::Skipped,
        _ => {
            let _ = status;
            CheckConclusion::Pending
        }
    }
}

#[async_trait]
impl PullRequestHost for GithubHost {
    async fn fetch_snapshot(
        &self,
        pr: &PullRequestRef,
    ) -> Result<PullRequestSnapshot, TriageError> {
        let inst = self.installation(pr.installation_id)?;
        let pulls = inst.pulls(&pr.owner, &pr.repo);

        let details = pulls
            .get(pr.number)
            .await
            .map_err(|e| host_err("fetch PR", e))?;

        let mut diff = pulls
            .get_diff(pr.number)
            .await
            .map_err(|e| host_err("fetch diff", e))?;
        let mut diff_truncated = false;
        if diff.len() > DIFF_BYTE_BUDGET {
            let mut end = DIFF_BYTE_BUDGET;
            while end > 0 && !diff.is_char_boundary(end) {
                end -= 1;
            }
            diff.truncate(end);
            diff_truncated = true;
        }

        let files = pulls
            .list_files(pr.number)
            .await
            .map_err(|e| host_err("list changed files", e))?;
        let changed_files = files
            .items
            .into_iter()
            .map(|f| ChangedFile {
                path: f.filename,
                additions: f.additions,
                deletions: f.deletions,
                status: format!("{:?}", f.status).to_lowercase(),
            })
            .collect();

        let head_sha = details.head.sha.clone();
        let checks = inst
            .checks(&pr.owner, &pr.repo)
            .list_check_runs_for_git_ref(head_sha.clone().into())
            .send()
            .await
            .map(|runs| {
                runs.check_runs
                    .into_iter()
                    .map(|c| CheckRun {
                        name: c.name,
                        conclusion: map_check_conclusion(
                            "",
                            c.conclusion.as_deref(),
                        ),
                    })
                    .collect()
            })
            .unwrap_or_else(|e| {
                // Checks can be unavailable (no CI configured); degrade quietly.
                tracing::debug!(pr = %pr, error = %e, "no check runs available");
                Vec::new()
            });

        Ok(PullRequestSnapshot {
            pr: pr.clone(),
            title: details.title.unwrap_or_default(),
            body: details.body.unwrap_or_default(),
            author: details.user.map(|u| u.login).unwrap_or_default(),
            base_branch: details.base.ref_field,
            head_branch: details.head.ref_field,
            head_sha,
            draft: details.draft.unwrap_or(false),
            mergeable: details.mergeable,
            diff,
            diff_truncated,
            changed_files,
            checks,
        })
    }

    async fn fetch_repo_config(&self, pr: &PullRequestRef) -> Result<Option<String>, TriageError> {
        let inst = self.installation(pr.installation_id)?;
        let result = inst
            .repos(&pr.owner, &pr.repo)
            .get_content()
            .path(".scaffold.toml")
            .send()
            .await;
        match result {
            Ok(mut contents) => Ok(contents
                .items
                .pop()
                .and_then(|item| item.decoded_content())),
            Err(octocrab::Error::GitHub { source, .. })
                if source.status_code == http::StatusCode::NOT_FOUND =>
            {
                Ok(None)
            }
            Err(e) => Err(host_err("fetch .scaffold.toml", e)),
        }
    }

    async fn upsert_triage_comment(
        &self,
        pr: &PullRequestRef,
        body: &str,
    ) -> Result<(), TriageError> {
        let inst = self.installation(pr.installation_id)?;
        let issues = inst.issues(&pr.owner, &pr.repo);
        let full_body = format!("{COMMENT_MARKER}\n{body}");

        let existing = issues
            .list_comments(pr.number)
            .send()
            .await
            .map_err(|e| host_err("list comments", e))?;

        if let Some(comment) = existing
            .items
            .into_iter()
            .find(|c| {
                c.body
                    .as_deref()
                    .is_some_and(|b| b.starts_with(COMMENT_MARKER))
            })
        {
            issues
                .update_comment(comment.id, full_body)
                .await
                .map_err(|e| host_err("update comment", e))?;
        } else {
            issues
                .create_comment(pr.number, full_body)
                .await
                .map_err(|e| host_err("create comment", e))?;
        }
        Ok(())
    }

    async fn add_labels(&self, pr: &PullRequestRef, labels: &[String]) -> Result<(), TriageError> {
        if labels.is_empty() {
            return Ok(());
        }
        let inst = self.installation(pr.installation_id)?;
        inst.issues(&pr.owner, &pr.repo)
            .add_labels(pr.number, labels)
            .await
            .map_err(|e| host_err("add labels", e))?;
        Ok(())
    }

    async fn request_reviewers(
        &self,
        pr: &PullRequestRef,
        reviewers: &[String],
    ) -> Result<(), TriageError> {
        if reviewers.is_empty() {
            return Ok(());
        }
        let inst = self.installation(pr.installation_id)?;
        inst.pulls(&pr.owner, &pr.repo)
            .request_reviews(pr.number, reviewers.to_vec(), Vec::<String>::new())
            .await
            .map_err(|e| host_err("request reviewers", e))?;
        Ok(())
    }
}
