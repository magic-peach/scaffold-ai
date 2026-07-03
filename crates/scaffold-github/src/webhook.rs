//! Webhook signature verification and payload types.

use hmac::{Hmac, Mac};
use scaffold_domain::TriageTrigger;
use serde::Deserialize;
use sha2::Sha256;

/// Verify GitHub's `X-Hub-Signature-256` header (constant-time comparison)
/// against the raw request body. Must run before any JSON parsing.
pub fn verify_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// Compute the signature header value for a body — used by tests to sign
/// synthetic deliveries.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[derive(Debug, Deserialize)]
pub struct PullRequestEvent {
    pub action: String,
    pub number: u64,
    pub installation: Installation,
    pub repository: Repository,
}

#[derive(Debug, Deserialize)]
pub struct InstallationRepositoriesEvent {
    pub action: String,
    pub installation: Installation,
    #[serde(default)]
    pub repositories_added: Vec<RepositorySummary>,
    #[serde(default)]
    pub repositories_removed: Vec<RepositorySummary>,
}

#[derive(Debug, Deserialize)]
pub struct InstallationEvent {
    pub action: String,
    pub installation: Installation,
    #[serde(default)]
    pub repositories: Vec<RepositorySummary>,
}

#[derive(Debug, Deserialize)]
pub struct Installation {
    pub id: u64,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub name: String,
    pub owner: Owner,
}

#[derive(Debug, Deserialize)]
pub struct RepositorySummary {
    pub full_name: String,
}

#[derive(Debug, Deserialize)]
pub struct Owner {
    pub login: String,
}

impl PullRequestEvent {
    /// Map the webhook `action` to a triage trigger; None for actions we
    /// don't triage (closed, labeled, edited, ...).
    pub fn trigger(&self) -> Option<TriageTrigger> {
        match self.action.as_str() {
            "opened" => Some(TriageTrigger::Opened),
            "reopened" => Some(TriageTrigger::Reopened),
            "synchronize" => Some(TriageTrigger::Synchronized),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_roundtrip() {
        let secret = "shhh";
        let body = b"{\"hello\":\"world\"}";
        let header = sign(secret, body);
        assert!(verify_signature(secret, body, &header));
        assert!(!verify_signature("wrong", body, &header));
        assert!(!verify_signature(secret, b"tampered", &header));
        assert!(!verify_signature(secret, body, "sha256=nothex"));
        assert!(!verify_signature(secret, body, "sha1=deadbeef"));
    }
}
