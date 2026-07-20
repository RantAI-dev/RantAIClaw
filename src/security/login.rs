//! Single-operator console password hashing (argon2id). One-way — the stored
//! PHC string cannot be reversed. Used by the optional gateway login gate
//! (`config.gateway.login`) that protects the web console and TUI.

use anyhow::Result;
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;

/// Auto-lock windows offered by `rantaiclaw setup login`, as
/// `(label, seconds)`. Index 0 is the "off" choice and must stay first — both
/// setup paths fall back to it when a selection is missing.
///
/// Shared by the TUI provisioner and the dialoguer section so the two cannot
/// drift into offering different options for the same setting. The shortest
/// window is deliberately 15 minutes: idleness is measured from operator
/// *input*, and a single long turn produces none of its own, so anything
/// tighter would lock the operator out mid-answer.
pub const IDLE_PRESETS: &[(&str, u64)] = &[
    ("Never", 0),
    ("15 minutes", 900),
    ("30 minutes", 1_800),
    ("1 hour", 3_600),
    ("4 hours", 14_400),
];

/// Hash a plaintext password into an argon2id PHC string (random salt embedded).
///
/// The returned string is safe to store verbatim in `config.toml` — it is
/// non-reversible and carries its own salt, so it does NOT go through the
/// reversible secret-encryption pass used for API keys.
pub fn hash_password(plain: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let phc = Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hashing failed: {e}"))?
        .to_string();
    Ok(phc)
}

/// Verify a plaintext password against a stored PHC string. Returns `false` on
/// any parse or verification error (fail-closed) so a corrupt hash never grants
/// access.
pub fn verify_password(plain: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_round_trips() {
        let phc = hash_password("s3cret-pass").unwrap();
        assert!(phc.starts_with("$argon2"));
        assert!(verify_password("s3cret-pass", &phc));
    }

    #[test]
    fn wrong_password_fails() {
        let phc = hash_password("right").unwrap();
        assert!(!verify_password("wrong", &phc));
    }

    #[test]
    fn malformed_phc_fails_closed() {
        assert!(!verify_password("anything", "not-a-valid-phc-string"));
        assert!(!verify_password("anything", ""));
    }
}
