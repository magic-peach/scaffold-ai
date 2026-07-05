//! Verdict-split behavior of the Autumn billing gate, against wiremock:
//! Allowed / Denied / self-heal on customer_not_found / fail-open on
//! feature_not_found and outages.

use scaffold_autumn::{AutumnBilling, AutumnConfig};
use scaffold_domain::{BillingGate, BillingVerdict, PullRequestRef};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn billing(server: &MockServer) -> AutumnBilling {
    let mut config = AutumnConfig::new("am_sk_test");
    config.base_url = server.uri();
    AutumnBilling::new(config)
}

fn pr() -> PullRequestRef {
    PullRequestRef {
        installation_id: 42,
        owner: "o".into(),
        repo: "r".into(),
        number: 1,
    }
}

#[tokio::test]
async fn allowed_check_maps_to_allowed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"allowed": true})))
        .mount(&server)
        .await;
    assert_eq!(billing(&server).await.check_triage(42).await, BillingVerdict::Allowed);
}

#[tokio::test]
async fn disallowed_check_maps_to_denied() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"allowed": false})))
        .mount(&server)
        .await;
    assert!(matches!(
        billing(&server).await.check_triage(42).await,
        BillingVerdict::Denied { .. }
    ));
}

#[tokio::test]
async fn missing_customer_self_heals_then_allows() {
    let server = MockServer::start().await;
    // First check: customer doesn't exist yet.
    Mock::given(method("POST"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"code": "customer_not_found", "message": "not found", "env": "sandbox"}),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Self-heal: create customer (idempotent upsert).
    Mock::given(method("POST"))
        .and(path("/customers"))
        .and(body_string_contains("gh-install-42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "gh-install-42"})))
        .expect(1)
        .mount(&server)
        .await;
    // Retried check succeeds.
    Mock::given(method("POST"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"allowed": true})))
        .mount(&server)
        .await;

    assert_eq!(billing(&server).await.check_triage(42).await, BillingVerdict::Allowed);
}

#[tokio::test]
async fn missing_feature_fails_open_as_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"code": "feature_not_found", "message": "not found", "env": "sandbox"}),
        ))
        .mount(&server)
        .await;
    // No /customers mock: a self-heal attempt here would 404 the test.
    assert_eq!(
        billing(&server).await.check_triage(42).await,
        BillingVerdict::Unavailable
    );
}

#[tokio::test]
async fn server_error_fails_open_as_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    assert_eq!(
        billing(&server).await.check_triage(42).await,
        BillingVerdict::Unavailable
    );
}

#[tokio::test]
async fn track_self_heals_missing_customer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/track"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"code": "customer_not_found", "message": "not found", "env": "sandbox"}),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/customers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "gh-install-42"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/track"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": 1})))
        .mount(&server)
        .await;

    let result = billing(&server).await.track_triage(42, &pr()).await;
    assert!(result.is_ok(), "expected self-healed track, got {result:?}");
}

#[tokio::test]
async fn track_other_errors_still_fail() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/track"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    assert!(billing(&server).await.track_triage(42, &pr()).await.is_err());
}
