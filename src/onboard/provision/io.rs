//! The three calls every provisioner makes to talk to its driver.
//!
//! These were defined identically in **36** modules — every channel, every
//! runtime surface, and the core provisioners. That duplication is why each
//! structural defect in this subsystem appeared uniformly in all of them and
//! had to be fixed in all of them: plans 132 and 133 each touched a dozen-plus
//! files to make one change.
//!
//! Three copies had already drifted, in `knowledge.rs`, `persona.rs` and
//! `login.rs`: their `send` said "failed to send provision event" and their
//! receivers said "unexpected response type". Nothing reads those strings —
//! they are wrapped in `anyhow` and only ever displayed — so unifying on the
//! majority wording is safe, and it is recorded here rather than left as a
//! silent normalisation.

use super::traits::{ProvisionEvent, ProvisionResponse};
use anyhow::Result;
use tokio::sync::mpsc;

/// Emit an event to the driver.
///
/// A closed channel means the driver is gone — the overlay was dismissed, or
/// the headless render loop ended — so the provisioner cannot continue and
/// this is an error rather than something to swallow.
pub async fn send(events: &mpsc::Sender<ProvisionEvent>, ev: ProvisionEvent) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))
}

/// Await a text answer to the prompt just sent.
pub async fn recv_text(responses: &mut mpsc::Receiver<ProvisionResponse>) -> Result<String> {
    match responses.recv().await {
        Some(ProvisionResponse::Text(t)) => Ok(t),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

/// Await a selection answer to the choice just sent.
pub async fn recv_selection(
    responses: &mut mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recv_text_returns_the_answer() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(ProvisionResponse::Text("answer".into()))
            .await
            .unwrap();
        assert_eq!(recv_text(&mut rx).await.unwrap(), "answer");
    }

    #[tokio::test]
    async fn recv_selection_returns_the_indices() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(ProvisionResponse::Selection(vec![2]))
            .await
            .unwrap();
        assert_eq!(recv_selection(&mut rx).await.unwrap(), vec![2]);
    }

    /// A cancel and a wrong-typed answer must both be errors, not defaults:
    /// treating either as an empty string would look to the provisioner like
    /// the operator deliberately skipped a required field.
    #[tokio::test]
    async fn a_cancel_is_an_error_on_both_receivers() {
        let (tx, mut rx) = mpsc::channel(2);
        tx.send(ProvisionResponse::Cancelled).await.unwrap();
        assert!(recv_text(&mut rx).await.is_err());
        tx.send(ProvisionResponse::Cancelled).await.unwrap();
        assert!(recv_selection(&mut rx).await.is_err());
    }

    #[tokio::test]
    async fn a_mismatched_response_kind_is_an_error() {
        let (tx, mut rx) = mpsc::channel(2);
        tx.send(ProvisionResponse::Selection(vec![0]))
            .await
            .unwrap();
        assert!(recv_text(&mut rx).await.is_err());
        tx.send(ProvisionResponse::Text("x".into())).await.unwrap();
        assert!(recv_selection(&mut rx).await.is_err());
    }

    #[tokio::test]
    async fn a_closed_channel_is_an_error() {
        let (tx, mut rx) = mpsc::channel::<ProvisionResponse>(1);
        drop(tx);
        assert!(recv_text(&mut rx).await.is_err());
    }

    #[tokio::test]
    async fn send_fails_once_the_driver_is_gone() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let r = send(
            &tx,
            ProvisionEvent::Done {
                summary: "x".into(),
            },
        )
        .await;
        assert!(
            r.is_err(),
            "a dropped driver must surface, not be swallowed"
        );
    }
}
