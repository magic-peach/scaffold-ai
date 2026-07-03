//! Persistence: a Postgres implementation of `TriageStore` (production) and
//! an in-memory implementation (tests, local experiments).

pub mod memory;
mod pg;

pub use memory::InMemoryStore;
pub use pg::PgStore;

use scaffold_domain::TriageTrigger;

pub(crate) fn trigger_to_str(t: TriageTrigger) -> &'static str {
    match t {
        TriageTrigger::Opened => "opened",
        TriageTrigger::Reopened => "reopened",
        TriageTrigger::Synchronized => "synchronized",
    }
}

pub(crate) fn trigger_from_str(s: &str) -> TriageTrigger {
    match s {
        "reopened" => TriageTrigger::Reopened,
        "synchronized" => TriageTrigger::Synchronized,
        _ => TriageTrigger::Opened,
    }
}

/// Attempts after which a job is declared dead and the worker degrades to
/// human escalation.
pub const MAX_JOB_ATTEMPTS: i32 = 3;
