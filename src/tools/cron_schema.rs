//! JSON-Schema fragments for the cron tools' object parameters, and the parsing
//! that goes with them.
//!
//! A tool parameter's schema is the only contract a model can actually read. A
//! shape described in a prose `description` is not a contract: a provider doing
//! constrained or structured decoding has nothing to constrain against, so a
//! model emitting `"600000"` for `every_ms` is guessing in the absence of a
//! type, not ignoring one.
//!
//! These fragments are derived from the types in `crate::cron::types` and must
//! change with them — `cron_add`'s
//! `the_advertised_schema_types_every_ms_as_an_integer` asserts on the emitted
//! schema so the two cannot silently separate.

use crate::cron::Schedule;
use serde_json::{json, Value};

/// One example of each schedule shape, used in the refusal message so a failed
/// attempt can be corrected without a human.
const SCHEDULE_EXAMPLES: &str = r#"{"kind": "cron", "expr": "*/5 * * * *", "tz": "Asia/Jakarta"} | {"kind": "at", "at": "2026-01-31T09:00:00Z"} | {"kind": "every", "every_ms": 600000}"#;

/// Schema for `crate::cron::Schedule` — an internally tagged enum, so `kind` is
/// a property of each branch rather than a sibling of the union.
pub(crate) fn schedule_schema() -> Value {
    json!({
        "description": "When the job runs. Exactly one of the three shapes.",
        "oneOf": [
            {
                "type": "object",
                "title": "cron",
                "properties": {
                    "kind": { "type": "string", "const": "cron" },
                    "expr": {
                        "type": "string",
                        "description": "5-field cron expression, e.g. `*/5 * * * *`."
                    },
                    "tz": {
                        "type": "string",
                        "description": "IANA timezone, e.g. `Asia/Jakarta`. Defaults to UTC."
                    }
                },
                "required": ["kind", "expr"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "title": "at",
                "properties": {
                    "kind": { "type": "string", "const": "at" },
                    "at": {
                        "type": "string",
                        "format": "date-time",
                        "description": "RFC 3339 instant, e.g. `2026-01-31T09:00:00Z`."
                    }
                },
                "required": ["kind", "at"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "title": "every",
                "properties": {
                    "kind": { "type": "string", "const": "every" },
                    "every_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Interval in MILLISECONDS. Ten minutes is 600000."
                    }
                },
                "required": ["kind", "every_ms"],
                "additionalProperties": false
            }
        ]
    })
}

/// Schema for `crate::cron::DeliveryConfig`. `announce` is the only mode that
/// pushes anything; `none` (the default) records the run and stops there.
pub(crate) fn delivery_schema() -> Value {
    json!({
        "type": "object",
        "description": "Where the job's output goes. Omit to record it in run history only.",
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["announce", "none"],
                "description": "`announce` sends the output to `channel`/`to`; `none` records it only."
            },
            "channel": {
                "type": "string",
                "description": "Configured channel name, e.g. `telegram`. Required when mode is `announce`."
            },
            "to": {
                "type": "string",
                "description": "Address within that channel (chat id, room, address). Required when mode is `announce`."
            },
            "best_effort": {
                "type": "boolean",
                "description": "When true (default) a delivery failure is logged, not recorded as a job failure."
            }
        },
        "additionalProperties": false
    })
}

/// Schema for `crate::cron::CronJobPatch` — every field optional, each one
/// replacing that part of the job.
pub(crate) fn patch_schema() -> Value {
    json!({
        "type": "object",
        "description": "Fields to change. Omitted fields keep their current value.",
        "properties": {
            "schedule": schedule_schema(),
            "command": { "type": "string", "description": "Shell job: the command to run." },
            "prompt": { "type": "string", "description": "Agent job: the prompt to run." },
            "name": { "type": "string" },
            "enabled": { "type": "boolean" },
            "delivery": delivery_schema(),
            "model": { "type": "string" },
            "session_target": { "type": "string", "enum": ["isolated", "main"] },
            "delete_after_run": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

/// Accept `"600000"` where `600000` was meant.
///
/// Deliberate, documented tolerance at the one boundary a model writes to
/// (CLAUDE.md §3.5 allows a fallback that is intentional and safe, and requires
/// it to be documented). Models stringify integers regardless of what the schema
/// says; the schema above tells them the right thing, and this catches the ones
/// that do it anyway. It is confined to this parameter on purpose — a
/// crate-wide argument-normalisation layer is a separate design decision.
///
/// Only a string that parses as a whole positive integer is coerced. Anything
/// else is left exactly as it arrived so it still fails, with the message below.
fn coerce_every_ms(schedule: &mut Value) {
    let Some(obj) = schedule.as_object_mut() else {
        return;
    };
    let Some(raw) = obj.get("every_ms").and_then(Value::as_str) else {
        return;
    };
    if let Ok(parsed) = raw.trim().parse::<u64>() {
        obj.insert("every_ms".to_string(), json!(parsed));
    }
}

/// Parse a `schedule` argument into a [`Schedule`], with a refusal a model can
/// act on.
///
/// Serde's raw message ("invalid type: string …") names neither the field nor
/// what to send instead, which leaves the caller no way to correct itself.
pub(crate) fn parse_schedule(raw: &Value) -> Result<Schedule, String> {
    let mut value = raw.clone();
    coerce_every_ms(&mut value);

    let schedule = serde_json::from_value::<Schedule>(value).map_err(|e| {
        format!(
            "Invalid schedule ({e}). Expected one of: {SCHEDULE_EXAMPLES}. \
             `every_ms` is an integer number of milliseconds (ten minutes = 600000)."
        )
    })?;

    // A zero interval is refused by `crate::cron::schedule` (which owns schedule
    // validation and says "every_ms must be > 0"), so it is not re-checked here.
    // The schema advertises `minimum: 1` to keep a model from sending it at all.

    Ok(schedule)
}

/// The two internal origin properties every cron_* tool schema carries. The
/// agent loop overwrites both on every chat turn, so a model cannot forge an
/// origin; on the TUI / CLI / web console they are absent and every job is
/// visible.
pub(crate) fn origin_channel_schema() -> Value {
    serde_json::json!({
        "type": "string",
        "description": "internal: set by the runtime, not by the model"
    })
}

pub(crate) fn origin_chat_schema() -> Value {
    serde_json::json!({
        "type": "string",
        "description": "internal: set by the runtime, not by the model"
    })
}

/// The chat a cron_* call came from, when it came from one. `None` on the
/// TUI / CLI / web console, which see and manage every job.
pub(crate) fn origin_filter(args: &Value) -> Option<(String, String, Option<String>)> {
    let channel = args.get("origin_channel").and_then(Value::as_str)?;
    let chat = args.get("origin_chat").and_then(Value::as_str)?;
    if channel.is_empty() || chat.is_empty() {
        return None;
    }
    Some((channel.to_string(), chat.to_string(), None))
}

/// The scope the cron_* tools work under, decided by the turn's memory view.
///
/// Cron tools always serve one — every `cron_add` writes an `origin` for later
/// visibility checks, and every `cron_list` / `cron_runs` / `cron_run` /
/// `cron_update` / `cron_remove` reads the rows it can reach through that
/// origin. The view is the only authority on which place that is:
///
/// * `None` view ⇒ the cron tools refuse with one answer that names no job.
///   A turn with no view has no place to scope to, and a cron tool that
///   accepted it would expose every job, including jobs an owner created
///   from their console.
/// * `Some(All)` ⇒ an unscoped caller (the TUI, the CLI, the web console,
///   and a `delegate` sub-agent that ran with `All`). The args'
///   `origin_channel` / `origin_chat` are honored when both are present;
///   without them the tools see and manage every job, as today.
/// * `Some(Only(place))` ⇒ a turn from one conversation (a guest, an owner
///   in a group, a `delegate` sub-agent inherited from a turn in one
///   conversation). The place is split into `surface:sender[:thread]` and
///   used as the origin; whatever the args say is dropped. A model that sets
///   a foreign `origin_channel` / `origin_chat` to reach another chat's
///   jobs cannot.
///
/// The place is built by
/// `ConversationKey::new(channel, chat).in_thread(thread).resolve()` and parsed
/// back with [`crate::channels::conversation::parse_place`]. The
/// `surface:encoded_sender[:encoded_thread]` shape survives the round trip
/// for plain chats, for senders containing `:` or `%` (Matrix,
/// `100%`-style ids), and for threaded chats (Slack threads, Telegram forum
/// topics the channel packs into reply_target). `parse_place` is the
/// tested inverse of `resolve`; if either side ever drifts the
/// `parse_place_round_trips_through_resolve` test fails before a job ever
/// loses its place.
pub(crate) fn cron_origin_for_view(
    view: Option<&crate::memory::MemoryView>,
    args: &Value,
) -> Result<Option<(String, String, Option<String>)>, String> {
    use crate::memory::MemoryView;
    match view {
        None => Err("Cron tools are unavailable: this turn has no memory view.".to_string()),
        Some(MemoryView::All) => Ok(origin_filter(args)),
        Some(MemoryView::Only(place)) => Ok(Some(parse_view_place(place)?)),
    }
}

/// Parse a `MemoryView::Only(place)` into the three origin columns. The place
/// is what the dispatcher built (`ConversationKey::new(channel, chat).in_thread(thread).resolve()`),
/// so it is always `surface:encoded_sender[:encoded_thread]`; the helper just
/// unpacks it via the same inverse of `resolve` that the per-job view is
/// rebuilt from. Exposed at module scope so the cron_add path can call it
/// with the place already in hand, and tests can pin it directly.
fn parse_view_place(place: &str) -> Result<(String, String, Option<String>), String> {
    let (channel, chat, thread) = crate::channels::conversation::parse_place(place)
        .map_err(|e| format!("Cannot parse view place as cron origin: {e}"))?;
    Ok((channel, chat, thread))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryView;
    use serde_json::json;

    /// A turn with no view has no place to scope to; the helper refuses rather
    /// than expose every job, the same way `memory_forget` refuses without a
    /// view. The error text names no job so a caller cannot probe.
    #[test]
    fn cron_origin_for_view_with_no_view_is_refused() {
        let result = cron_origin_for_view(
            None,
            &json!({"origin_channel": "telegram", "origin_chat": "1"}),
        );
        let err = result.expect_err("a no-view caller must be refused");
        assert!(err.contains("no memory view"), "{err}");
    }

    /// `All` is the unscoped caller: `args` decide. With both
    /// `origin_channel` and `origin_chat` set, the helper passes them through,
    /// with thread = None (the TUI / CLI / web console never sit in a thread).
    #[test]
    fn cron_origin_for_view_with_all_uses_args_origin_when_set() {
        let args = json!({"origin_channel": "telegram", "origin_chat": "1"});
        let got =
            cron_origin_for_view(Some(&MemoryView::All), &args).expect("All view does not refuse");
        assert_eq!(got, Some(("telegram".to_string(), "1".to_string(), None)));
    }

    /// `All` without an `origin` is the TUI / CLI / web console: the helper
    /// returns `Ok(None)` and the tool sees every job.
    #[test]
    fn cron_origin_for_view_with_all_and_no_args_origin_returns_none() {
        let got = cron_origin_for_view(Some(&MemoryView::All), &json!({}))
            .expect("All view does not refuse");
        assert_eq!(got, None);
    }

    /// `All` ignores a `origin` whose channel is empty (the legacy origin-less
    /// shape) the way `origin_filter` already does.
    #[test]
    fn cron_origin_for_view_with_all_drops_empty_origin_channel() {
        let args = json!({"origin_channel": "", "origin_chat": "1"});
        let got =
            cron_origin_for_view(Some(&MemoryView::All), &args).expect("All view does not refuse");
        assert_eq!(got, None);
    }

    /// `Only(place)` ignores whatever the args say — a `delegate` sub-agent
    /// running with the inherited view cannot widen by passing a foreign one.
    /// The place is split via the inverse of `resolve`, which decodes any
    /// `%3A` / `%25` the encoder wrote into the components.
    #[test]
    fn cron_origin_for_view_with_only_ignores_args_and_uses_the_place() {
        let args = json!({"origin_channel": "discord", "origin_chat": "elsewhere"});
        let got = cron_origin_for_view(Some(&MemoryView::Only("telegram:chat-a".into())), &args)
            .expect("Only view does not refuse with a parsable place");
        assert_eq!(
            got,
            Some(("telegram".to_string(), "chat-a".to_string(), None))
        );
    }

    /// A Matrix sender contains `:` (`@localpart:homeserver`), and the encoder
    /// writes `%3A`. The helper decodes the sender, not splits it.
    #[test]
    fn cron_origin_for_view_with_only_splits_an_encoded_sender() {
        let place =
            crate::channels::conversation::ConversationKey::new("matrix", "@bob:example.org")
                .resolve();
        let got = cron_origin_for_view(Some(&MemoryView::Only(place.clone())), &json!({}))
            .expect("Only view does not refuse");
        assert_eq!(
            got,
            Some(("matrix".to_string(), "@bob:example.org".to_string(), None)),
            "place: {place}"
        );
    }

    /// A threaded chat (Slack thread, Telegram forum topic encoded into
    /// reply_target) carries the thread id through. The third `:` in the
    /// place is the thread separator, and the helper must read it.
    #[test]
    fn cron_origin_for_view_with_only_reads_the_thread() {
        let place = crate::channels::conversation::ConversationKey::new("slack", "C0CHAN")
            .in_thread(Some("1700000000.000500"))
            .resolve();
        let got = cron_origin_for_view(Some(&MemoryView::Only(place.clone())), &json!({}))
            .expect("threaded view does not refuse");
        assert_eq!(
            got,
            Some((
                "slack".to_string(),
                "C0CHAN".to_string(),
                Some("1700000000.000500".to_string()),
            )),
            "place: {place}"
        );
    }

    /// A place that has no `:` is malformed: ConversationKey always produces
    /// `surface:rest`, so this only happens on a misconfigured view. Refused
    /// rather than silently widened.
    #[test]
    fn cron_origin_for_view_with_only_refuses_a_place_with_no_separator() {
        let result = cron_origin_for_view(Some(&MemoryView::Only("noplace".into())), &json!({}));
        assert!(result.is_err(), "a malformed place must be refused");
    }
}
