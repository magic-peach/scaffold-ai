//! The triage worker: claims jobs from the store, enforces billing and
//! cooldown, runs the agent, meters usage, records the audit trail.

use std::sync::Arc;
use std::time::Duration;

use scaffold_agent::{AgentOutcome, TriageAgent};
use scaffold_domain::{BillingVerdict, JobDisposition, TriageJob};

use crate::state::AppState;

/// Comment posted when the plan quota is exhausted. Replaced by a real triage
/// comment once quota is available again.
const QUOTA_COMMENT: &str = "Scaffold AI triage is paused for this repository: the plan's monthly \
PR-triage quota has been reached. A maintainer can upgrade the plan to resume automatic triage.";

/// Fallback comment when triage failed repeatedly — degrade to a human, never
/// to a guessed decision.
const DEAD_JOB_COMMENT: &str = "Scaffold AI could not complete automated triage for this PR after \
several attempts. Routing to a human maintainer.";
const DEAD_JOB_LABEL: &str = "needs-maintainer-review";

/// Claim-and-process loop. Run one or more of these as background tasks;
/// claiming is concurrency-safe (`FOR UPDATE SKIP LOCKED` in Postgres).
pub async fn run_worker(state: Arc<AppState>, poll_interval: Duration) {
    loop {
        match state.store.claim_next_job().await {
            Ok(Some(job)) => {
                let job_id = job.id;
                if let Err(e) = process_job(&state, job).await {
                    tracing::error!(job_id, error = %e, "job processing error");
                }
            }
            Ok(None) => tokio::time::sleep(poll_interval).await,
            Err(e) => {
                tracing::error!(error = %e, "failed to claim job");
                tokio::time::sleep(poll_interval).await;
            }
        }
    }
}

/// Process exactly one already-claimed job. Public so tests can drive the
/// pipeline without the polling loop.
pub async fn process_job(state: &AppState, job: TriageJob) -> anyhow::Result<()> {
    let pr = job.pr.clone();

    // Billing gate. A real denial blocks; an outage fails open (journaled).
    let verdict = state.billing.check_triage(pr.installation_id).await;
    if let BillingVerdict::Denied { reason } = &verdict {
        tracing::info!(pr = %pr, reason, "triage denied by billing");
        if let Err(e) = state.host.upsert_triage_comment(&pr, QUOTA_COMMENT).await {
            tracing::warn!(pr = %pr, error = %e, "could not post quota comment");
        }
        state.store.complete_job(job.id).await?;
        return Ok(());
    }

    let last_triaged = state.store.last_triaged_at(&pr).await?;
    let agent = TriageAgent::new(state.model.as_ref(), state.host.as_ref());

    match agent.run(&pr, job.trigger, last_triaged).await {
        Ok(AgentOutcome::Completed(report)) => {
            // Meter first, then audit. On billing failure/outage, journal for
            // re-tracking — fail open, never lose the audit record.
            let tracked = if verdict == BillingVerdict::Unavailable {
                false
            } else {
                match state.billing.track_triage(pr.installation_id, &pr).await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(pr = %pr, error = %e, "usage tracking failed, journaling");
                        false
                    }
                }
            };
            if !tracked {
                state
                    .store
                    .journal_untracked_usage(pr.installation_id, &pr)
                    .await?;
            }
            state.store.record_audit(&report).await?;
            state.store.complete_job(job.id).await?;
            tracing::info!(pr = %pr, escalated = report.decision.is_escalation(), "triage complete");
        }
        Ok(AgentOutcome::Skipped { reason }) => {
            tracing::info!(pr = %pr, reason, "triage skipped");
            state.store.complete_job(job.id).await?;
        }
        Err(e) => {
            tracing::warn!(pr = %pr, error = %e, attempts = job.attempts, "triage attempt failed");
            let disposition = state.store.fail_job(job.id, &e.to_string()).await?;
            if disposition == JobDisposition::Dead {
                degrade_to_escalation(state, &job).await;
            }
        }
    }
    Ok(())
}

/// Attempts exhausted: make the failure visible on the PR instead of failing
/// silently. Best effort — errors here are logged, not propagated.
async fn degrade_to_escalation(state: &AppState, job: &TriageJob) {
    tracing::error!(pr = %job.pr, "job dead — degrading to human escalation");
    if let Err(e) = state
        .host
        .upsert_triage_comment(&job.pr, DEAD_JOB_COMMENT)
        .await
    {
        tracing::error!(pr = %job.pr, error = %e, "could not post dead-job comment");
    }
    if let Err(e) = state
        .host
        .add_labels(&job.pr, &[DEAD_JOB_LABEL.to_string()])
        .await
    {
        tracing::error!(pr = %job.pr, error = %e, "could not add dead-job label");
    }
}

/// Background loop that re-tracks usage journaled during billing outages.
pub async fn run_usage_retracker(state: Arc<AppState>, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        let batch = match state.store.drain_untracked_usage(50).await {
            Ok(batch) => batch,
            Err(e) => {
                tracing::error!(error = %e, "could not read usage journal");
                continue;
            }
        };
        for usage in batch {
            match state
                .billing
                .track_triage(usage.installation_id, &usage.pr)
                .await
            {
                Ok(()) => {
                    if let Err(e) = state.store.mark_usage_tracked(usage.id).await {
                        tracing::error!(error = %e, id = usage.id, "could not mark usage tracked");
                    }
                }
                // Billing still down; stop and try again next interval.
                Err(_) => break,
            }
        }
    }
}
