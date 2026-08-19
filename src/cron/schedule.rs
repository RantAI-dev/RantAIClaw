use crate::cron::Schedule;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
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

pub fn validate_schedule(schedule: &Schedule, now: DateTime<Utc>) -> Result<()> {
    match schedule {
        Schedule::Cron { expr, .. } => {
            let _ = normalize_expression(expr)?;
            let _ = next_run_for_schedule(schedule, now)?;
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
