# Meeting Notes

## Description
Capture, organize, and follow up on meeting notes. Use this skill when the
user pastes a transcript, shares a recording link, or wants to plan or
debrief a meeting. Produces a tight, action-oriented record that's easy to
share.

## Tools
- name: file_read
  kind: builtin
- name: memory_write
  kind: builtin
- name: cron_schedule
  kind: builtin

## Instructions
- Output four sections in this order: `Attendees`, `Decisions`, `Action
  Items`, `Open Questions`.
- Each action item must have an owner and a due date. If either is missing
  from the source, mark it `OWNER: TBD` or `DUE: TBD` — do not invent.
- Keep decisions terse: one sentence each, in the past tense.
- Open questions are things the meeting did not resolve — list them so the
  user can chase follow-ups.
- Save the rendered notes to `memory/meetings/<YYYY-MM-DD>-<slug>.md`.
- Offer to schedule reminders for any action item that has a due date,
  using the scheduler-reminders skill.
- Never paraphrase quotes or numbers. Quote them verbatim with attribution.
