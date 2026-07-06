# Scaffold AI

Agentic pull-request triage for open-source maintainers and small teams. Install the GitHub App on a repo and every incoming PR gets classified (bug fix / feature / docs / chore / needs-discussion), checked against policy rules (missing tests, unclear description, merge conflicts, failing CI), and answered with a single sticky triage comment plus labels — with anything ambiguous or high-risk escalated to a human maintainer instead of auto-decided.

Grown out of a triage pipeline that managed 150+ contributors and 250+ merged PRs during GSSoC 2026.

## How it works

```
GitHub webhook ──▶ axum server ──▶ Postgres job queue ──▶ worker
                   (verify HMAC,                          │
                    dedupe, 202)                          ▼
                                          billing gate (Autumn check)
                                                          │
                                                          ▼
                              agent loop: gather ▶ classify ▶ policy-check ▶ decide ▶ act
                                          (Claude, typed JSON)   (plain Rust)  (Claude)
                                                          │
                                                          ▼
                                    comment + labels on the PR · usage tracked (Autumn)
                                    escalations: label + @maintainers + reviewer request
```

Design principles:

- **Typed model calls.** Both Claude calls (classify, decide) use structured outputs with a JSON schema and deserialize directly into Rust structs. A malformed response fails the job loudly — after retries it degrades to *human escalation*, never to a guessed decision.
- **Escalation is an outcome, not an error.** Low confidence, `needs_discussion` classifications, and the model's own judgment all route to a `needs-maintainer-review` label with maintainers mentioned and requested as reviewers.
- **Deterministic policy checks.** Missing tests, short descriptions, merge conflicts, and failing CI are detected by plain Rust — the model interprets findings, it never invents them.
- **Fail open on billing outages.** If Autumn is unreachable, triage proceeds and usage is journaled locally, then re-tracked when billing recovers. Real quota denials still block.

## Workspace layout

| Crate | Role |
|---|---|
| `scaffold-domain` | Types + traits, zero I/O. Everything else implements or consumes these. |
| `scaffold-agent` | The triage orchestrator and policy checks. Depends only on domain traits. |
| `scaffold-anthropic` | Thin reqwest client for the Anthropic Messages API (structured outputs). |
| `scaffold-github` | GitHub App auth, PR data, comments/labels/reviewers, webhook signature verify. |
| `scaffold-autumn` | Autumn `check` / `track` / `attach` client. |
| `scaffold-store` | Postgres (sqlx) job queue, audit log, usage journal; in-memory impl for tests. |
| `scaffold-server` | The binary: axum routes, worker loop, wiring. |

## Local setup

Prerequisites: Rust 1.85+, Postgres 14+, and a tunnel for webhook delivery during development ([smee.io](https://smee.io) or `ngrok`).

```sh
createdb scaffold_ai

# either copy the template and fill it in...
cp .env.example .env
# ...or export the variables directly:
export DATABASE_URL=postgres://localhost/scaffold_ai
export GITHUB_APP_ID=<from your GitHub App>
export GITHUB_PRIVATE_KEY_PATH=./scaffold-ai.private-key.pem
export GITHUB_WEBHOOK_SECRET=<the secret you set on the App>
export ANTHROPIC_API_KEY=sk-ant-...
export AUTUMN_SECRET_KEY=am_sk_...

cargo run -p scaffold-server
```

Migrations are embedded in the binary and run automatically on startup.

### Environment variables

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `DATABASE_URL` | ✅ | — | Postgres connection string |
| `GITHUB_APP_ID` | ✅ | — | Numeric App ID |
| `GITHUB_PRIVATE_KEY` / `GITHUB_PRIVATE_KEY_PATH` | ✅ (one) | — | App RSA key (inline PEM, or path to a `.pem`) |
| `GITHUB_WEBHOOK_SECRET` | ✅ | — | HMAC secret for webhook verification |
| `ANTHROPIC_API_KEY` | ✅ | — | Anthropic API key |
| `AUTUMN_SECRET_KEY` | ✅ | — | Autumn secret key (`am_sk_...`) |
| `BIND_ADDR` | — | `0.0.0.0:8080` | Listen address |
| `ANTHROPIC_MODEL` | — | `claude-opus-4-8` | Model for classify + decide |
| `ANTHROPIC_BASE_URL` / `AUTUMN_BASE_URL` / `GITHUB_BASE_URL` | — | production APIs | Endpoint overrides (tests/staging) |
| `RUST_LOG` | — | `info,scaffold=debug` | Tracing filter |

## Creating the GitHub App

1. GitHub → Settings → Developer settings → **GitHub Apps** → New GitHub App.
2. Webhook URL: `https://<your-host>/webhooks/github` (or your smee/ngrok tunnel). Set a **webhook secret** and export it as `GITHUB_WEBHOOK_SECRET`.
3. **Repository permissions:**
   - Pull requests: **Read & write** (comments, reviewer requests)
   - Issues: **Read & write** (labels)
   - Contents: **Read** (diff, `.scaffold.toml`)
   - Checks: **Read** (CI status)
   - Metadata: Read (mandatory)
4. **Subscribe to events:** `Pull request`, `Installation`, `Installation repositories`.
5. Generate a **private key** (downloads a `.pem`) and note the **App ID**.
6. Install the App on a test repository, start the server, open a PR — a triage comment and labels should appear within ~30 seconds.

## Autumn setup

Create two **metered features** in the [Autumn dashboard](https://useautumn.com) — the service only knows these ids; all tier limits and pricing live in Autumn:

- `pr_triage` — one unit per executed triage (billing customer: `gh-install-<installation id>`)
- `repos` — count of repos with the App installed

Attach them to your products (e.g. Free: 1 repo / 50 triages per month; paid tiers higher). Changing limits or prices never requires a redeploy.

## Per-repo configuration (`.scaffold.toml`)

Optional file on the repo's default branch; every field has a sane default:

```toml
maintainers = ["alice", "bob"]      # mentioned + requested as reviewers on escalation
escalation_label = "needs-maintainer-review"
confidence_threshold = 0.7          # below this, the agent escalates instead of deciding
cooldown_minutes = 10               # min gap between re-triages on push
triage_on_synchronize = true        # re-triage (debounced) when new commits are pushed
min_description_chars = 40
skip_drafts = true
test_globs = ["tests/**", "**/*_test.rs"]
source_globs = ["src/**"]           # changes here without test changes → missing-tests finding
```

## Docker

```sh
docker build -t scaffold-ai .
docker run --env-file .env -p 8080:8080 scaffold-ai
```

The image is a single static-ish binary on `debian:bookworm-slim`; it runs anywhere a container runs (Fly.io, Railway, a VPS) — no vendor-specific services beyond Postgres.

## Tests

```sh
cargo test --workspace
```

Unit tests cover policy checks, decision resolution, config parsing, and signature verification. The integration suite (`crates/scaffold-server/tests/pipeline.rs`) drives the real HTTP clients end-to-end — signed webhook in, comment/labels out — against wiremock-faked GitHub, Anthropic, and Autumn APIs, including the quota-denied, billing-outage (fail-open + re-track), and dead-job escalation paths.

## Status

Live webhook pipeline verified end-to-end on a real GitHub App installation.

<!-- triage demo: 2026-07-06T10:49:24Z -->
