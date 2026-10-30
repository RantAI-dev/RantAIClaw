//! The marker for a Gemini credential refusal that no retry can fix.
//!
//! A refused Gemini login is decided locally, before anything is sent: the
//! credential is missing, or the CLI login has expired. A retry cannot change
//! that outcome — it can only spend the operator's time waiting out a backoff
//! that ends in the same sentence. The type is how the retry wrapper learns
//! it, without reading the message text.

use thiserror::Error;

/// A Gemini request refused because the credential is not usable.
///
/// The `Display` text is the operator-facing message. The expired-login
/// sentence is byte-for-byte what it was before this type existed; the
/// missing-credential one names the second fix and says that no restart is
/// needed, which it did not before.
#[derive(Debug, Clone, Error)]
pub(crate) enum GeminiAuthError {
    /// The Gemini CLI login has expired: renew it with `gemini`, or set
    /// `GEMINI_API_KEY`.
    #[error("{}", crate::providers::gemini_cli::EXPIRED_LOGIN_REFUSAL)]
    ExpiredCliLogin,

    /// No Gemini credential is available at all.
    #[error("{}", crate::providers::gemini::MISSING_CREDENTIAL_REFUSAL)]
    MissingCredential,
}
