use crate::cron::Schedule;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, LocalResult, NaiveDate, TimeZone, Utc};
use cron::Schedule as CronExprSchedule;
use std::str::FromStr;

pub fn next_run_for_schedule(schedule: &Schedule, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    match schedule {
        Schedule::Cron { expr, tz } => {
            let normalized = normalize_expression(expr)?;
            let cron = CronExprSchedule::from_str(&normalized)
                .with_context(|| format!("Invalid cron expression: {expr}"))?;

            if let Some(tz_name) = tz {
                let timezone = chrono_tz::Tz::from_str(tz_name)
                    .with_context(|| format!("Invalid IANA timezone: {tz_name}"))?;
                let localized_from = from.with_timezone(&timezone);
                let next_local = cron.after(&localized_from).next().ok_or_else(|| {
                    anyhow::anyhow!("No future occurrence for expression: {expr}")
                })?;
                Ok(next_local.with_timezone(&Utc))
            } else {
                cron.after(&from)
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("No future occurrence for expression: {expr}"))
            }
        }
        Schedule::At { at } => Ok(*at),
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            let ms = i64::try_from(*every_ms).context("every_ms is too large")?;
            let delta = ChronoDuration::milliseconds(ms);
            from.checked_add_signed(delta)
                .ok_or_else(|| anyhow::anyhow!("every_ms overflowed DateTime"))
        }
    }
}

/// Scan up to ~400 days of upcoming occurrences for a tz-qualified cron schedule
/// and return the local dates whose scheduled wall-clock time does not exist
/// because of a DST spring-forward gap. Coarse schedules (daily/weekly) are fully
/// covered; a very-high-frequency schedule is bounded by the iteration cap and
/// may not reach the DST date (a single skipped instance there is immaterial).
fn dst_skipped_dates(expr: &str, tz_name: &str, from: DateTime<Utc>) -> Result<Vec<NaiveDate>> {
    const MAX_PROBE_DAYS: i64 = 400;
    const MAX_PROBE_ITERS: usize = 5_000;

    let normalized = normalize_expression(expr)?;
    let cron = CronExprSchedule::from_str(&normalized)
        .with_context(|| format!("Invalid cron expression: {expr}"))?;
    let tz = chrono_tz::Tz::from_str(tz_name)
        .with_context(|| format!("Invalid IANA timezone: {tz_name}"))?;

    let horizon = from + ChronoDuration::days(MAX_PROBE_DAYS);
    let mut skipped: Vec<NaiveDate> = Vec::new();

    // Enumerate in the UTC frame: each occurrence's naive Y-M-D H:M:S equals the
    // intended local wall-clock fields (02:00 stays 02:00 regardless of frame).
    for occ in cron.after(&from).take(MAX_PROBE_ITERS) {
        if occ > horizon {
            break;
        }
        let naive = occ.naive_utc();
        if let LocalResult::None = tz.from_local_datetime(&naive) {
            let date = naive.date();
            if skipped.last() != Some(&date) {
                skipped.push(date);
            }
        }
    }
    Ok(skipped)
}

pub fn validate_schedule(schedule: &Schedule, now: DateTime<Utc>) -> Result<()> {
    match schedule {
        Schedule::Cron { expr, tz } => {
            let _ = normalize_expression(expr)?;
            let _ = next_run_for_schedule(schedule, now)?;
            if let Some(tz_name) = tz {
                match dst_skipped_dates(expr, tz_name, now) {
                    Ok(dates) => {
                        for date in dates {
                            tracing::warn!(
                                target: "cron",
                                expr = %expr,
                                tz = %tz_name,
                                date = %date,
                                "cron schedule falls on a nonexistent local time (DST spring-forward); it will be skipped that day"
                            );
                        }
                    }
                    // Detection is best-effort; never fail validation because the
                    // probe errored (the schedule itself already validated above).
                    Err(e) => tracing::debug!(target: "cron", error = %e, "DST probe failed"),
                }
            }
            Ok(())
        }
        Schedule::At { at } => {
            if *at <= now {
                anyhow::bail!("Invalid schedule: 'at' must be in the future");
            }
            Ok(())
        }
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            Ok(())
        }
    }
}

pub fn schedule_cron_expression(schedule: &Schedule) -> Option<String> {
    match schedule {
        Schedule::Cron { expr, .. } => Some(expr.clone()),
        _ => None,
    }
}

/// Translate a single weekday token from crontab numbering (Sunday=0..Saturday=6,
/// with 7 also = Sunday) to the `cron` crate's Quartz numbering (Sunday=1..
/// Saturday=7). Non-numeric tokens (`*`, day names like `mon`) and out-of-range
/// numbers pass through unchanged so the crate applies its own parsing — the
/// crate already maps names to the same Quartz ordinals, so `mon` is the real
/// Monday without remapping.
fn remap_weekday_token(token: &str) -> String {
    match token.trim().parse::<u8>() {
        Ok(n @ 0..=7) => {
            let crate_ordinal = if n == 7 { 1 } else { n + 1 };
            crate_ordinal.to_string()
        }
        _ => token.to_string(),
    }
}

/// Remap one comma-separated element of the weekday field, preserving an
/// optional `/step` suffix and remapping both endpoints of a `lo-hi` range.
fn remap_weekday_element(element: &str) -> String {
    let (base, step) = match element.split_once('/') {
        Some((b, s)) => (b, Some(s)),
        None => (element, None),
    };
    let remapped_base = if let Some((lo, hi)) = base.split_once('-') {
        format!("{}-{}", remap_weekday_token(lo), remap_weekday_token(hi))
    } else {
        remap_weekday_token(base)
    };
    match step {
        Some(s) => format!("{remapped_base}/{s}"),
        None => remapped_base,
    }
}

/// Remap the weekday field (lists, ranges, steps, names) from crontab to crate
/// numbering. `*` and day names are left untouched.
fn remap_weekday_field(field: &str) -> String {
    field
        .split(',')
        .map(remap_weekday_element)
        .collect::<Vec<_>>()
        .join(",")
}

pub fn normalize_expression(expression: &str) -> Result<String> {
    let expression = expression.trim();
    let fields: Vec<&str> = expression.split_whitespace().collect();

    match fields.len() {
        // standard crontab syntax: minute hour day month weekday. The `cron`
        // crate numbers weekdays the Quartz way (Sunday=1..Saturday=7) while
        // crontab uses Sunday=0..Saturday=6 (7 also = Sunday), so the weekday
        // field is remapped before the seconds field is prepended.
        5 => {
            let weekday = remap_weekday_field(fields[4]);
            let mut normalized = fields.clone();
            normalized[4] = weekday.as_str();
            Ok(format!("0 {}", normalized.join(" ")))
        }
        // crate-native syntax includes seconds (+ optional year)
        6 | 7 => Ok(expression.to_string()),
        _ => anyhow::bail!(
            "Invalid cron expression: {expression} (expected 5, 6, or 7 fields, got {})",
            fields.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn next_run_for_schedule_supports_every_and_at() {
        let now = Utc::now();
        let every = Schedule::Every { every_ms: 60_000 };
        let next = next_run_for_schedule(&every, now).unwrap();
        assert!(next > now);

        let at = now + ChronoDuration::minutes(10);
        let at_schedule = Schedule::At { at };
        let next_at = next_run_for_schedule(&at_schedule, now).unwrap();
        assert_eq!(next_at, at);
    }

    #[test]
    fn next_run_for_schedule_supports_timezone() {
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };

        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 16, 17, 0, 0).unwrap());
    }

    #[test]
    fn tz_schedule_skips_nonexistent_local_time_on_spring_forward() {
        // America/New_York springs forward 2026-03-08: 02:00 local does not
        // exist. A `0 2 * * *` job must therefore skip 03-08 and next fire on
        // 03-09. This documents the crate's skip behavior (the reason for the
        // warning added by this plan); it holds before and after the fix.
        let from = Utc.with_ymd_and_hms(2026, 3, 7, 12, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 2 * * *".into(),
            tz: Some("America/New_York".into()),
        };
        let next = next_run_for_schedule(&schedule, from).unwrap();
        let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let next_local = next.with_timezone(&ny);
        assert_eq!(
            next_local.date_naive(),
            chrono::NaiveDate::from_ymd_opt(2026, 3, 9).unwrap(),
            "spring-forward day 2026-03-08 must be skipped; got {next_local}"
        );
    }

    #[test]
    fn dst_skipped_dates_flags_spring_forward_and_ignores_safe_times() {
        let from = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();

        let skipped = dst_skipped_dates("0 2 * * *", "America/New_York", from).unwrap();
        assert!(
            skipped.contains(&chrono::NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()),
            "expected 2026-03-08 to be flagged, got {skipped:?}"
        );

        // Noon always exists → no gap.
        let safe = dst_skipped_dates("0 12 * * *", "America/New_York", from).unwrap();
        assert!(
            !safe.contains(&chrono::NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()),
            "noon must never be flagged as a DST gap, got {safe:?}"
        );
    }

    // Weekday numbering: crontab uses Sunday=0..Saturday=6 (7 also = Sunday),
    // the `cron` crate uses Quartz Sunday=1..Saturday=7. `normalize_expression`
    // remaps the 5-field weekday so a crontab expression means what the operator
    // expects. These tests pin that (they fail if the remap is removed).

    #[test]
    fn crontab_weekday_one_is_monday() {
        use chrono::Datelike;
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * 1".into(),
            tz: None,
        };
        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Mon, "got {next}");
    }

    #[test]
    fn crontab_weekday_zero_is_sunday_and_not_rejected() {
        use chrono::Datelike;
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 4 * * 0".into(),
            tz: None,
        };
        // Used to be rejected (0 is below the crate's inclusive minimum of 1).
        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Sun, "got {next}");
    }

    #[test]
    fn crontab_weekday_seven_is_sunday() {
        use chrono::Datelike;
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 0 * * 7".into(),
            tz: None,
        };
        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Sun, "got {next}");
    }

    #[test]
    fn crontab_weekday_range_one_to_five_is_weekdays() {
        use chrono::Datelike;
        let schedule = Schedule::Cron {
            expr: "0 9 * * 1-5".into(),
            tz: None,
        };
        let mut cursor = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        for _ in 0..7 {
            let next = next_run_for_schedule(&schedule, cursor).unwrap();
            let wd = next.weekday();
            assert!(
                matches!(
                    wd,
                    chrono::Weekday::Mon
                        | chrono::Weekday::Tue
                        | chrono::Weekday::Wed
                        | chrono::Weekday::Thu
                        | chrono::Weekday::Fri
                ),
                "1-5 must be Mon-Fri, got {wd} at {next}"
            );
            cursor = next;
        }
    }

    #[test]
    fn crontab_weekday_name_is_monday() {
        use chrono::Datelike;
        // The crate maps day names to Quartz ordinals already, so `mon` needs no
        // remap. If this fails, the name-passthrough assumption is wrong.
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * mon".into(),
            tz: None,
        };
        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next.weekday(), chrono::Weekday::Mon, "got {next}");
    }
}
