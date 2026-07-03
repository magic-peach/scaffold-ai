CREATE TABLE installations (
    installation_id BIGINT PRIMARY KEY,
    account_login   TEXT NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE triage_jobs (
    id              BIGSERIAL PRIMARY KEY,
    delivery_id     TEXT NOT NULL UNIQUE,
    installation_id BIGINT NOT NULL,
    owner           TEXT NOT NULL,
    repo            TEXT NOT NULL,
    pr_number       BIGINT NOT NULL,
    trigger         TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'queued', -- queued | running | done | dead
    attempts        INT NOT NULL DEFAULT 0,
    last_error      TEXT,
    run_after       TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_triage_jobs_claim ON triage_jobs (status, run_after);

CREATE TABLE triage_audit (
    id              BIGSERIAL PRIMARY KEY,
    installation_id BIGINT NOT NULL,
    owner           TEXT NOT NULL,
    repo            TEXT NOT NULL,
    pr_number       BIGINT NOT NULL,
    trigger         TEXT NOT NULL,
    report          JSONB NOT NULL,
    escalated       BOOLEAN NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_triage_audit_pr ON triage_audit (owner, repo, pr_number, created_at DESC);

-- Fail-open journal: triages executed while Autumn was unreachable, awaiting re-track.
CREATE TABLE untracked_usage (
    id              BIGSERIAL PRIMARY KEY,
    installation_id BIGINT NOT NULL,
    owner           TEXT NOT NULL,
    repo            TEXT NOT NULL,
    pr_number       BIGINT NOT NULL,
    tracked         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_untracked_usage_pending ON untracked_usage (tracked) WHERE NOT tracked;
