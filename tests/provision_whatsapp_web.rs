//! Integration test for the WhatsApp Web pairing stream.
//!
//! The `whatsapp_web` module is feature-gated, so the whole file is cfg'd —
//! `#[ignore]` only skips execution, not compilation, so an unconditional
//! `use` would break `cargo test --tests` on a build without the feature.

#![cfg(feature = "whatsapp-web")]

use futures::StreamExt;
use rantaiclaw::channels::whatsapp_web::{pair_once, PairEvent, PairOptions};

/// The pairing stream must always end, and end in a state the caller can act on.
///
/// This used to be `#[ignore]`d for "requires whatsapp-web feature" — a reason
/// that has not held for some time: the feature is in the default set and the
/// `#![cfg]` above already handles its absence. Worse, the body asserted
/// nothing (`let _ = saw_qr;`), so even when run it could not fail.
///
/// What is assertable without depending on WhatsApp being reachable is the
/// *shape*: within its own budget the stream yields at least one event, ends,
/// and never reports `Connected` — nobody scanned a code in a test. Offline
/// that path is `Failed`/`Timeout`, online it is `Qr`; both are terminal, and
/// a hang — the failure mode that would otherwise stall CI for minutes — is
/// caught by the outer timeout rather than being waited out.
#[tokio::test]
async fn pair_once_always_terminates_without_connecting() {
    let budget = std::time::Duration::from_secs(2);

    let collected = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut stream = pair_once(PairOptions {
            session_path: tempfile::tempdir().unwrap().path().join("wa.db"),
            pair_phone: None,
            timeout: budget,
        });
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let terminal = matches!(
                ev,
                PairEvent::Qr(_) | PairEvent::Timeout | PairEvent::Failed(_)
            );
            events.push(ev);
            if terminal {
                break;
            }
        }
        events
    })
    .await
    .expect("the pairing stream must respect its own timeout, not hang");

    assert!(
        !collected.is_empty(),
        "the stream ended without saying anything — the caller has nothing to show the operator"
    );
    assert!(
        !collected.iter().any(|e| matches!(e, PairEvent::Connected)),
        "nothing scanned a code, so Connected cannot be reachable here: {collected:?}"
    );
}
