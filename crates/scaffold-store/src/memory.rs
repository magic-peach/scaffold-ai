//! In-memory `TriageStore` for tests and DB-free local runs. Mirrors the
//! Postgres semantics (idempotent enqueue, attempt-capped retries).

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use scaffold_domain::{
    JobDisposition, PullRequestRef, TriageError, TriageJob, TriageReport, TriageStore,
    TriageTrigger, UntrackedUsage,
};

use crate::MAX_JOB_ATTEMPTS;

#[derive(Default)]
struct Inner {
    next_job_id: i64,
    next_usage_id: i64,
    jobs: Vec<StoredJob>,
    audits: Vec<TriageReport>,
    usage: Vec<StoredUsage>,
}

struct StoredJob {
    id: i64,
    delivery_id: String,
    pr: PullRequestRef,
    trigger: TriageTrigger,
    attempts: i32,
    status: JobStatus,
}

#[derive(PartialEq)]
enum JobStatus {
    Queued,
    Running,
    Done,
    Dead,
}

struct StoredUsage {
    id: i64,
    installation_id: u64,
    pr: PullRequestRef,
    tracked: bool,
}

#[derive(Default)]
pub struct InMemoryStore {
    inner: Mutex<Inner>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper: all recorded audit reports.
    pub fn audits(&self) -> Vec<TriageReport> {
        self.inner.lock().unwrap().audits.clone()
    }

    /// Test helper: pending (untracked) usage journal size.
    pub fn pending_usage_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .usage
            .iter()
            .filter(|u| !u.tracked)
            .count()
    }
}

#[async_trait]
impl TriageStore for InMemoryStore {
    async fn enqueue_job(
        &self,
        delivery_id: &str,
        pr: &PullRequestRef,
        trigger: TriageTrigger,
    ) -> Result<bool, TriageError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.jobs.iter().any(|j| j.delivery_id == delivery_id) {
            return Ok(false);
        }
        inner.next_job_id += 1;
        let id = inner.next_job_id;
        inner.jobs.push(StoredJob {
            id,
            delivery_id: delivery_id.to_string(),
            pr: pr.clone(),
            trigger,
            attempts: 0,
            status: JobStatus::Queued,
        });
        Ok(true)
    }

    async fn claim_next_job(&self) -> Result<Option<TriageJob>, TriageError> {
        let mut inner = self.inner.lock().unwrap();
        let job = inner
            .jobs
            .iter_mut()
            .find(|j| j.status == JobStatus::Queued);
        Ok(job.map(|j| {
            j.status = JobStatus::Running;
            j.attempts += 1;
            TriageJob {
                id: j.id,
                delivery_id: j.delivery_id.clone(),
                pr: j.pr.clone(),
                trigger: j.trigger,
                attempts: j.attempts,
            }
        }))
    }

    async fn complete_job(&self, job_id: i64) -> Result<(), TriageError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(j) = inner.jobs.iter_mut().find(|j| j.id == job_id) {
            j.status = JobStatus::Done;
        }
        Ok(())
    }

    async fn fail_job(&self, job_id: i64, _error: &str) -> Result<JobDisposition, TriageError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(j) = inner.jobs.iter_mut().find(|j| j.id == job_id) else {
            return Err(TriageError::Store(format!("no such job {job_id}")));
        };
        if j.attempts >= MAX_JOB_ATTEMPTS {
            j.status = JobStatus::Dead;
            Ok(JobDisposition::Dead)
        } else {
            j.status = JobStatus::Queued;
            Ok(JobDisposition::Retry)
        }
    }

    async fn last_triaged_at(
        &self,
        pr: &PullRequestRef,
    ) -> Result<Option<DateTime<Utc>>, TriageError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .audits
            .iter()
            .filter(|a| a.pr == *pr)
            .map(|a| a.finished_at)
            .max())
    }

    async fn record_audit(&self, report: &TriageReport) -> Result<(), TriageError> {
        self.inner.lock().unwrap().audits.push(report.clone());
        Ok(())
    }

    async fn journal_untracked_usage(
        &self,
        installation_id: u64,
        pr: &PullRequestRef,
    ) -> Result<(), TriageError> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_usage_id += 1;
        let id = inner.next_usage_id;
        inner.usage.push(StoredUsage {
            id,
            installation_id,
            pr: pr.clone(),
            tracked: false,
        });
        Ok(())
    }

    async fn drain_untracked_usage(
        &self,
        limit: i64,
    ) -> Result<Vec<UntrackedUsage>, TriageError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .usage
            .iter()
            .filter(|u| !u.tracked)
            .take(limit as usize)
            .map(|u| UntrackedUsage {
                id: u.id,
                installation_id: u.installation_id,
                pr: u.pr.clone(),
            })
            .collect())
    }

    async fn mark_usage_tracked(&self, usage_id: i64) -> Result<(), TriageError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(u) = inner.usage.iter_mut().find(|u| u.id == usage_id) {
            u.tracked = true;
        }
        Ok(())
    }
}
