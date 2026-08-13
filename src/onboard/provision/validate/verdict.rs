//! What a credential probe actually proved, and what to do about it.
//!
//! Every probing provisioner used to collapse three very different outcomes
//! into two: 2xx meant "validated", and *everything else* — a rejected token, a
//! DNS failure, a platform outage — meant "may be invalid… Continuing…", with
//! the config write following unconditionally. So a typo'd, expired or revoked
//! credential landed in `config.toml` wearing the same "✅ configured" badge as
//! a working one, and resurfaced later as a silently dead channel.
//!
//! The gateway's equivalent path already decided the opposite policy — "fail
//! closed so we never save a credential that doesn't work" — so this module
//! brings provisioning in line with it, with one deliberate exception: a
//! transport failure is *not* evidence against the credential, and treating it
//! as one would break air-gapped and offline installs.

use super::http::ProbeResult;
use crate::onboard::provision::traits::{ProvisionEvent, ProvisionResponse, Severity};
use anyhow::Result;
use tokio::sync::mpsc;

/// What a probe response says about the credential that was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// The platform answered and accepted the credential.
    Accepted,
    /// The platform answered and rejected the credential. This is evidence.
    Rejected(String),
    /// No answer, or an answer that says nothing about the credential — a
    /// transport failure, a 404 on a changed path, a 5xx outage. Not evidence.
    Inconclusive(String),
}

/// Classify a probe by HTTP status.
///
/// Only 401 and 403 count as rejection. Everything else non-2xx is
/// inconclusive on purpose: a 404 means the probe path is wrong, a 5xx means
/// the platform is down, and neither tells us the operator's token is bad.
///
/// Platforms that answer 200 with an error body (Slack's `{"ok": false}`) can
/// not be classified from the status alone — those provisioners build the
/// verdict themselves and pass it to [`resolve`].
pub fn classify_status(result: &Result<ProbeResult>) -> ProbeVerdict {
    match result {
        Ok(r) if (200..300).contains(&r.status) => ProbeVerdict::Accepted,
        Ok(r) if r.status == 401 || r.status == 403 => {
            ProbeVerdict::Rejected(format!("the platform rejected it (HTTP {})", r.status))
        }
        Ok(r) => ProbeVerdict::Inconclusive(format!("unexpected response (HTTP {})", r.status)),
        Err(e) => ProbeVerdict::Inconclusive(format!("{e}")),
    }
}

/// Whether the caller should go on to write the credential to the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistDecision {
    Persist,
    Discard,
}

impl PersistDecision {
    pub fn should_persist(self) -> bool {
        matches!(self, Self::Persist)
    }
}

/// Report the verdict to the operator and, when the probe did not accept the
/// credential, ask whether to save it anyway.
///
/// `subject` names the thing being validated, e.g. `"bot token"`.
///
/// **Option ordering is load-bearing.** The headless driver answers every
/// `Choose` with option 0, so option 0 is the safe default for each case and
/// they differ:
///
/// - **Rejected** — the platform told us the credential is bad, so the safe
///   default is to discard it. Option 0 is "don't save".
/// - **Inconclusive** — we learned nothing, and an air-gapped or offline
///   install produces this on every probe. Refusing to save would make setup
///   impossible there, so option 0 is "save anyway".
pub async fn resolve(
    events: &mpsc::Sender<ProvisionEvent>,
    responses: &mut mpsc::Receiver<ProvisionResponse>,
    verdict: ProbeVerdict,
    subject: &str,
) -> Result<PersistDecision> {
    match verdict {
        ProbeVerdict::Accepted => {
            emit(
                events,
                Severity::Success,
                format!("Validated the {subject}."),
            )
            .await?;
            Ok(PersistDecision::Persist)
        }
        ProbeVerdict::Rejected(detail) => {
            emit(
                events,
                Severity::Error,
                format!("This {subject} was rejected: {detail}."),
            )
            .await?;
            choose(
                events,
                responses,
                format!("Save the {subject} anyway?"),
                vec![
                    "No — let me correct it".to_string(),
                    "Yes, save it anyway".to_string(),
                ],
                // Index of the "save anyway" option.
                1,
            )
            .await
        }
        ProbeVerdict::Inconclusive(detail) => {
            emit(
                events,
                Severity::Warn,
                format!("Could not check the {subject}: {detail}."),
            )
            .await?;
            choose(
                events,
                responses,
                format!("Save the {subject} without a successful check?"),
                vec![
                    "Yes, save it (offline or air-gapped install)".to_string(),
                    "No — let me correct it".to_string(),
                ],
                0,
            )
            .await
        }
    }
}

async fn choose(
    events: &mpsc::Sender<ProvisionEvent>,
    responses: &mut mpsc::Receiver<ProvisionResponse>,
    label: String,
    options: Vec<String>,
    persist_index: usize,
) -> Result<PersistDecision> {
    events
        .send(ProvisionEvent::Choose {
            id: "save_anyway".into(),
            label,
            options,
            multi: false,
        })
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;

    match responses.recv().await {
        Some(ProvisionResponse::Selection(indices)) => {
            if indices.first().copied() == Some(persist_index) {
                Ok(PersistDecision::Persist)
            } else {
                Ok(PersistDecision::Discard)
            }
        }
        // A cancel or a closed channel is not consent to save a credential the
        // platform would not accept.
        _ => Ok(PersistDecision::Discard),
    }
}

async fn emit(
    events: &mpsc::Sender<ProvisionEvent>,
    severity: Severity,
    text: String,
) -> Result<()> {
    events
        .send(ProvisionEvent::Message { severity, text })
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(status: u16) -> Result<ProbeResult> {
        Ok(ProbeResult {
            status,
            body: String::new(),
        })
    }

    #[test]
    fn two_hundreds_are_accepted() {
        assert_eq!(classify_status(&ok(200)), ProbeVerdict::Accepted);
        assert_eq!(classify_status(&ok(204)), ProbeVerdict::Accepted);
    }

    /// The distinction the whole module exists for: only the platform saying
    /// "no" is evidence against the credential.
    #[test]
    fn only_401_and_403_are_a_rejection() {
        assert!(matches!(
            classify_status(&ok(401)),
            ProbeVerdict::Rejected(_)
        ));
        assert!(matches!(
            classify_status(&ok(403)),
            ProbeVerdict::Rejected(_)
        ));
        assert!(matches!(
            classify_status(&ok(404)),
            ProbeVerdict::Inconclusive(_)
        ));
        assert!(matches!(
            classify_status(&ok(500)),
            ProbeVerdict::Inconclusive(_)
        ));
    }

    #[test]
    fn a_transport_error_is_inconclusive_not_a_rejection() {
        let err: Result<ProbeResult> = Err(anyhow::anyhow!("dns failure"));
        assert!(matches!(
            classify_status(&err),
            ProbeVerdict::Inconclusive(_)
        ));
    }

    async fn drive(verdict: ProbeVerdict, answer: Option<usize>) -> PersistDecision {
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
        if let Some(i) = answer {
            resp_tx
                .send(ProvisionResponse::Selection(vec![i]))
                .await
                .unwrap();
        } else {
            drop(resp_tx);
        }
        resolve(&events_tx, &mut resp_rx, verdict, "bot token")
            .await
            .expect("resolve")
    }

    #[tokio::test]
    async fn an_accepted_probe_persists_without_asking() {
        assert_eq!(
            drive(ProbeVerdict::Accepted, None).await,
            PersistDecision::Persist
        );
    }

    /// Headless answers every `Choose` with option 0. On a rejection that must
    /// be "don't save", or unattended setup writes credentials the platform has
    /// already refused.
    #[tokio::test]
    async fn a_rejection_discards_on_the_headless_default() {
        assert_eq!(
            drive(ProbeVerdict::Rejected("HTTP 401".into()), Some(0)).await,
            PersistDecision::Discard
        );
        assert_eq!(
            drive(ProbeVerdict::Rejected("HTTP 401".into()), Some(1)).await,
            PersistDecision::Persist
        );
    }

    /// The mirror case: an offline install produces an inconclusive probe on
    /// every channel, so option 0 must keep setup working.
    #[tokio::test]
    async fn an_inconclusive_probe_persists_on_the_headless_default() {
        assert_eq!(
            drive(ProbeVerdict::Inconclusive("dns failure".into()), Some(0)).await,
            PersistDecision::Persist
        );
        assert_eq!(
            drive(ProbeVerdict::Inconclusive("dns failure".into()), Some(1)).await,
            PersistDecision::Discard
        );
    }

    #[tokio::test]
    async fn a_cancelled_confirmation_never_persists() {
        assert_eq!(
            drive(ProbeVerdict::Rejected("HTTP 401".into()), None).await,
            PersistDecision::Discard
        );
        assert_eq!(
            drive(ProbeVerdict::Inconclusive("offline".into()), None).await,
            PersistDecision::Discard
        );
    }
}
