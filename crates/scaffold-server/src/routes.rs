use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use scaffold_domain::PullRequestRef;
use scaffold_github::webhook::{
    verify_signature, InstallationEvent, InstallationRepositoriesEvent, PullRequestEvent,
};

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/webhooks/github", post(github_webhook))
        .with_state(state)
}

use crate::state::AppState;

/// Webhook entry point. Does the minimum before responding — verify, dedupe,
/// enqueue — because GitHub times deliveries out at 10s and a triage takes
/// far longer. The worker does the actual work.
async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !verify_signature(&state.webhook_secret, &body, signature) {
        return (StatusCode::UNAUTHORIZED, "invalid signature");
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let delivery_id = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if delivery_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing delivery id");
    }

    match event {
        "pull_request" => handle_pull_request(&state, delivery_id, &body).await,
        "installation" => handle_installation(&state, &body).await,
        "installation_repositories" => handle_installation_repos(&state, &body).await,
        "ping" => (StatusCode::OK, "pong"),
        _ => (StatusCode::OK, "ignored"),
    }
}

async fn handle_pull_request(
    state: &AppState,
    delivery_id: &str,
    body: &[u8],
) -> (StatusCode, &'static str) {
    let event: PullRequestEvent = match serde_json::from_slice(body) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "unparseable pull_request payload");
            return (StatusCode::BAD_REQUEST, "unparseable payload");
        }
    };
    let Some(trigger) = event.trigger() else {
        return (StatusCode::OK, "action not triaged");
    };

    let pr = PullRequestRef {
        installation_id: event.installation.id,
        owner: event.repository.owner.login,
        repo: event.repository.name,
        number: event.number,
    };

    match state.store.enqueue_job(delivery_id, &pr, trigger).await {
        Ok(true) => (StatusCode::ACCEPTED, "queued"),
        Ok(false) => (StatusCode::OK, "duplicate delivery"),
        Err(e) => {
            tracing::error!(error = %e, pr = %pr, "failed to enqueue triage job");
            // Non-2xx so GitHub redelivers.
            (StatusCode::INTERNAL_SERVER_ERROR, "enqueue failed")
        }
    }
}

async fn handle_installation(state: &AppState, body: &[u8]) -> (StatusCode, &'static str) {
    let Ok(event) = serde_json::from_slice::<InstallationEvent>(body) else {
        return (StatusCode::OK, "ignored");
    };
    let delta = match event.action.as_str() {
        "created" => event.repositories.len() as i64,
        "deleted" => -(event.repositories.len() as i64),
        _ => 0,
    };
    if delta != 0 {
        if let Err(e) = state.billing.track_repo(event.installation.id, delta).await {
            tracing::error!(error = %e, installation = event.installation.id, "repo tracking failed");
        }
    }
    (StatusCode::OK, "ok")
}

async fn handle_installation_repos(state: &AppState, body: &[u8]) -> (StatusCode, &'static str) {
    let Ok(event) = serde_json::from_slice::<InstallationRepositoriesEvent>(body) else {
        return (StatusCode::OK, "ignored");
    };
    let delta = event.repositories_added.len() as i64 - event.repositories_removed.len() as i64;
    if delta != 0 {
        if let Err(e) = state.billing.track_repo(event.installation.id, delta).await {
            tracing::error!(error = %e, installation = event.installation.id, "repo tracking failed");
        }
    }
    (StatusCode::OK, "ok")
}
