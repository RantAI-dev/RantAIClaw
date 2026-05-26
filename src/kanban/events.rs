/// Kanban event kinds. Order them as: lifecycle, edits, worker telemetry. The
/// CLI `watch` command and the gateway notifier both filter on these strings,
/// so changing one is a behaviour change for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    // Lifecycle
    Created,
    Promoted,
    Claimed,
    Completed,
    Blocked,
    Unblocked,
    Archived,
    // Edits
    Assigned,
    Edited,
    Reprioritized,
    Status,
    // Worker telemetry
    Spawned,
    Heartbeat,
    Reclaimed,
    ClaimExtended,
    ClaimRejected,
    Crashed,
    TimedOut,
    SpawnFailed,
    GaveUp,
    CompletionBlockedHallucination,
    SuspectedHallucinatedReferences,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Created => "created",
            EventKind::Promoted => "promoted",
            EventKind::Claimed => "claimed",
            EventKind::Completed => "completed",
            EventKind::Blocked => "blocked",
            EventKind::Unblocked => "unblocked",
            EventKind::Archived => "archived",
            EventKind::Assigned => "assigned",
            EventKind::Edited => "edited",
            EventKind::Reprioritized => "reprioritized",
            EventKind::Status => "status",
            EventKind::Spawned => "spawned",
            EventKind::Heartbeat => "heartbeat",
            EventKind::Reclaimed => "reclaimed",
            EventKind::ClaimExtended => "claim_extended",
            EventKind::ClaimRejected => "claim_rejected",
            EventKind::Crashed => "crashed",
            EventKind::TimedOut => "timed_out",
            EventKind::SpawnFailed => "spawn_failed",
            EventKind::GaveUp => "gave_up",
            EventKind::CompletionBlockedHallucination => "completion_blocked_hallucination",
            EventKind::SuspectedHallucinatedReferences => "suspected_hallucinated_references",
        }
    }
}

/// Terminal event kinds — what the gateway notifier delivers back to the
/// originating chat. Same shape as Hermes.
pub const TASK_TERMINAL_EVENT_KINDS: &[&str] =
    &["completed", "blocked", "gave_up", "crashed", "timed_out"];
