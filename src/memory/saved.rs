//! The notes a turn stored, so the door that ran the turn can say so.
//!
//! `memory_store` is a `Tool`, and a tool result is a `ToolResult` that carries
//! text and a success flag, nothing about what was stored. A door that wants to
//! tell the person "I noted that" would have to read the text back out of the
//! history, which differs between native and prompt-guided tool calls. So the
//! door sets a collector around the turn, and the tool adds to it after a
//! write that succeeded.
//!
//! Modeled on [`super::view::MEMORY_VIEW`]: a `tokio` task-local set by the door
//! around the turn's future. Outside any scope [`record_saved_note`] does
//! nothing, so a door that sets no collector behaves as it did.

use std::sync::{Arc, Mutex};

tokio::task_local! {
    /// The collector of the turn the current task runs. Set by the door around
    /// the turn, and read back by the door after it.
    pub static SAVED_NOTES: SavedNotes;
}

/// The content of every note a turn stored, in the order they were stored.
///
/// A handle: the door keeps one clone to read after the turn, and scopes
/// another around it. A turn that times out drops its future, and the notes
/// stored before then stay readable here.
#[derive(Clone, Default)]
pub struct SavedNotes(Arc<Mutex<Vec<String>>>);

impl SavedNotes {
    /// The notes stored so far.
    #[must_use]
    pub fn all(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Record that the current turn stored `content`. Does nothing outside a
/// [`SAVED_NOTES`] scope.
pub fn record_saved_note(content: &str) {
    let _ = SAVED_NOTES.try_with(|notes| {
        notes
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(content.to_string());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notes_recorded_inside_a_scope_are_read_back_in_order() {
        let notes = SavedNotes::default();
        SAVED_NOTES
            .scope(notes.clone(), async {
                record_saved_note("first");
                record_saved_note("second");
            })
            .await;
        assert_eq!(notes.all(), vec!["first".to_string(), "second".to_string()]);
    }

    #[tokio::test]
    async fn recording_outside_a_scope_does_nothing_and_does_not_panic() {
        assert!(
            SAVED_NOTES.try_with(|_| ()).is_err(),
            "no collector is set outside a scope"
        );
        record_saved_note("nobody is listening");

        // A scope opened afterwards starts empty: nothing was kept for it.
        let notes = SavedNotes::default();
        SAVED_NOTES.scope(notes.clone(), async {}).await;
        assert!(notes.all().is_empty());
    }
}
