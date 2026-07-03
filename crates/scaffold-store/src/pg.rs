use async_trait::async_trait;
use chrono::{DateTime, Utc};
use scaffold_domain::{
    JobDisposition, PullRequestRef, TriageError, TriageJob, TriageReport, TriageStore,
    UntrackedUsage,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::{trigger_from_str, trigger_to_str, MAX_JOB_ATTEMPTS};

pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Connect and run embedded migrations.
    pub async fn connect(database_url: &str) -> Result<Self, TriageError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await
            .map_err(store_err)?;
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .map_err(|e| TriageError::Store(format!("migrations failed: {e}")))?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn store_err(e: impl std::fmt::Display) -> TriageError {
    TriageError::Store(e.to_string())
}

#[async_trait]
impl TriageStore for PgStore {
    async fn enqueue_job(
        &self,
        delivery_id: &str,
        pr: &PullRequestRef,
        trigger: scaffold_domain::TriageTrigger,
    ) -> Result<bool, TriageError> {
        let result = sqlx::query(
            "INSERT INTO triage_jobs (delivery_id, installation_id, owner, repo, pr_number, trigger)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (delivery_id) DO NOTHING",
        )
        .bind(delivery_id)
        .bind(pr.installation_id as i64)
        .bind(&pr.owner)
        .bind(&pr.repo)
        .bind(pr.number as i64)
        .bind(trigger_to_str(trigger))
        .execute(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn claim_next_job(&self) -> Result<Option<TriageJob>, TriageError> {
        let row = sqlx::query(
            "UPDATE triage_jobs
             SET status = 'running', attempts = attempts + 1, updated_at = now()
             WHERE id = (
                 SELECT id FROM triage_jobs
                 WHERE status = 'queued' AND run_after <= now()
                 ORDER BY id
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             RETURNING id, delivery_id, installation_id, owner, repo, pr_number, trigger, attempts",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(store_err)?;

        Ok(row.map(|r| TriageJob {
            id: r.get::<i64, _>("id"),
            delivery_id: r.get("delivery_id"),
            pr: PullRequestRef {
                installation_id: r.get::<i64, _>("installation_id") as u64,
                owner: r.get("owner"),
                repo: r.get("repo"),
                number: r.get::<i64, _>("pr_number") as u64,
            },
            trigger: trigger_from_str(&r.get::<String, _>("trigger")),
            attempts: r.get("attempts"),
        }))
    }

    async fn complete_job(&self, job_id: i64) -> Result<(), TriageError> {
        sqlx::query("UPDATE triage_jobs SET status = 'done', updated_at = now() WHERE id = $1")
            .bind(job_id)
            .execute(&self.pool)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    async fn fail_job(&self, job_id: i64, error: &str) -> Result<JobDisposition, TriageError> {
        // Exponential-ish backoff: 30s * attempts before the next try.
        let row = sqlx::query(
            "UPDATE triage_jobs
             SET status = CASE WHEN attempts >= $2 THEN 'dead' ELSE 'queued' END,
                 last_error = $3,
                 run_after = now() + make_interval(secs => 30 * attempts),
                 updated_at = now()
             WHERE id = $1
             RETURNING status",
        )
        .bind(job_id)
        .bind(MAX_JOB_ATTEMPTS)
        .bind(error)
        .fetch_one(&self.pool)
        .await
        .map_err(store_err)?;

        let status: String = row.get("status");
        Ok(if status == "dead" {
            JobDisposition::Dead
        } else {
            JobDisposition::Retry
        })
    }

    async fn last_triaged_at(
        &self,
        pr: &PullRequestRef,
    ) -> Result<Option<DateTime<Utc>>, TriageError> {
        let row = sqlx::query(
            "SELECT max(created_at) AS last FROM triage_audit
             WHERE owner = $1 AND repo = $2 AND pr_number = $3",
        )
        .bind(&pr.owner)
        .bind(&pr.repo)
        .bind(pr.number as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(row.get::<Option<DateTime<Utc>>, _>("last"))
    }

    async fn record_audit(&self, report: &TriageReport) -> Result<(), TriageError> {
        let json = serde_json::to_value(report).map_err(store_err)?;
        sqlx::query(
            "INSERT INTO triage_audit (installation_id, owner, repo, pr_number, trigger, report, escalated)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(report.pr.installation_id as i64)
        .bind(&report.pr.owner)
        .bind(&report.pr.repo)
        .bind(report.pr.number as i64)
        .bind(trigger_to_str(report.trigger))
        .bind(json)
        .bind(report.decision.is_escalation())
        .execute(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(())
    }

    async fn journal_untracked_usage(
        &self,
        installation_id: u64,
        pr: &PullRequestRef,
    ) -> Result<(), TriageError> {
        sqlx::query(
            "INSERT INTO untracked_usage (installation_id, owner, repo, pr_number)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(installation_id as i64)
        .bind(&pr.owner)
        .bind(&pr.repo)
        .bind(pr.number as i64)
        .execute(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(())
    }

    async fn drain_untracked_usage(
        &self,
        limit: i64,
    ) -> Result<Vec<UntrackedUsage>, TriageError> {
        let rows = sqlx::query(
            "SELECT id, installation_id, owner, repo, pr_number
             FROM untracked_usage WHERE NOT tracked ORDER BY id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;

        Ok(rows
            .into_iter()
            .map(|r| UntrackedUsage {
                id: r.get::<i64, _>("id"),
                installation_id: r.get::<i64, _>("installation_id") as u64,
                pr: PullRequestRef {
                    installation_id: r.get::<i64, _>("installation_id") as u64,
                    owner: r.get("owner"),
                    repo: r.get("repo"),
                    number: r.get::<i64, _>("pr_number") as u64,
                },
            })
            .collect())
    }

    async fn mark_usage_tracked(&self, usage_id: i64) -> Result<(), TriageError> {
        sqlx::query("UPDATE untracked_usage SET tracked = TRUE WHERE id = $1")
            .bind(usage_id)
            .execute(&self.pool)
            .await
            .map_err(store_err)?;
        Ok(())
    }
}
