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

/// One recorded webhook delivery attempt, for diagnostics.
pub struct DeliverySummary {
    pub event: String,
    pub guid: String,
    pub action: String,
    pub status: String,
    pub status_code: i64,
    pub delivered_at: String,
}

/// Fetch the App's configured webhook URL and recent delivery outcomes.
/// The config endpoint's `secret` field is deliberately never read.
pub async fn webhook_diagnostics(
    config: GithubConfig,
) -> Result<(String, Vec<DeliverySummary>), TriageError> {
    let host = GithubHost::new(config)?;
    let app = &host.app_client;

    let hook: serde_json::Value = app
        .get("/app/hook/config", None::<&()>)
        .await
        .map_err(|e| host_err("GET /app/hook/config", e))?;
    let url = hook["url"].as_str().unwrap_or("(none)").to_string();

    let deliveries: serde_json::Value = app
        .get("/app/hook/deliveries?per_page=8", None::<&()>)
        .await
        .map_err(|e| host_err("GET /app/hook/deliveries", e))?;
    let summaries = deliveries
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|d| DeliverySummary {
                    event: d["event"].as_str().unwrap_or("?").to_string(),
                    guid: d["guid"].as_str().unwrap_or("?").to_string(),
                    action: d["action"].as_str().unwrap_or("-").to_string(),
                    status: d["status"].as_str().unwrap_or("?").to_string(),
                    status_code: d["status_code"].as_i64().unwrap_or(0),
                    delivered_at: d["delivered_at"].as_str().unwrap_or("?").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok((url, summaries))
}

/// Update the App's webhook delivery URL (used when a dev tunnel rotates).
/// Touches only `url` — the secret and other config are left untouched.
pub async fn set_webhook_url(config: GithubConfig, url: &str) -> Result<(), TriageError> {
    let host = GithubHost::new(config)?;
    let _: serde_json::Value = host
        .app_client
        .patch(
            "/app/hook/config".to_string(),
            Some(&serde_json::json!({ "url": url })),
        )
        .await
        .map_err(|e| host_err("PATCH /app/hook/config", e))?;
    Ok(())
}

/// Result of a live App-auth verification. Carries no secret material —
/// only installation identity and the minted token's expiry.
pub struct AuthCheckReport {
    /// (installation id, account login) for every installation of the App.
    pub installations: Vec<(u64, String)>,
    /// The installation the token was minted for.
    pub installation_id_used: u64,
    pub token_expires_at: Option<String>,
    /// full_name of every repo the installation can access.
    pub accessible_repos: Vec<String>,
}

/// Exercise the real auth chain — private key → JWT → installation access
/// token — against the live GitHub API. The token value is dropped here and
/// never returned.
pub async fn check_app_auth(
    config: GithubConfig,
    installation_override: Option<u64>,
) -> Result<AuthCheckReport, TriageError> {
    let host = GithubHost::new(config)?;
    let app = &host.app_client;

    let installations = app
        .apps()
        .installations()
        .send()
        .await
        .map_err(|e| host_err("GET /app/installations (JWT rejected or App ID wrong?)", e))?;
    let installations: Vec<(u64, String)> = installations
        .items
        .into_iter()
        .map(|i| (i.id.0, i.account.login))
        .collect();
    if installations.is_empty() {
        return Err(TriageError::Host(
            "JWT accepted, but the App has no installations — install it on a repo first".into(),
        ));
    }

    let installation_id_used = installation_override.unwrap_or(installations[0].0);
    let token: octocrab::models::InstallationToken = app
        .post(
            format!("/app/installations/{installation_id_used}/access_tokens"),
            None::<&()>,
        )
        .await
        .map_err(|e| host_err("minting installation token", e))?;

    let inst_client = app
        .installation(InstallationId(installation_id_used))
        .map_err(|e| host_err("installation client", e))?;
    let repos: serde_json::Value = inst_client
        .get("/installation/repositories", None::<&()>)
        .await
        .map_err(|e| host_err("GET /installation/repositories", e))?;
    let accessible_repos = repos["repositories"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r["full_name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok(AuthCheckReport {
        installations,
        installation_id_used,
        token_expires_at: token.expires_at,
        accessible_repos,
    })
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
