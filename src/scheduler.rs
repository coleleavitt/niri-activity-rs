use chrono::{Datelike, Months, NaiveDate};
use rusqlite::{Connection, params};

use crate::config::Config;
use crate::email;
use crate::error::Error;
use crate::report::{App, TimeRange};

fn was_sent(conn: &Connection, period_type: &str, period_key: &str) -> Result<bool, Error> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sent_reports WHERE period_type = ?1 AND period_key = ?2)",
        params![period_type, period_key],
        |row| row.get(0),
    )?;
    Ok(exists)
}

fn mark_sent(conn: &Connection, period_type: &str, period_key: &str) -> Result<(), Error> {
    conn.execute(
        "INSERT OR IGNORE INTO sent_reports (period_type, period_key, sent_at) VALUES (?1, ?2, ?3)",
        params![period_type, period_key, chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// Returns the Saturday that starts the current week (Sat-Fri alignment).
fn current_week_saturday(today: NaiveDate) -> NaiveDate {
    let days_since_saturday = (today.weekday().num_days_from_monday() as i64 + 2) % 7;
    today - chrono::Duration::days(days_since_saturday)
}

/// Completed weekly periods (Sat-Fri) not yet sent, oldest first.
fn pending_weekly_reports(
    conn: &Connection,
    today: NaiveDate,
    max_backfill: u32,
) -> Result<Vec<(NaiveDate, NaiveDate)>, Error> {
    let this_saturday = current_week_saturday(today);

    (1..=max_backfill)
        .rev()
        .map(|i| {
            let start = this_saturday - chrono::Duration::weeks(i64::from(i));
            (start, start + chrono::Duration::days(6))
        })
        .filter(|(_, end)| today > *end)
        .filter_map(|(start, end)| {
            let key = start.format("%Y-%m-%d").to_string();
            match was_sent(conn, "weekly", &key) {
                Ok(true) => None,
                Ok(false) => Some(Ok((start, end))),
                Err(e) => Some(Err(e)),
            }
        })
        .collect()
}

/// Completed monthly periods not yet sent, oldest first.
fn pending_monthly_reports(
    conn: &Connection,
    today: NaiveDate,
    max_backfill: u32,
) -> Result<Vec<(NaiveDate, NaiveDate)>, Error> {
    (1..=max_backfill)
        .rev()
        .filter_map(|i| {
            let month_start = today.with_day(1)?.checked_sub_months(Months::new(i))?;
            let month_end =
                month_start.checked_add_months(Months::new(1))? - chrono::Duration::days(1);
            (today > month_end).then_some((month_start, month_end))
        })
        .filter_map(|(start, end)| {
            let key = start.format("%Y-%m").to_string();
            match was_sent(conn, "monthly", &key) {
                Ok(true) => None,
                Ok(false) => Some(Ok((start, end))),
                Err(e) => Some(Err(e)),
            }
        })
        .collect()
}

/// Dispatch a list of pending reports: send each one and record success.
fn dispatch_pending(
    conn: &Connection,
    pending: Result<Vec<(NaiveDate, NaiveDate)>, Error>,
    period_type: &str,
    key_fmt: &str,
    quiet: bool,
) {
    let reports = match pending {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                "[SCHEDULER] Error checking pending {} reports: {}",
                period_type,
                e
            );
            return;
        }
    };

    for (start, end) in reports {
        let label = format!("{} ({} to {})", capitalize(period_type), start, end);

        if !quiet {
            tracing::info!("[SCHEDULER] Sending {}: {} to {}", period_type, start, end);
        }

        match send_report(start, end, &label) {
            Ok(()) => {
                let key = start.format(key_fmt).to_string();
                if let Err(e) = mark_sent(conn, period_type, &key) {
                    tracing::error!("[SCHEDULER] Failed to record sent status: {}", e);
                }
                if !quiet {
                    tracing::info!("[SCHEDULER] {} sent successfully", label);
                }
            }
            Err(e) => {
                tracing::error!("[SCHEDULER] Failed to send {}: {}", label, e);
            }
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

fn send_report(start: NaiveDate, end: NaiveDate, period_name: &str) -> Result<(), Error> {
    let app = App::open()?;
    email::send_report(&app, TimeRange::DateRange(start, end), period_name)
}

/// Periodically called from the watcher loop. Sends any pending weekly
/// (Sat-Fri) and monthly reports, backfilling missed periods.
pub fn check_scheduled_reports(conn: &Connection, config: &Config, quiet: bool) {
    if !config.email.enabled {
        return;
    }

    let today = config.local_now().date_naive();
    const MAX_WEEKLY: u32 = 8;
    const MAX_MONTHLY: u32 = 3;

    dispatch_pending(
        conn,
        pending_weekly_reports(conn, today, MAX_WEEKLY),
        "weekly",
        "%Y-%m-%d",
        quiet,
    );

    dispatch_pending(
        conn,
        pending_monthly_reports(conn, today, MAX_MONTHLY),
        "monthly",
        "%Y-%m",
        quiet,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturday_alignment() {
        let cases: &[(NaiveDate, NaiveDate)] = &[
            // (input, expected saturday)
            (
                NaiveDate::from_ymd_opt(2026, 4, 5).unwrap(), // Sunday
                NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
            ),
            (
                NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(), // Saturday
                NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
            ),
            (
                NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(), // Friday
                NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
            ),
            (
                NaiveDate::from_ymd_opt(2026, 4, 6).unwrap(), // Monday
                NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
            ),
            (
                NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(), // Wednesday
                NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
            ),
            (
                NaiveDate::from_ymd_opt(2026, 4, 11).unwrap(), // Next Saturday
                NaiveDate::from_ymd_opt(2026, 4, 11).unwrap(),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                current_week_saturday(*input),
                *expected,
                "failed for input {}",
                input,
            );
        }
    }

    #[test]
    fn month_boundary_subtraction() {
        let jan = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let dec = jan
            .with_day(1)
            .and_then(|d| d.checked_sub_months(Months::new(1)));
        assert_eq!(dec, NaiveDate::from_ymd_opt(2025, 12, 1));

        let nov = jan
            .with_day(1)
            .and_then(|d| d.checked_sub_months(Months::new(2)));
        assert_eq!(nov, NaiveDate::from_ymd_opt(2025, 11, 1));

        let mar = NaiveDate::from_ymd_opt(2026, 3, 10).unwrap();
        let feb = mar
            .with_day(1)
            .and_then(|d| d.checked_sub_months(Months::new(1)));
        assert_eq!(feb, NaiveDate::from_ymd_opt(2026, 2, 1));
    }
}
