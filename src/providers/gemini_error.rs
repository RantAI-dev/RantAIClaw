//! The marker for a Gemini credential refusal that no retry can fix.
//!
//! A refused Gemini request is decided locally, before anything is sent: the
//! credential is missing. A retry cannot change that outcome — it can only
//! spend the operator's time waiting out a backoff that ends in the same
//! sentence. The type is how the retry wrapper learns it, without reading the
//! message text.

use thiserror::Error;

/// A Gemini request refused because the credential is not usable.
///
/// The `Display` text is the operator-facing message.
#[derive(Debug, Clone, Error)]
pub(crate) enum GeminiAuthError {
    /// No Gemini credential is available at all.
    #[error("{}", crate::providers::gemini::MISSING_CREDENTIAL_REFUSAL)]
    MissingCredential,
}

impl GeminiAuthError {
    /// The operator-facing sentence this refusal carries.
    pub(crate) fn refusal_text(&self) -> &'static str {
        match self {
            Self::MissingCredential => crate::providers::gemini::MISSING_CREDENTIAL_REFUSAL,
        }
    }
}
