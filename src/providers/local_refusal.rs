//! The marker for a request a provider refuses locally, before anything is sent.
//!
//! The refusal follows from what the process already holds: a credential is
//! not set, or the request names a route the configuration cannot serve. A
//! retry sees the same inputs and ends in the same sentence after the backoff,
//! so it can only spend the operator's time. The retry wrapper recognises the
//! refusal by this type, never by its wording.

use super::gemini_error::GeminiAuthError;
use thiserror::Error;

/// A request refused before anything was sent, where no retry can help.
///
/// The `Display` text is the operator-facing message, exactly as the provider
/// wrote it.
#[derive(Debug, Clone, Error)]
#[error("{0}")]
pub(crate) struct LocalRefusal(String);

impl LocalRefusal {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Whether `error`, or any error it wraps, is a refusal decided locally.
///
/// `GeminiAuthError` is the older of the two types and stays as it is.
pub(crate) fn is_local_refusal(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<LocalRefusal>().is_some()
            || cause.downcast_ref::<GeminiAuthError>().is_some()
    })
}
