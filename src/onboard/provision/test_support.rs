//! Driving a provisioner from a test with scripted answers.
//!
//! A provisioner talks over two bounded channels, so a test that queues
//! answers and then awaits `run` deadlocks as soon as the event channel fills.
//! This drains events concurrently and hands back everything both sides said.

use super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, ProvisionResponse, TuiProvisioner,
};
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;

/// One scripted answer to whatever the provisioner asks next.
pub enum Answer {
    Text(&'static str),
    Pick(usize),
}

pub struct Transcript {
    pub outcome: Result<ProvisionOutcome>,
    pub events: Vec<ProvisionEvent>,
}

impl Transcript {
    /// Every `Message` body, in order. Prompts and choices are excluded.
    pub fn messages(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ProvisionEvent::Message { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every `Prompt` label, in order.
    pub fn prompts(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ProvisionEvent::Prompt { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn aborted(&self) -> bool {
        matches!(&self.outcome, Ok(ProvisionOutcome::Aborted(_)))
    }

    pub fn configured(&self) -> bool {
        matches!(&self.outcome, Ok(ProvisionOutcome::Configured))
    }
}

/// Run `prov` against `config`, answering its prompts from `answers` in order.
///
/// Answers past the end of the script are not supplied, so the provisioner's
/// `recv_*` sees a closed channel and bails — which is what a test wants when
/// it only cares about the first few steps.
pub async fn drive(
    prov: &dyn TuiProvisioner,
    config: &mut Config,
    profile: &Profile,
    answers: Vec<Answer>,
) -> Transcript {
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
    let (resp_tx, resp_rx) = tokio::sync::mpsc::channel(64);

    for a in answers {
        let r = match a {
            Answer::Text(t) => ProvisionResponse::Text(t.to_string()),
            Answer::Pick(i) => ProvisionResponse::Selection(vec![i]),
        };
        resp_tx.send(r).await.expect("queue answer");
    }
    drop(resp_tx);

    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(ev) = events_rx.recv().await {
            seen.push(ev);
        }
        seen
    });

    let outcome = prov
        .run(
            config,
            profile,
            ProvisionIo {
                events: events_tx,
                responses: resp_rx,
            },
        )
        .await;

    let events = collector.await.expect("event collector");
    Transcript { outcome, events }
}

/// A profile rooted in a temp dir, for tests that must not touch a real one.
pub fn scratch_profile(root: &std::path::Path) -> Profile {
    Profile {
        name: "provisioning-test".to_string(),
        root: root.to_path_buf(),
    }
}
