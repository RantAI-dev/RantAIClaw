//! A config failure that a restart cannot fix.
//!
//! The installed unit carries `Restart=always` with
//! `RestartPreventExitStatus=78`, so a daemon that exits 1 is restarted
//! every few seconds. A failure that follows from the files on disk alone —
//! a profile that names nothing, a config that does not parse, a config
//! written by a newer schema — fails the same way on every start, so the
//! daemon must exit 78 instead: the unit stays stopped and the journal
//! keeps the sentence that says why.
//!
//! A failure that CAN pass on a retry — a write that failed for disk space
//! during the first-run init — is deliberately NOT this type. It keeps exit
//! 1, so systemd keeps retrying it and the daemon comes up on its own.

/// A config failure that follows from the files on disk alone.
///
/// Typed so the daemon can classify it without reading the message: the
/// sentence is for the operator, the type is for the exit code.
#[derive(Debug)]
pub struct PersistentConfigError {
    sentence: String,
}

impl PersistentConfigError {
    /// Mark a sentence as a failure a restart cannot fix.
    pub fn new(sentence: impl Into<String>) -> Self {
        Self {
            sentence: sentence.into(),
        }
    }

    /// Mark an error as a failure a restart cannot fix, keeping its
    /// message (the whole chain, so a context is not lost).
    pub fn from_error(error: &anyhow::Error) -> Self {
        Self {
            sentence: format!("{error:#}"),
        }
    }
}

impl std::fmt::Display for PersistentConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.sentence)
    }
}

impl std::error::Error for PersistentConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The type is what the daemon classifies on, so it has to survive the
    /// trip through an `anyhow` chain: a caller downcasts the error it got
    /// back and finds this type.
    #[test]
    fn the_error_survives_the_anyhow_chain() {
        let error: anyhow::Error = PersistentConfigError::new("the config is broken").into();
        let error = error.context("while loading the config");

        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<PersistentConfigError>().is_some()),
            "the persistent failure must be visible in the chain"
        );
    }

    /// A retryable failure is not this type, so it keeps exit 1.
    #[test]
    fn a_plain_error_is_not_a_persistent_failure() {
        let error: anyhow::Error = anyhow::anyhow!("the disk is full");

        assert!(
            !error
                .chain()
                .any(|cause| cause.downcast_ref::<PersistentConfigError>().is_some()),
            "a failure that can pass on a retry must not be marked persistent"
        );
    }
}
