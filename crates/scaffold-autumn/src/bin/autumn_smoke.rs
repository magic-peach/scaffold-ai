//! Live smoke test against the real Autumn API.
//!
//!   set -a; source .env; set +a
//!   cargo run -p scaffold-autumn --bin autumn_smoke
//!
//! Uses a disposable customer, saves redacted raw responses to
//! smoke-test-fixtures/, diffs response shapes against scaffold-autumn's
//! serde structs, and probes failure paths through the real BillingGate
//! impl. Never prints the secret key.

use scaffold_autumn::{customer_id, AutumnBilling, AutumnConfig, FEATURE_PR_TRIAGE};
use scaffold_domain::{BillingGate, PullRequestRef};
use serde_json::{json, Value};

const FIXTURE_DIR: &str = "smoke-test-fixtures";

struct Smoke {
    http: reqwest::Client,
    base: String,
    secret: String,
    step: u32,
}

impl Smoke {
    /// POST, save a redacted fixture, return (status, body).
    async fn post(&mut self, name: &str, path: &str, body: Value) -> (u16, Value) {
        let resp = self
            .http
            .post(format!("{}{}", self.base, path))
            .bearer_auth(&self.secret)
            .json(&body)
            .send()
            .await
            .expect("network error talking to Autumn");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let value: Value =
            serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));

        self.step += 1;
        let fixture = json!({
            "request": { "method": "POST", "path": path, "body": body },
            "response": { "status": status, "body": value },
        });
        let redacted = redact(&fixture, &self.secret);
        let file = format!("{FIXTURE_DIR}/{:02}-{name}.json", self.step);
        std::fs::write(&file, serde_json::to_string_pretty(&redacted).unwrap())
            .expect("write fixture");
        println!("  [{status}] POST {path}  -> {file}");
        (status, value)
    }
}

/// Recursively scrub the secret and any URL query strings (checkout links can
/// embed session tokens).
fn redact(value: &Value, secret: &str) -> Value {
    match value {
        Value::String(s) => {
            let mut s = s.replace(secret, "[REDACTED_KEY]");
            if s.starts_with("http") {
                if let Some(q) = s.find('?') {
                    s.truncate(q);
                    s.push_str("?[REDACTED_QUERY]");
                }
            }
            Value::String(s)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| redact(v, secret)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact(v, secret)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Which top-level keys does the real response have vs what our struct reads?
fn shape_report(context: &str, body: &Value, fields_we_read: &[&str]) {
    let Some(map) = body.as_object() else {
        println!("  SHAPE {context}: response is not a JSON object!");
        return;
    };
    let present: Vec<&str> = map.keys().map(|s| s.as_str()).collect();
    let missing: Vec<&&str> = fields_we_read
        .iter()
        .filter(|f| !map.contains_key(**f))
        .collect();
    let extra: Vec<&str> = present
        .iter()
        .filter(|k| !fields_we_read.contains(*k))
        .copied()
        .collect();
    println!("  SHAPE {context}: fields we deserialize: {fields_we_read:?}");
    if missing.is_empty() {
        println!("    all expected fields present");
    } else {
        println!("    MISMATCH — expected fields ABSENT from real response: {missing:?}");
    }
    println!("    extra fields we ignore: {extra:?}");
}

#[tokio::main]
async fn main() {
    let secret = std::env::var("AUTUMN_SECRET_KEY")
        .expect("AUTUMN_SECRET_KEY not set (run: set -a; source .env; set +a)");
    let base = std::env::var("AUTUMN_BASE_URL")
        .unwrap_or_else(|_| "https://api.useautumn.com/v1".to_string());
    std::fs::create_dir_all(FIXTURE_DIR).expect("create fixture dir");

    let disposable = format!("scaffold-smoke-{}", chrono::Utc::now().timestamp());
    println!("disposable customer: {disposable}\nbase: {base}\n");

    let mut smoke = Smoke {
        http: reqwest::Client::new(),
        base: base.clone(),
        secret: secret.clone(),
        step: 0,
    };

    // --- setup: customer + feature -----------------------------------------
    println!("== setup ==");
    smoke
        .post(
            "create-customer",
            "/customers",
            json!({ "id": disposable, "name": "Scaffold smoke test" }),
        )
        .await;

    let (feat_status, _) = smoke
        .post(
            "create-feature",
            "/features",
            json!({
                "id": FEATURE_PR_TRIAGE,
                "name": "PR triage",
                "type": "metered",
                "consumable": true,
            }),
        )
        .await;
    if !(200..300).contains(&(feat_status as u32)) && feat_status != 409 {
        println!("  note: feature creation returned {feat_status} — later checks may show feature_not_found instead of the happy path");
    }
    smoke
        .post(
            "create-repos-feature",
            "/features",
            json!({
                "id": scaffold_autumn::FEATURE_REPOS,
                "name": "Repos",
                "type": "metered",
                "consumable": false,
            }),
        )
        .await;

    // --- happy path: check -> track -> check --------------------------------
    println!("\n== check / track / check ==");
    let (_, check1) = smoke
        .post(
            "check",
            "/check",
            json!({ "customer_id": disposable, "feature_id": FEATURE_PR_TRIAGE }),
        )
        .await;
    shape_report("/check", &check1, &["allowed"]);

    let (_, track) = smoke
        .post(
            "track",
            "/track",
            json!({ "customer_id": disposable, "feature_id": FEATURE_PR_TRIAGE, "value": 1 }),
        )
        .await;
    shape_report("/track (we ignore the body)", &track, &[]);

    let (_, check2) = smoke
        .post(
            "check-after-track",
            "/check",
            json!({ "customer_id": disposable, "feature_id": FEATURE_PR_TRIAGE }),
        )
        .await;
    shape_report("/check after track", &check2, &["allowed"]);

    // --- failure paths (raw) -------------------------------------------------
    println!("\n== failure paths (raw API) ==");
    smoke
        .post(
            "check-missing-feature",
            "/check",
            json!({ "customer_id": disposable, "feature_id": "definitely_not_a_feature_xyz" }),
        )
        .await;
    smoke
        .post(
            "check-missing-customer",
            "/check",
            json!({ "customer_id": "scaffold-smoke-never-created", "feature_id": FEATURE_PR_TRIAGE }),
        )
        .await;
    smoke
        .post(
            "attach-missing-product",
            "/attach",
            json!({ "customer_id": disposable, "product_id": "definitely_not_a_product_xyz" }),
        )
        .await;

    // --- failure paths through our actual BillingGate impl -------------------
    println!("\n== BillingGate verdicts (through scaffold-autumn code) ==");
    let mut config = AutumnConfig::new(secret.clone());
    config.base_url = base.clone();
    let billing = AutumnBilling::new(config);

    let fake_install: u64 = 999_999_999;
    println!(
        "  customer for verdict probes: {} (does not exist in Autumn)",
        customer_id(fake_install)
    );
    let verdict = billing.check_triage(fake_install).await;
    println!("  check_triage(nonexistent customer) -> {verdict:?}");

    let pr = PullRequestRef {
        installation_id: fake_install,
        owner: "smoke".into(),
        repo: "smoke".into(),
        number: 1,
    };
    match billing.track_triage(fake_install, &pr).await {
        Ok(()) => println!("  track_triage(nonexistent customer) -> Ok(())"),
        Err(e) => println!("  track_triage(nonexistent customer) -> Err: {e}"),
    }

    println!("\ndone — fixtures in {FIXTURE_DIR}/ (secrets redacted)");
}
