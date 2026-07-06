# scaffold-ai

A GitHub App that triages incoming pull requests: it classifies each PR, runs
deterministic policy checks, and posts a single review comment with labels —
or routes the PR to a human when it isn't confident. Written in Rust
(`axum` / `tokio` / `sqlx` / `octocrab`), it calls Claude for judgment and
Autumn for usage metering.

The interesting parts are structural, so this README leads with how it's built.
Setup and run instructions are at the bottom.

## The central constraint: webhooks are fast, triage is slow

GitHub expects a webhook to be answered within about 10 seconds and treats a
slower response as a failed delivery. A triage run takes tens of seconds — a
recorded run was ~15s — from two Claude round-trips plus several GitHub API
calls, already past the 10-second budget. The two can't share a request
handler, so the system is split in two:

```
GitHub ──webhook──▶ axum handler                 Postgres job queue
                    - verify HMAC signature   ┌─▶ triage_jobs
                    - deserialize             │   (queued│running│done│dead)
                    - enqueue ────────────────┘
                    - return 202 Accepted
                                                        │ claim
                                                        ▼
                                                     worker
                    fetch PR ▶ policy-check ▶ classify ▶ decide ▶ act
                    (octocrab)   (Rust)      (Claude)  (Claude)  (comment+labels)
                                                        │
                                              billing gate (Autumn) around it
```

The HTTP handler does only cheap, bounded work and returns immediately. All slow
work happens in a decoupled worker that polls the queue. This is the design's
load-bearing decision, not an incidental optimization.

## Job queue (Postgres, no broker)

The queue is an ordinary Postgres table, not a separate message broker.

- **Concurrent claiming.** A worker takes the next job with a single
  `UPDATE … WHERE id = (SELECT id … WHERE status='queued' AND run_after<=now()
  ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1)`. `SKIP LOCKED` lets multiple
  worker processes draw from the same queue at once without blocking on each
  other and without two workers ever claiming the same row.
- **Deduplication.** Each job is keyed by GitHub's delivery GUID under a
  `UNIQUE` constraint with `ON CONFLICT DO NOTHING`, so a redelivered webhook
  (GitHub's own retries, or a manual replay) enqueues at most once.
- **Retries and dead-lettering.** A failed job is requeued with a linear
  backoff (`run_after = now() + 30s × attempts`) and retried up to three times.
  After that it's marked `dead` and the PR gets a comment plus a
  `needs-maintainer-review` label — the failure surfaces to a human instead of
  being silently dropped.

## Trait boundaries

`scaffold-domain` defines the contracts and the types that cross them, and has
no I/O dependencies:

| Trait | Responsibility |
|---|---|
| `TriageModel` | classify a PR, decide a comment/priority |
| `PullRequestHost` | fetch PR data, post comments/labels/reviewers |
| `BillingGate` | check and track usage, ensure a customer exists |
| `TriageStore` | enqueue/claim jobs, record audits, journal usage |

Each adapter crate implements exactly one of these against a real service
(Anthropic over `reqwest`, GitHub via `octocrab`, Autumn over `reqwest`,
Postgres via `sqlx`). The agent and server depend only on the traits — so the
entire pipeline runs in tests against in-memory and `wiremock`-backed fakes
without a network.

## The agent loop: deterministic checks, model for judgment

The loop is five explicit steps: **gather → policy-check → classify → decide →
act**. The split between plain code and the model is deliberate:

- **Policy checks are Rust.** Merge conflict, failing CI, a too-thin
  description, and source files changed without accompanying tests are detected
  deterministically — no model call, no cost, no variance.
- **Claude is called exactly twice**, both with structured-output JSON schemas
  that deserialize directly into Rust structs: once to classify the PR
  (type, risk, confidence), once to write the review comment and priority. A
  response that doesn't match the schema fails the step loudly rather than being
  coerced into a guess.
- **Escalation is a first-class outcome.** Low model confidence, a
  `needs_discussion` classification, or the model's own escalate signal route
  the PR to a human (escalation label, maintainer mention, reviewer request)
  instead of forcing an automated decision.

## Billing degradation

The billing gate classifies failures instead of treating every non-2xx alike.
Only a genuine `allowed: false` blocks a triage; the rest degrade rather than
break:

- **`customer_not_found` (404)** — the installation isn't onboarded yet. The
  gate performs an idempotent `POST /customers` upsert, then retries the
  original call once.
- **`feature_not_found` (404)** — a misconfiguration on our side. Triage
  proceeds (fail-open) but the error is logged loudly as something to fix; a
  configuration mistake shouldn't block users.
- **5xx / network / unparseable** — the provider is unreachable. Triage
  proceeds and the usage event is written to a Postgres journal table; a
  background task re-tracks journaled usage once the provider recovers.

## Workspace layout

| Crate | Role |
|---|---|
| `scaffold-domain` | Traits and shared types. Zero I/O. |
| `scaffold-agent` | The five-step triage loop and the deterministic policy checks. |
| `scaffold-anthropic` | `reqwest` client for the Claude Messages API (structured outputs). |
| `scaffold-github` | GitHub App auth, PR data, comments/labels/reviewers, webhook HMAC verification. |
| `scaffold-autumn` | Autumn billing client (`check` / `track` / `attach` / customer upsert). |
| `scaffold-store` | Postgres job queue, audit log, usage journal; plus an in-memory impl for tests. |
| `scaffold-server` | The binary: axum routes, the worker loop, and wiring. |

## Tests

`cargo test --workspace`. Unit tests cover the policy checks, decision
resolution, config parsing, and signature verification. Integration tests drive
the real adapter code end-to-end — a signed webhook in, a comment and labels out
— against `wiremock`-faked GitHub, Anthropic, and Autumn, including the
quota-denied, billing-outage (fail-open then re-track), and dead-letter
escalation paths.

## Running it

Prerequisites: Rust, Postgres, and a public webhook URL for local development
(any tunnel — `cloudflared`, `ngrok`, `smee`).

```sh
createdb scaffold_ai
cp .env.example .env    # then fill in the values below
cargo run -p scaffold-server
```

Migrations run automatically on startup.

**Required environment:**

| Variable | Purpose |
|---|---|
| `DATABASE_URL` | Postgres connection string |
| `GITHUB_APP_ID` | numeric App ID |
| `GITHUB_PRIVATE_KEY` *or* `GITHUB_PRIVATE_KEY_PATH` | App RSA key, inline PEM or a file path |
| `GITHUB_WEBHOOK_SECRET` | HMAC secret for webhook verification |
| `ANTHROPIC_API_KEY` | Claude API key |
| `AUTUMN_SECRET_KEY` | Autumn API key |

Optional: `BIND_ADDR` (default `0.0.0.0:8080`), `ANTHROPIC_MODEL`, and
`*_BASE_URL` overrides for pointing the clients at test servers.

```sh
docker build -t scaffold-ai .
docker run --env-file .env -p 8080:8080 scaffold-ai
```

A single binary plus Postgres — deployable to any container host.
