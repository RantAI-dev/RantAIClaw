# Scheduler & Reminders

## Description
Cron-driven reminders, time-aware scheduling, and recurring nudges. Use this
skill when the user asks to be reminded later, set up a recurring task, or
schedule something at a specific time. Time-zone aware: always interpret
times in the user's configured timezone unless they explicitly say otherwise.

## Tools
- name: cron_schedule
  kind: builtin
- name: cron_list
  kind: builtin
- name: cron_cancel
  kind: builtin
- name: memory_recall
  kind: builtin

## Instructions
- Confirm the resolved absolute time before scheduling (e.g. "I'll remind you
  on Monday 2026-05-04 at 09:00 Asia/Jakarta — confirm?").
- Use the user's timezone from `project_context.timezone`. If unset, ask.
- For recurring schedules, prefer human-readable descriptions ("every weekday
  at 9am") and translate to cron only when committing.
- Capture the reminder text verbatim from the user; do not paraphrase.
- After scheduling, give the user the cron handle so they can cancel later.
- For "in N minutes/hours" relative reminders, compute the absolute time once
  at scheduling and quote it back.
