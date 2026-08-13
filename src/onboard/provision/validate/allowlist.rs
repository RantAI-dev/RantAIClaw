//! Telling the operator how wide the allowlist they just typed actually is.
//!
//! Three provisioners used to map an empty answer to `vec!["*"]` — allow
//! anyone — under a prompt whose own label said "empty = deny all". Five more
//! pre-filled the prompt with `*`, so pressing Enter through setup opened the
//! channel to the whole platform. Nothing warned about either.
//!
//! The gateway's config API already warns for both cases; this is the same
//! wording for the provisioning path.

use crate::onboard::provision::traits::{ProvisionEvent, Severity};
use anyhow::Result;
use tokio::sync::mpsc;

/// Warn when the allowlist denies everyone, or lets in everyone.
///
/// `subject` names the list as the operator saw it, e.g. `"Allowed senders"`.
/// Neither case is an error: denying all is the safe default for a channel the
/// operator has not finished setting up, and `*` is legitimate for a private
/// bot on a private server. They just must not be silent.
pub async fn warn_on_reach(
    events: &mpsc::Sender<ProvisionEvent>,
    entries: &[String],
    subject: &str,
) -> Result<()> {
    let text = if entries.is_empty() {
        format!("{subject} is empty — this channel will ignore EVERY sender until you add some.")
    } else if entries.iter().any(|e| e.trim() == "*") {
        format!(
            "{subject} contains \"*\" — this channel will answer ANYONE who messages it. \
             Use specific entries unless that is intentional."
        )
    } else {
        return Ok(());
    };

    events
        .send(ProvisionEvent::Message {
            severity: Severity::Warn,
            text,
        })
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn warnings_for(entries: &[&str]) -> Vec<String> {
        let (tx, mut rx) = mpsc::channel(8);
        let owned: Vec<String> = entries.iter().map(|s| (*s).to_string()).collect();
        warn_on_reach(&tx, &owned, "Allowed senders")
            .await
            .expect("warn");
        drop(tx);
        let mut out = Vec::new();
        while let Some(ProvisionEvent::Message { text, .. }) = rx.recv().await {
            out.push(text);
        }
        out
    }

    #[tokio::test]
    async fn an_empty_allowlist_says_it_denies_everyone() {
        let w = warnings_for(&[]).await;
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("EVERY sender"), "{}", w[0]);
    }

    #[tokio::test]
    async fn a_wildcard_says_it_admits_anyone() {
        let w = warnings_for(&["*"]).await;
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("ANYONE"), "{}", w[0]);
    }

    /// A wildcard buried in a longer list is the same hazard — the runtime
    /// stops at the first `*` it sees.
    #[tokio::test]
    async fn a_wildcard_among_real_entries_still_warns() {
        let w = warnings_for(&["+15550001111", " * ", "+15550002222"]).await;
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("ANYONE"), "{}", w[0]);
    }

    #[tokio::test]
    async fn a_specific_list_is_quiet() {
        assert!(warnings_for(&["+15550001111", "+15550002222"])
            .await
            .is_empty());
    }
}
