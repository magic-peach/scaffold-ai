use std::sync::Arc;

use scaffold_domain::{BillingGate, PullRequestHost, TriageModel, TriageStore};

/// Everything the routes and worker need, behind trait objects so tests can
/// swap in fakes for any boundary.
pub struct AppState {
    pub webhook_secret: String,
    pub store: Arc<dyn TriageStore>,
    pub host: Arc<dyn PullRequestHost>,
    pub model: Arc<dyn TriageModel>,
    pub billing: Arc<dyn BillingGate>,
}
