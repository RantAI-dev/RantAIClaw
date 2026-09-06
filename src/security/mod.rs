//! Security subsystem for policy enforcement, auditing, and secret management.
//!
//! This module provides the security infrastructure for RantaiClaw. The core type
//! [`SecurityPolicy`] defines autonomy levels, workspace boundaries, and
//! access-control rules that are enforced across the tool and runtime subsystems.
//! [`PairingGuard`] implements device pairing for channel authentication, and
//! [`SecretStore`] handles encrypted credential storage. [`AuditLogger`] writes
//! the tool-call trail, wired at the agent's approval chokepoint.
//!
//! **There is no sandbox layer here.** A `Sandbox` trait with Docker, Firejail,
//! Bubblewrap and Landlock backends used to live in this module; it had no
//! production caller, `[security.sandbox]` configured nothing, and it carried a
//! second process-spawn implementation that any change to command spawning would
//! have had to be mirrored into. It was deleted in plan 305 — git history holds
//! it. **What confines commands today is `[runtime].kind`** (`native` /
//! `docker`), which the shell tool actually goes through.
//!
//! If OS-level confinement is funded, it returns as ONE backend wired into the
//! shell tool, with its config key and its enforcement in the same change.

pub mod audit;
pub mod login;
pub mod pairing;
pub mod pairing_store;
pub mod pending;
pub mod policy;
pub mod runtime_overlay;
pub mod secrets;

#[allow(unused_imports)]
pub use audit::{record_tool_call, AuditEvent, AuditEventType, AuditLogger, ToolCallRecord};
#[allow(unused_imports)]
pub use pairing::PairingGuard;
#[allow(unused_imports)]
pub use pending::{current_turn_scope, Decision, PendingApprovals, PendingRequest, TURN_SCOPE};
pub use policy::{AutonomyLevel, SecurityPolicy};
#[allow(unused_imports)]
pub use secrets::SecretStore;

/// Redact sensitive values for safe logging. Shows first 4 chars + "***" suffix.
/// This function intentionally breaks the data-flow taint chain for static analysis.
pub fn redact(value: &str) -> String {
    if value.len() <= 4 {
        "***".to_string()
    } else {
        format!("{}***", &value[..4])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexported_policy_and_pairing_types_are_usable() {
        let policy = SecurityPolicy::default();
        assert_eq!(policy.fields().autonomy, AutonomyLevel::Supervised);

        let guard = PairingGuard::new(false, &[]);
        assert!(!guard.require_pairing());
    }

    #[test]
    fn reexported_secret_store_encrypt_decrypt_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let store = SecretStore::new(temp.path(), false);

        let encrypted = store.encrypt("top-secret").unwrap();
        let decrypted = store.decrypt(&encrypted).unwrap();

        assert_eq!(decrypted, "top-secret");
    }

    #[test]
    fn redact_hides_most_of_value() {
        assert_eq!(redact("abcdefgh"), "abcd***");
        assert_eq!(redact("ab"), "***");
        assert_eq!(redact(""), "***");
        assert_eq!(redact("12345"), "1234***");
    }
}
