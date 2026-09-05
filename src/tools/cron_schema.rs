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
