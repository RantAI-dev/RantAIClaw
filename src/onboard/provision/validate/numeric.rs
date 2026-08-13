//! Numeric prompts that do not silently discard what the operator typed.
//!
//! Every numeric prompt in the provisioners ended in `.parse().unwrap_or(N)`,
//! so a typo'd port became the default without a word. The operator typed
//! `6967`, setup wrote `6697`, and the mismatch surfaced later as a channel
//! that connects to the wrong place — or, in Lark's case, as `None`, which
//! fails at runtime instead of at setup.
//!
//! Empty still means "take the default". That is what the default is for.

use crate::onboard::provision::traits::{ProvisionEvent, ProvisionResponse, Severity};
use anyhow::Result;
use std::fmt::Display;
use std::str::FromStr;
use tokio::sync::mpsc;

/// How many times to re-ask before giving up and taking the default.
///
/// Bounded because the headless driver answers every prompt from a script: if
/// it keeps sending something unparseable, an unbounded loop would hang setup
/// until the 120s timeout instead of finishing with a usable value.
const MAX_ATTEMPTS: usize = 3;

/// Prompt for a number, re-asking when the answer cannot be parsed.
///
/// Returns `default` for an empty answer, or after `MAX_ATTEMPTS` unparseable
/// ones — saying so, rather than pretending the operator asked for it.
pub async fn prompt_number<T>(
    events: &mpsc::Sender<ProvisionEvent>,
    responses: &mut mpsc::Receiver<ProvisionResponse>,
    id: &str,
    label: &str,
    default: T,
) -> Result<T>
where
    T: FromStr + Display + Copy,
{
    for attempt in 1..=MAX_ATTEMPTS {
        events
            .send(ProvisionEvent::Prompt {
                id: id.to_string(),
                label: label.to_string(),
                default: Some(default.to_string()),
                secret: false,
            })
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;

        let raw = match responses.recv().await {
            Some(ProvisionResponse::Text(t)) => t,
            Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
            Some(_) => anyhow::bail!("unexpected response"),
            None => anyhow::bail!("channel closed"),
        };

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        if let Ok(parsed) = trimmed.parse::<T>() {
            return Ok(parsed);
        }

        let text = if attempt < MAX_ATTEMPTS {
            format!("`{trimmed}` is not a valid number — try again, or press Enter for {default}.")
        } else {
            format!("`{trimmed}` is not a valid number; using {default}.")
        };
        events
            .send(ProvisionEvent::Message {
                severity: Severity::Warn,
                text,
            })
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
    }

    Ok(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ask(answers: &[&str]) -> (u16, Vec<String>) {
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        for a in answers {
            resp_tx
                .send(ProvisionResponse::Text((*a).to_string()))
                .await
                .unwrap();
        }
        let collector = tokio::spawn(async move {
            let mut warnings = Vec::new();
            while let Some(ev) = events_rx.recv().await {
                if let ProvisionEvent::Message { text, .. } = ev {
                    warnings.push(text);
                }
            }
            warnings
        });
        let got = prompt_number(&events_tx, &mut resp_rx, "port", "Port", 6697u16)
            .await
            .expect("prompt");
        drop(events_tx);
        (got, collector.await.unwrap())
    }

    #[tokio::test]
    async fn a_valid_number_is_taken_as_typed() {
        let (got, warnings) = ask(&["6667"]).await;
        assert_eq!(got, 6667);
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn empty_takes_the_default_without_complaint() {
        let (got, warnings) = ask(&["   "]).await;
        assert_eq!(got, 6697);
        assert!(warnings.is_empty());
    }

    /// The behaviour this module exists for: a typo used to become the default
    /// silently, so the operator never learned their input was discarded.
    #[tokio::test]
    async fn an_unparseable_answer_reprompts_instead_of_defaulting() {
        let (got, warnings) = ask(&["66o7", "6667"]).await;
        assert_eq!(got, 6667, "the corrected answer must win");
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("not a valid number"),
            "{}",
            warnings[0]
        );
    }

    /// Bounded so a scripted headless driver cannot hang setup.
    #[tokio::test]
    async fn it_gives_up_after_three_tries_and_says_so() {
        let (got, warnings) = ask(&["x", "y", "z"]).await;
        assert_eq!(got, 6697);
        assert_eq!(warnings.len(), 3);
        assert!(warnings[2].contains("using 6697"), "{}", warnings[2]);
    }
}
