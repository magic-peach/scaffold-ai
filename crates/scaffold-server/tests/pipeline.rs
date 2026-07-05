//! End-to-end integration tests: a signed GitHub webhook goes in, the worker
//! runs the full triage pipeline against wiremock-faked GitHub, Anthropic,
//! and Autumn APIs, and the triage comment/labels come out.
//!
//! The real adapters (`GithubHost`, `AnthropicTriageModel`, `AutumnBilling`)
//! are used — only the network endpoints and the store are substituted.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use scaffold_anthropic::{AnthropicConfig, AnthropicTriageModel};
use scaffold_autumn::{AutumnBilling, AutumnConfig};
use scaffold_github::webhook::sign;
use scaffold_github::{GithubConfig, GithubHost, COMMENT_MARKER};
use scaffold_server::{build_router, worker, AppState};
use scaffold_store::InMemoryStore;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request as WiremockRequest, ResponseTemplate};

const WEBHOOK_SECRET: &str = "test-webhook-secret";
const TEST_KEY: &str = include_str!("fixtures/test-github-app-key.pem");

struct TestStack {
    github: MockServer,
    anthropic: MockServer,
    autumn: MockServer,
    store: Arc<InMemoryStore>,
    state: Arc<AppState>,
}

async fn stack() -> TestStack {
    static TRACING: std::sync::Once = std::sync::Once::new();
    TRACING.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .try_init();
    });

    let github = MockServer::start().await;
    let anthropic = MockServer::start().await;
    let autumn = MockServer::start().await;

    let store = Arc::new(InMemoryStore::new());

    let host = GithubHost::new(GithubConfig {
        app_id: 1,
        private_key_pem: TEST_KEY.to_string(),
        base_url: Some(github.uri()),
    })
    .expect("github host");

    let mut anthropic_config = AnthropicConfig::new("test-key");
    anthropic_config.base_url = anthropic.uri();
    let mut autumn_config = AutumnConfig::new("am_sk_test");
    autumn_config.base_url = autumn.uri();

    let state = Arc::new(AppState {
        webhook_secret: WEBHOOK_SECRET.to_string(),
        store: store.clone(),
        host: Arc::new(host),
        model: Arc::new(AnthropicTriageModel::new(anthropic_config)),
        billing: Arc::new(AutumnBilling::new(autumn_config)),
    });

    TestStack {
        github,
        anthropic,
        autumn,
        store,
        state,
    }
}

fn pr_opened_payload() -> Value {
    json!({
        "action": "opened",
        "number": 7,
        "installation": { "id": 42 },
        "repository": { "name": "demo", "owner": { "login": "acme" } },
        "pull_request": { "number": 7 }
    })
}

async fn post_webhook(
    state: Arc<AppState>,
    event: &str,
    delivery: &str,
    payload: &Value,
) -> (StatusCode, String) {
    let body = serde_json::to_vec(payload).unwrap();
    let signature = sign(WEBHOOK_SECRET, &body);
    let response = build_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/github")
                .header("x-github-event", event)
                .header("x-github-delivery", delivery)
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// A GitHub user object with every field octocrab's `Author` model requires.
fn gh_user() -> Value {
    let u = "https://gh.test/u";
    json!({
        "login": "scaffold-ai[bot]", "id": 99, "node_id": "U_1",
        "avatar_url": u, "gravatar_id": "", "url": u, "html_url": u,
        "followers_url": u, "following_url": u, "gists_url": u,
        "starred_url": u, "subscriptions_url": u, "organizations_url": u,
        "repos_url": u, "events_url": u, "received_events_url": u,
        "type": "Bot", "site_admin": false
    })
}

/// Mount every GitHub API mock the pipeline touches for acme/demo#7.
/// `scaffold_toml`: None -> 404 (defaults apply); Some(s) -> served as content.
async fn mount_github(github: &MockServer, scaffold_toml: Option<&str>) {
    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_test_token",
            "permissions": {}
        })))
        .mount(github)
        .await;

    // Diff variant of the PR endpoint must be mounted before the JSON variant
    // so the Accept header disambiguates.
    Mock::given(method("GET"))
        .and(path("/repos/acme/demo/pulls/7"))
        .and(header("accept", "application/vnd.github.v3.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "diff --git a/src/lib.rs b/src/lib.rs\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        ))
        .mount(github)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/demo/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": "https://gh.test/repos/acme/demo/pulls/7",
            "id": 1001,
            "number": 7,
            "locked": false,
            "maintainer_can_modify": false,
            "title": "Add add() helper",
            "body": "Adds a small addition helper used by the calculator module.",
            "draft": false,
            "mergeable": true,
            "user": gh_user(),
            "head": { "ref": "feat/add", "sha": "abc123" },
            "base": { "ref": "main", "sha": "def456" }
        })))
        .mount(github)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/demo/pulls/7/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "sha": "f1", "filename": "src/lib.rs", "status": "modified",
            "additions": 1, "deletions": 0, "changes": 1,
            "contents_url": "https://gh.test/contents/src/lib.rs"
        }])))
        .mount(github)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/demo/commits/abc123/check-runs"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"total_count": 0, "check_runs": []})),
        )
        .mount(github)
        .await;

    let contents = match scaffold_toml {
        Some(raw) => {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
            ResponseTemplate::new(200).set_body_json(json!({
                "type": "file",
                "name": ".scaffold.toml", "path": ".scaffold.toml", "sha": "c1",
                "size": raw.len(), "url": "https://gh.test/contents/.scaffold.toml",
                "encoding": "base64", "content": encoded,
                "_links": { "self": "https://gh.test/contents/.scaffold.toml" }
            }))
        }
        None => ResponseTemplate::new(404).set_body_json(json!({
            "message": "Not Found",
            "documentation_url": "https://docs.github.com"
        })),
    };
    Mock::given(method("GET"))
        .and(path("/repos/acme/demo/contents/.scaffold.toml"))
        .respond_with(contents)
        .mount(github)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/demo/issues/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(github)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/acme/demo/issues/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 555, "node_id": "C_1",
            "url": "https://gh.test/comments/555",
            "html_url": "https://gh.test/comments/555",
            "author_association": "NONE",
            "user": gh_user(),
            "created_at": "2026-07-02T00:00:00Z",
            "body": "placeholder"
        })))
        .mount(github)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/acme/demo/issues/7/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(github)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/acme/demo/pulls/7/requested_reviewers"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 777, "node_id": "R_1", "html_url": "https://gh.test/reviews/777"
        })))
        .mount(github)
        .await;
}

fn anthropic_message(structured: Value) -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-8",
        "stop_reason": "end_turn",
        "content": [{ "type": "text", "text": structured.to_string() }],
        "usage": { "input_tokens": 100, "output_tokens": 50 }
    })
}

/// Mount classify + decide responses. The decide prompt always contains the
/// classification section header, which disambiguates the two calls.
async fn mount_anthropic(anthropic: &MockServer, decide: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("Classification (from previous step)"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_message(decide)))
        .mount(anthropic)
        .await;

    let classify = json!({
        "category": "feature",
        "confidence": 0.92,
        "summary": "Adds an addition helper function.",
        "risk": "low"
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_message(classify)))
        .mount(anthropic)
        .await;
}

fn confident_decision() -> Value {
    json!({
        "escalate": false,
        "escalation_reason": null,
        "comment_markdown": "Looks like a clean feature addition. Consider adding a unit test.",
        "labels": ["needs-tests"],
        "priority": "medium",
        "confidence": 0.9
    })
}

async fn mount_autumn_allowed(autumn: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"allowed": true})))
        .mount(autumn)
        .await;
    Mock::given(method("POST"))
        .and(path("/track"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(autumn)
        .await;
}

async fn drain_queue(stack: &TestStack) {
    while let Some(job) = stack.store.claim_next_job().await.unwrap() {
        worker::process_job(&stack.state, job).await.unwrap();
    }
}

fn received_bodies<'a>(
    requests: &'a [WiremockRequest],
    method: &str,
    path_suffix: &str,
) -> Vec<&'a WiremockRequest> {
    requests
        .iter()
        .filter(|r| r.method.as_str() == method && r.url.path().ends_with(path_suffix))
        .collect()
}

use scaffold_domain::TriageStore;

#[tokio::test]
async fn installation_created_onboards_customer_and_tracks_repos() {
    let stack = stack().await;
    Mock::given(method("POST"))
        .and(path("/customers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "gh-install-42"})))
        .mount(&stack.autumn)
        .await;
    Mock::given(method("POST"))
        .and(path("/track"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": 1})))
        .mount(&stack.autumn)
        .await;

    let payload = json!({
        "action": "created",
        "installation": { "id": 42 },
        "repositories": [{ "full_name": "acme/demo" }]
    });
    let (status, _) = post_webhook(stack.state.clone(), "installation", "d-install", &payload).await;
    assert_eq!(status, StatusCode::OK);

    let requests = stack.autumn.received_requests().await.unwrap();
    let creates = received_bodies(&requests, "POST", "/customers");
    assert_eq!(creates.len(), 1);
    let create_body: Value = serde_json::from_slice(&creates[0].body).unwrap();
    assert_eq!(create_body["id"], "gh-install-42");

    let tracks = received_bodies(&requests, "POST", "/track");
    assert_eq!(tracks.len(), 1);
    let track_body: Value = serde_json::from_slice(&tracks[0].body).unwrap();
    assert_eq!(track_body["feature_id"], "repos");
    assert_eq!(track_body["value"], 1);
}

#[tokio::test]
async fn rejects_invalid_signature() {
    let stack = stack().await;
    let body = serde_json::to_vec(&pr_opened_payload()).unwrap();
    let response = build_router(stack.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/github")
                .header("x-github-event", "pull_request")
                .header("x-github-delivery", "d-1")
                .header("x-hub-signature-256", "sha256=deadbeef")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn duplicate_deliveries_enqueue_once() {
    let stack = stack().await;
    let payload = pr_opened_payload();
    let (s1, _) = post_webhook(stack.state.clone(), "pull_request", "d-dup", &payload).await;
    let (s2, b2) = post_webhook(stack.state.clone(), "pull_request", "d-dup", &payload).await;
    assert_eq!(s1, StatusCode::ACCEPTED);
    assert_eq!(s2, StatusCode::OK);
    assert!(b2.contains("duplicate"));

    let first = stack.store.claim_next_job().await.unwrap();
    let second = stack.store.claim_next_job().await.unwrap();
    assert!(first.is_some());
    assert!(second.is_none());
}

#[tokio::test]
async fn full_pipeline_posts_comment_labels_and_meters_usage() {
    let stack = stack().await;
    mount_github(&stack.github, None).await;
    mount_anthropic(&stack.anthropic, confident_decision()).await;
    mount_autumn_allowed(&stack.autumn).await;

    let (status, _) = post_webhook(
        stack.state.clone(),
        "pull_request",
        "d-full",
        &pr_opened_payload(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    drain_queue(&stack).await;

    // Two model calls: classify + decide.
    let model_requests = stack.anthropic.received_requests().await.unwrap();
    assert_eq!(received_bodies(&model_requests, "POST", "/v1/messages").len(), 2);

    // Billing: one check, one track, both for the installation's customer id.
    let autumn_requests = stack.autumn.received_requests().await.unwrap();
    let checks = received_bodies(&autumn_requests, "POST", "/check");
    let tracks = received_bodies(&autumn_requests, "POST", "/track");
    assert_eq!(checks.len(), 1);
    assert_eq!(tracks.len(), 1);
    let track_body: Value = serde_json::from_slice(&tracks[0].body).unwrap();
    assert_eq!(track_body["customer_id"], "gh-install-42");
    assert_eq!(track_body["feature_id"], "pr_triage");

    // GitHub: sticky comment created with marker + model text; labels applied.
    let gh_requests = stack.github.received_requests().await.unwrap();
    let comments = received_bodies(&gh_requests, "POST", "/issues/7/comments");
    assert_eq!(comments.len(), 1);
    let comment_body: Value = serde_json::from_slice(&comments[0].body).unwrap();
    let text = comment_body["body"].as_str().unwrap();
    assert!(text.starts_with(COMMENT_MARKER));
    assert!(text.contains("clean feature addition"));

    let labels = received_bodies(&gh_requests, "POST", "/issues/7/labels");
    assert_eq!(labels.len(), 1);
    let label_body: Value = serde_json::from_slice(&labels[0].body).unwrap();
    let labels_sent: Vec<&str> = label_body["labels"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(labels_sent.contains(&"feature"));
    assert!(labels_sent.contains(&"priority:medium"));
    assert!(labels_sent.contains(&"needs-tests"));
    // Confident decision: no escalation label, no reviewer request.
    assert!(!labels_sent.contains(&"needs-maintainer-review"));
    assert!(received_bodies(&gh_requests, "POST", "/requested_reviewers").is_empty());

    // Audit recorded, nothing left in the usage journal.
    assert_eq!(stack.store.audits().len(), 1);
    assert_eq!(stack.store.pending_usage_count(), 0);
}

#[tokio::test]
async fn low_confidence_escalates_with_mentions_and_reviewers() {
    let stack = stack().await;
    // Maintainers configured via .scaffold.toml served from the repo.
    mount_github(&stack.github, Some("maintainers = [\"alice\", \"bob\"]\n")).await;
    let unsure_decision = json!({
        "escalate": false,
        "escalation_reason": null,
        "comment_markdown": "This mixes refactoring with behavior changes.",
        "labels": [],
        "priority": "high",
        "confidence": 0.4
    });
    mount_anthropic(&stack.anthropic, unsure_decision).await;
    mount_autumn_allowed(&stack.autumn).await;

    post_webhook(
        stack.state.clone(),
        "pull_request",
        "d-esc",
        &pr_opened_payload(),
    )
    .await;
    drain_queue(&stack).await;

    let gh_requests = stack.github.received_requests().await.unwrap();

    let comments = received_bodies(&gh_requests, "POST", "/issues/7/comments");
    let comment_body: Value = serde_json::from_slice(&comments[0].body).unwrap();
    let text = comment_body["body"].as_str().unwrap();
    assert!(text.contains("@alice"));
    assert!(text.contains("@bob"));
    assert!(text.contains("Flagged for maintainer review"));

    let labels = received_bodies(&gh_requests, "POST", "/issues/7/labels");
    let label_body: Value = serde_json::from_slice(&labels[0].body).unwrap();
    assert!(label_body["labels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "needs-maintainer-review"));

    let reviewers = received_bodies(&gh_requests, "POST", "/requested_reviewers");
    assert_eq!(reviewers.len(), 1);
    let reviewer_body: Value = serde_json::from_slice(&reviewers[0].body).unwrap();
    assert_eq!(reviewer_body["reviewers"], json!(["alice", "bob"]));

    let audits = stack.store.audits();
    assert!(audits[0].decision.is_escalation());
}

#[tokio::test]
async fn quota_denied_posts_quota_comment_and_skips_model() {
    let stack = stack().await;
    mount_github(&stack.github, None).await;
    // No Anthropic mocks: any model call would 404 and fail the test below.
    Mock::given(method("POST"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"allowed": false})))
        .mount(&stack.autumn)
        .await;

    post_webhook(
        stack.state.clone(),
        "pull_request",
        "d-quota",
        &pr_opened_payload(),
    )
    .await;
    drain_queue(&stack).await;

    assert!(stack
        .anthropic
        .received_requests()
        .await
        .unwrap()
        .is_empty());

    let gh_requests = stack.github.received_requests().await.unwrap();
    let comments = received_bodies(&gh_requests, "POST", "/issues/7/comments");
    assert_eq!(comments.len(), 1);
    let comment_body: Value = serde_json::from_slice(&comments[0].body).unwrap();
    assert!(comment_body["body"]
        .as_str()
        .unwrap()
        .contains("quota has been reached"));

    assert!(stack.store.audits().is_empty());
}

#[tokio::test]
async fn billing_outage_fails_open_and_journals_usage() {
    let stack = stack().await;
    mount_github(&stack.github, None).await;
    mount_anthropic(&stack.anthropic, confident_decision()).await;
    // Autumn is down: both endpoints 500.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&stack.autumn)
        .await;

    post_webhook(
        stack.state.clone(),
        "pull_request",
        "d-outage",
        &pr_opened_payload(),
    )
    .await;
    drain_queue(&stack).await;

    // Triage still happened (fail-open)...
    let gh_requests = stack.github.received_requests().await.unwrap();
    assert_eq!(
        received_bodies(&gh_requests, "POST", "/issues/7/comments").len(),
        1
    );
    assert_eq!(stack.store.audits().len(), 1);
    // ...and the usage is journaled for later re-tracking.
    assert_eq!(stack.store.pending_usage_count(), 1);

    // Billing recovers: the re-tracker drains the journal.
    stack.autumn.reset().await;
    mount_autumn_allowed(&stack.autumn).await;
    let retrack_state = stack.state.clone();
    tokio::spawn(worker::run_usage_retracker(
        retrack_state,
        Duration::from_millis(20),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stack.store.pending_usage_count(), 0);
    let autumn_requests = stack.autumn.received_requests().await.unwrap();
    assert_eq!(received_bodies(&autumn_requests, "POST", "/track").len(), 1);
}

#[tokio::test]
async fn malformed_model_output_fails_job_then_degrades_to_escalation() {
    let stack = stack().await;
    mount_github(&stack.github, None).await;
    mount_autumn_allowed(&stack.autumn).await;
    // Model returns text that is not valid schema JSON.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_message(json!("::"))))
        .mount(&stack.anthropic)
        .await;

    post_webhook(
        stack.state.clone(),
        "pull_request",
        "d-bad-model",
        &pr_opened_payload(),
    )
    .await;

    // Drive the job through all retry attempts to the dead state.
    for _ in 0..scaffold_store::MAX_JOB_ATTEMPTS {
        if let Some(job) = stack.store.claim_next_job().await.unwrap() {
            worker::process_job(&stack.state, job).await.unwrap();
        }
    }

    // No audit (nothing was decided), but the failure is visible on the PR:
    // dead-job comment + escalation label.
    assert!(stack.store.audits().is_empty());
    let gh_requests = stack.github.received_requests().await.unwrap();
    let comments = received_bodies(&gh_requests, "POST", "/issues/7/comments");
    let last: Value = serde_json::from_slice(&comments.last().unwrap().body).unwrap();
    assert!(last["body"]
        .as_str()
        .unwrap()
        .contains("could not complete automated triage"));
    let labels = received_bodies(&gh_requests, "POST", "/issues/7/labels");
    let label_body: Value = serde_json::from_slice(&labels.last().unwrap().body).unwrap();
    assert!(label_body["labels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "needs-maintainer-review"));
}
