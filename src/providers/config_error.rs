//! The marker for a provider error that no retry can fix.
//!
//! A caller decides what is fatal by the TYPE of the error, never by its
//! message text: a missing credential is repaired by `rantaiclaw auth login`,
//! by an environment variable, or by a key stored in the config later, and it
//! must be retried; an error of this type follows from the config file alone
//! and fails identically on every retry.

use thiserror::Error;

/// A provider configuration error: the `default_provider` value names a
/// provider the factory does not know, or carries a custom endpoint URL that
/// is missing, unparseable, or not http(s).
///
/// No credential, no login, no environment variable and no DNS lookup can
/// change the outcome, so a caller may treat this as fatal. The `Display`
/// text is the operator-facing message and stays byte-for-byte what it was
/// before this type existed.
#[derive(Debug, Error)]
pub enum ProviderConfigError {
    /// The provider name is not one the factory knows.
    ///
    /// `guidance` carries the operator-facing remediation text, and it is
    /// deliberately a caller-supplied field rather than part of the
    /// `#[error(...)]` attribute. Moving the text into the attribute would
    /// shorten the match arm in `create_provider_with_url_and_options` to a
    /// few characters, and rustfmt would then reflow the ~30 neighbouring
    /// arms of that match — mixing a large formatting-only change into a
    /// functional one (CLAUDE.md §10). The trade-off buys a diff that only
    /// touches the code it has to touch; the `Display` text is unchanged
    /// either way.
    #[error("Unknown provider: {name}. {guidance}")]
    UnknownProvider { name: String, guidance: String },

    /// The custom endpoint URL is empty.
    #[error("{provider_label} requires a URL. Format: {format_hint}")]
    CustomUrlMissing {
        provider_label: String,
        format_hint: String,
    },

    /// The custom endpoint URL does not parse.
    #[error("{provider_label} requires a valid URL. Format: {format_hint}")]
    CustomUrlInvalid {
        provider_label: String,
        format_hint: String,
    },

    /// The custom endpoint URL is not http(s).
    #[error("{provider_label} requires an http:// or https:// URL. Format: {format_hint}")]
    CustomUrlScheme {
        provider_label: String,
        format_hint: String,
    },
}
