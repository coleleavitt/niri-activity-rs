//! Enhanced Excel export with conditional formatting, sparklines, and
//! multi-sheet output.

#[cfg(feature = "excel-extra-sheets")]
use std::collections::HashMap;
use std::str::FromStr;

#[cfg(not(feature = "excel-extra-sheets"))]
use chrono::NaiveDate;
#[cfg(feature = "excel-extra-sheets")]
use chrono::{Datelike, NaiveDate};
use rusqlite::params;
#[cfg(feature = "excel-extra-sheets")]
use rust_xlsxwriter::{
    Color, ConditionalFormat3ColorScale, Format, Sparkline, SparklineType, Workbook,
};
#[cfg(not(feature = "excel-extra-sheets"))]
use rust_xlsxwriter::{Color, ConditionalFormat3ColorScale, Format, Workbook};

use super::types::Metrics;
use super::{App, TimeBounds, TimeRange};
use crate::config::Category;
use crate::error::Error;
use crate::fmt::fmt_hms;

// Time constants (milliseconds)
const MS_PER_HOUR: u64 = 3_600_000;
const MS_PER_MIN: u64 = 60_000;
const MS_PER_MIN_I64: i64 = 60_000;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert xlsx errors to our crate error type.
fn xlsx_err(e: impl std::fmt::Display) -> Error {
    Error::NiriError(e.to_string())
}

/// Format a millisecond delta as `+HH:MM` / `-HH:MM`.
fn fmt_delta_hms(delta_ms: i64) -> String {
    let sign = if delta_ms >= 0 { "+" } else { "-" };
    let abs = delta_ms.unsigned_abs();
    let h = abs / MS_PER_HOUR;
    let m = (abs % MS_PER_HOUR) / MS_PER_MIN;
    format!("{sign}{h}:{m:02}")
}

#[cfg(feature = "excel-extra-sheets")]
/// Row from a top-apps query.
struct TopApp {
    app_id: String,
    category: Category,
    total_ms: i64,
}

#[cfg(feature = "excel-extra-sheets")]
/// Query the top N apps by total time in a UTC window.
fn query_top_apps(
    conn: &rusqlite::Connection,
    since_utc: &str,
    until_utc: &str,
    limit: u32,
) -> Result<Vec<TopApp>, Error> {
    let mut stmt = conn.prepare(
        "SELECT app_id, category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) AS total
         FROM events
         WHERE timestamp >= ?1 AND timestamp < ?2
         GROUP BY app_id
         ORDER BY total DESC
         LIMIT ?3",
    )?;

    let rows = stmt.query_map(params![since_utc, until_utc, limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;

    let mut apps = Vec::new();
    for r in rows {
        let (app_id, cat_raw, total_ms) = r?;
        let category = Category::from_str(&cat_raw).unwrap_or(Category::Neutral);
        apps.push(TopApp {
            app_id,
            category,
            total_ms,
        });
    }
    Ok(apps)
}

#[cfg(feature = "excel-extra-sheets")]
// ---------------------------------------------------------------------------
// Weekly aggregation helper
// ---------------------------------------------------------------------------
struct WeekBucket {
    iso_year: i32,
    iso_week: u32,
    start_date: NaiveDate,
    end_date: NaiveDate,
    total_ms: i64,
    productive_ms: i64,
    unproductive_ms: i64,
    neutral_ms: i64,
    day_count: u32,
}

#[cfg(feature = "excel-extra-sheets")]
fn aggregate_weeks(daily: &[(NaiveDate, Metrics, bool)]) -> Vec<WeekBucket> {
    let mut map: HashMap<(i32, u32), WeekBucket> = HashMap::new();

    for (date, m, _is_workday) in daily {
        let iso = date.iso_week();
        let key = (iso.year(), iso.week());

        let bucket = map.entry(key).or_insert_with(|| WeekBucket {
            iso_year: iso.year(),
            iso_week: iso.week(),
            start_date: *date,
            end_date: *date,
            total_ms: 0,
            productive_ms: 0,
            unproductive_ms: 0,
            neutral_ms: 0,
            day_count: 0,
        });

        if *date < bucket.start_date {
            bucket.start_date = *date;
        }
        if *date > bucket.end_date {
            bucket.end_date = *date;
        }
        bucket.total_ms = bucket.total_ms.saturating_add(m.total_ms);
        bucket.productive_ms = bucket.productive_ms.saturating_add(m.productive_ms);
        bucket.unproductive_ms = bucket.unproductive_ms.saturating_add(m.unproductive_ms);
        bucket.neutral_ms = bucket.neutral_ms.saturating_add(m.neutral_ms);
        bucket.day_count = bucket.day_count.saturating_add(1);
    }

    let mut weeks: Vec<WeekBucket> = map.into_values().collect();
    weeks.sort_by_key(|w| (w.iso_year, w.iso_week));
    weeks
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Export an enhanced multi-sheet Excel workbook for the given time range.
/// Export activity data as Excel workbook with formatting and conditional
/// colors.
pub fn export_xlsx_range(app: &App, range: TimeRange, path: &str) -> Result<(), Error> {
    let bounds: TimeBounds = range.resolve(&app.config)?;
    let since_local = bounds.start_date;
    let today_local = bounds.end_date;

    let mut workbook = Workbook::new();

    // ── Shared formats ──────────────────────────────────────────────────
    let header_fmt = Format::new().set_bold();
    #[cfg(feature = "excel-extra-sheets")]
    let pct_fmt = Format::new().set_num_format("0.0%");
    let total_fmt = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xE0E0E0));
    let total_pct_fmt = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xE0E0E0))
        .set_num_format("0.0%");
    let weekend_fmt = Format::new().set_background_color(Color::RGB(0xF0F0F0));
    let weekend_pct_fmt = Format::new()
        .set_background_color(Color::RGB(0xF0F0F0))
        .set_num_format("0.0%");
    let workday_fmt = Format::new();
    let workday_pct_fmt = Format::new().set_num_format("0.0%");

    // Delta column formats (colored based on positive/negative)
    let delta_positive_fmt = Format::new().set_font_color(Color::RGB(0x006400)); // dark green
    let delta_negative_fmt = Format::new().set_font_color(Color::RGB(0xCC0000)); // dark red
    let _delta_neutral_fmt = Format::new().set_font_color(Color::RGB(0x666666)); // gray for zero
    let weekend_delta_positive_fmt = Format::new()
        .set_background_color(Color::RGB(0xF0F0F0))
        .set_font_color(Color::RGB(0x006400));
    let weekend_delta_negative_fmt = Format::new()
        .set_background_color(Color::RGB(0xF0F0F0))
        .set_font_color(Color::RGB(0xCC0000));
    let _weekend_delta_neutral_fmt = Format::new()
        .set_background_color(Color::RGB(0xF0F0F0))
        .set_font_color(Color::RGB(0x666666));

    let workdays = app.config.schedule.count_workdays(since_local, today_local);
    let days_header = format!("Days ({})", workdays);
    // ── Column headers (Daily Summary) ──────────────────────────────────
    let headers = [
        "Date",
        "Screen Time",
        "Productive",
        "Unproductive",
        "Undefined",
        "Prod Active",
        "Prod Passive",
        "Productive Ratio",
        "Productive Active %",
        "Avg Workday",
        "Avg Workday Prod",
        &days_header,
        "Daily \u{0394}",
    ];

    let daily_sheet = workbook.add_worksheet();
    daily_sheet.set_name("Daily Summary").map_err(xlsx_err)?;

    for (col, header) in headers.iter().enumerate() {
        daily_sheet
            .write_string_with_format(0, col as u16, *header, &header_fmt)
            .map_err(xlsx_err)?;
    }

    // ── Collect daily metrics ───────────────────────────────────────────
    let mut row: u32 = 1;
    let mut date = since_local;
    let mut daily_metrics: Vec<(NaiveDate, Metrics, bool)> = Vec::new();

    while date <= today_local {
        let day_start = super::day_start_utc(&app.config, date)?;
        let day_end = super::day_end_utc(&app.config, date)?;

        let mut stmt = app.conn.prepare(
            "SELECT category, SUM(active_ms), COALESCE(SUM(passive_ms),0), SUM(idle_ms)
             FROM events
             WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY category",
        )?;

        let mut m = Metrics::default();

        let rows_iter = stmt.query_map(params![&day_start, &day_end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;

        for r in rows_iter {
            let (category_raw, active_ms, passive_ms, idle_ms) = r?;
            let total = active_ms.saturating_add(passive_ms);
            m.total_ms = m.total_ms.saturating_add(total);

            match Category::from_str(&category_raw).unwrap_or(Category::Neutral) {
                Category::Productive => {
                    m.productive_ms = m.productive_ms.saturating_add(total);
                    m.productive_active_ms = m.productive_active_ms.saturating_add(active_ms);
                    m.productive_passive_ms = m.productive_passive_ms.saturating_add(passive_ms);
                    m.productive_idle_ms = m.productive_idle_ms.saturating_add(idle_ms);
                }
                Category::Unproductive => {
                    m.unproductive_ms = m.unproductive_ms.saturating_add(total);
                }
                Category::Neutral => {
                    m.neutral_ms = m.neutral_ms.saturating_add(total);
                }
            }
        }

        let is_workday = app.config.schedule.is_workday(date);

        daily_metrics.push((date, m, is_workday));
        date += chrono::Duration::days(1);
    }

    // ── Write daily rows (with deltas + rolling avg) ────────────────────
    for (idx, (date, m, is_workday)) in daily_metrics.iter().enumerate() {
        let prod_ratio = if m.total_ms > 0 {
            m.productive_ms as f64 / m.total_ms as f64
        } else {
            0.0
        };
        let prod_active_pct = if m.productive_ms > 0 {
            m.productive_active_ms as f64 / m.productive_ms as f64
        } else {
            0.0
        };

        // Delta from previous day (for Daily Δ column)
        let delta_productive: i64 = if idx > 0 {
            let prev = &daily_metrics[idx.saturating_sub(1)].1;
            m.productive_ms - prev.productive_ms
        } else {
            0
        };

        // Choose format based on weekend/holiday
        let str_fmt = if *is_workday {
            &workday_fmt
        } else {
            &weekend_fmt
        };
        let pct_fmt = if *is_workday {
            &workday_pct_fmt
        } else {
            &weekend_pct_fmt
        };

        // Date (col 0)
        daily_sheet
            .write_string_with_format(row, 0, date.to_string(), str_fmt)
            .map_err(xlsx_err)?;

        // Screen Time (col 1)
        daily_sheet
            .write_string_with_format(row, 1, fmt_hms(m.total_ms), str_fmt)
            .map_err(xlsx_err)?;

        // Productive (col 2)
        daily_sheet
            .write_string_with_format(row, 2, fmt_hms(m.productive_ms), str_fmt)
            .map_err(xlsx_err)?;

        // Unproductive (col 3)
        daily_sheet
            .write_string_with_format(row, 3, fmt_hms(m.unproductive_ms), str_fmt)
            .map_err(xlsx_err)?;

        // Undefined (col 4)
        daily_sheet
            .write_string_with_format(row, 4, fmt_hms(m.neutral_ms), str_fmt)
            .map_err(xlsx_err)?;

        // Prod Active (col 5)
        daily_sheet
            .write_string_with_format(row, 5, fmt_hms(m.productive_active_ms), str_fmt)
            .map_err(xlsx_err)?;

        // Prod Passive (col 6)
        daily_sheet
            .write_string_with_format(row, 6, fmt_hms(m.productive_passive_ms), str_fmt)
            .map_err(xlsx_err)?;

        // Productive Ratio (col 7)
        daily_sheet
            .write_number_with_format(row, 7, prod_ratio, pct_fmt)
            .map_err(xlsx_err)?;

        // Productive Active % (col 8)
        daily_sheet
            .write_number_with_format(row, 8, prod_active_pct, pct_fmt)
            .map_err(xlsx_err)?;

        // Cols 9-11 (avg/days) written in totals pass below; leave blank per-row

        // Daily Δ (col 12) - colored based on productive time change
        let delta_fmt = if *is_workday {
            if delta_productive > MS_PER_MIN_I64 {
                &delta_positive_fmt
            } else if delta_productive < -MS_PER_MIN_I64 {
                &delta_negative_fmt
            } else {
                &workday_fmt
            }
        } else if delta_productive > MS_PER_MIN_I64 {
            &weekend_delta_positive_fmt
        } else if delta_productive < -MS_PER_MIN_I64 {
            &weekend_delta_negative_fmt
        } else {
            &weekend_fmt
        };
        daily_sheet
            .write_string_with_format(row, 12, fmt_delta_hms(delta_productive), delta_fmt)
            .map_err(xlsx_err)?;

        row = row.saturating_add(1);
    }

    // ── Totals row ──────────────────────────────────────────────────────────────
    let mut daily_total = Metrics::default();

    for (_, m, _is_workday) in &daily_metrics {
        daily_total.total_ms = daily_total.total_ms.saturating_add(m.total_ms);
        daily_total.productive_ms = daily_total.productive_ms.saturating_add(m.productive_ms);
        daily_total.unproductive_ms = daily_total
            .unproductive_ms
            .saturating_add(m.unproductive_ms);
        daily_total.neutral_ms = daily_total.neutral_ms.saturating_add(m.neutral_ms);
        daily_total.productive_active_ms = daily_total
            .productive_active_ms
            .saturating_add(m.productive_active_ms);
        daily_total.productive_passive_ms = daily_total
            .productive_passive_ms
            .saturating_add(m.productive_passive_ms);
    }

    // Calculate sum of truncated values to show truncation error.
    // Sum ms first, then divide once to minimize per-row truncation loss.
    let mut sum_total_ms: i64 = 0;
    let mut sum_prod_ms: i64 = 0;
    let mut sum_unprod_ms: i64 = 0;
    let mut sum_neutral_ms: i64 = 0;
    let mut sum_prod_active_ms: i64 = 0;
    let mut sum_prod_passive_ms: i64 = 0;

    for (_, m, _) in &daily_metrics {
        sum_total_ms = sum_total_ms.saturating_add(m.total_ms);
        sum_prod_ms = sum_prod_ms.saturating_add(m.productive_ms);
        sum_unprod_ms = sum_unprod_ms.saturating_add(m.unproductive_ms);
        sum_neutral_ms = sum_neutral_ms.saturating_add(m.neutral_ms);
        sum_prod_active_ms = sum_prod_active_ms.saturating_add(m.productive_active_ms);
        sum_prod_passive_ms = sum_prod_passive_ms.saturating_add(m.productive_passive_ms);
    }
    let sum_truncated_total_secs = sum_total_ms / 1000;
    let sum_truncated_prod_secs = sum_prod_ms / 1000;
    let sum_truncated_unprod_secs = sum_unprod_ms / 1000;
    let sum_truncated_neutral_secs = sum_neutral_ms / 1000;
    let sum_truncated_prod_active_secs = sum_prod_active_ms / 1000;
    let sum_truncated_prod_passive_secs = sum_prod_passive_ms / 1000;

    let total_prod_ratio = if daily_total.total_ms > 0 {
        daily_total.productive_ms as f64 / daily_total.total_ms as f64
    } else {
        0.0
    };
    let total_prod_active_pct = if daily_total.productive_ms > 0 {
        daily_total.productive_active_ms as f64 / daily_total.productive_ms as f64
    } else {
        0.0
    };

    let avg_total_ms = if workdays > 0 {
        daily_total.total_ms / workdays
    } else {
        0
    };
    let avg_productive_ms = if workdays > 0 {
        daily_total.productive_ms / workdays
    } else {
        0
    };

    daily_sheet
        .write_string_with_format(row, 0, "TOTAL", &total_fmt)
        .map_err(xlsx_err)?;
    daily_sheet
        .write_string_with_format(
            row,
            1,
            fmt_hms(sum_truncated_total_secs.saturating_mul(1000)),
            &total_fmt,
        )
        .map_err(xlsx_err)?;
    daily_sheet
        .write_string_with_format(
            row,
            2,
            fmt_hms(sum_truncated_prod_secs.saturating_mul(1000)),
            &total_fmt,
        )
        .map_err(xlsx_err)?;
    daily_sheet
        .write_string_with_format(
            row,
            3,
            fmt_hms(sum_truncated_unprod_secs.saturating_mul(1000)),
            &total_fmt,
        )
        .map_err(xlsx_err)?;
    daily_sheet
        .write_string_with_format(
            row,
            4,
            fmt_hms(sum_truncated_neutral_secs.saturating_mul(1000)),
            &total_fmt,
        )
        .map_err(xlsx_err)?;
    daily_sheet
        .write_string_with_format(
            row,
            5,
            fmt_hms(sum_truncated_prod_active_secs.saturating_mul(1000)),
            &total_fmt,
        )
        .map_err(xlsx_err)?;
    daily_sheet
        .write_string_with_format(
            row,
            6,
            fmt_hms(sum_truncated_prod_passive_secs.saturating_mul(1000)),
            &total_fmt,
        )
        .map_err(xlsx_err)?;
    daily_sheet
        .write_number_with_format(row, 7, total_prod_ratio, &total_pct_fmt)
        .map_err(xlsx_err)?;
    daily_sheet
        .write_number_with_format(row, 8, total_prod_active_pct, &total_pct_fmt)
        .map_err(xlsx_err)?;
    daily_sheet
        .write_string_with_format(row, 9, fmt_hms(avg_total_ms), &total_fmt)
        .map_err(xlsx_err)?;
    daily_sheet
        .write_string_with_format(row, 10, fmt_hms(avg_productive_ms), &total_fmt)
        .map_err(xlsx_err)?;
    daily_sheet
        .write_number_with_format(row, 11, workdays as f64, &total_fmt)
        .map_err(xlsx_err)?;

    // ── Feature 5: Freeze header row + autofilter ───────────────────────
    let last_data_row = row.saturating_sub(1); // row before TOTAL
    let last_col: u16 = (headers.len() as u16).saturating_sub(1);

    daily_sheet.set_freeze_panes(1, 0).map_err(xlsx_err)?;

    if last_data_row >= 1 {
        daily_sheet
            .autofilter(0, 0, last_data_row, last_col)
            .map_err(xlsx_err)?;
    }

    // Conditional color formatting on Productive Ratio and Productive Active %
    if last_data_row >= 1 {
        // Productive Ratio (col 7) - red → yellow → green gradient
        let cond_ratio = ConditionalFormat3ColorScale::new()
            .set_minimum_color(Color::RGB(0xF8696B)) // red
            .set_midpoint_color(Color::RGB(0xFFEB84)) // yellow
            .set_maximum_color(Color::RGB(0x63BE7B)); // green

        daily_sheet
            .add_conditional_format(1, 7, last_data_row, 7, &cond_ratio)
            .map_err(xlsx_err)?;

        // Productive Active % (col 8) - red → yellow → green gradient
        let cond_active = ConditionalFormat3ColorScale::new()
            .set_minimum_color(Color::RGB(0xF8696B)) // red
            .set_midpoint_color(Color::RGB(0xFFEB84)) // yellow
            .set_maximum_color(Color::RGB(0x63BE7B)); // green

        daily_sheet
            .add_conditional_format(1, 8, last_data_row, 8, &cond_active)
            .map_err(xlsx_err)?;
    }
    daily_sheet.autofit();

    #[cfg(feature = "excel-extra-sheets")]
    {
        // ================================================================
        // Weekly Summary sheet
        // ================================================================
        let weeks = aggregate_weeks(&daily_metrics);

        let weekly_sheet = workbook.add_worksheet();
        weekly_sheet.set_name("Weekly Summary").map_err(xlsx_err)?;

        let week_headers = [
            "Week",
            "Start Date",
            "End Date",
            "Total Time",
            "Productive",
            "Unproductive",
            "Prod Ratio",
            "Avg Daily",
            "Trend",
        ];
        for (col, h) in week_headers.iter().enumerate() {
            weekly_sheet
                .write_string_with_format(0, col as u16, *h, &header_fmt)
                .map_err(xlsx_err)?;
        }

        for (widx, w) in weeks.iter().enumerate() {
            let wrow = (widx as u32).saturating_add(1);
            let week_label = format!("{}-W{:02}", w.iso_year, w.iso_week);
            let prod_ratio = if w.total_ms > 0 {
                w.productive_ms as f64 / w.total_ms as f64
            } else {
                0.0
            };
            let avg_daily = if w.day_count > 0 {
                w.total_ms / w.day_count as i64
            } else {
                0
            };

            weekly_sheet
                .write_string(wrow, 0, &week_label)
                .map_err(xlsx_err)?;
            weekly_sheet
                .write_string(wrow, 1, w.start_date.to_string())
                .map_err(xlsx_err)?;
            weekly_sheet
                .write_string(wrow, 2, w.end_date.to_string())
                .map_err(xlsx_err)?;
            weekly_sheet
                .write_string(wrow, 3, fmt_hms(w.total_ms))
                .map_err(xlsx_err)?;
            weekly_sheet
                .write_string(wrow, 4, fmt_hms(w.productive_ms))
                .map_err(xlsx_err)?;
            weekly_sheet
                .write_string(wrow, 5, fmt_hms(w.unproductive_ms))
                .map_err(xlsx_err)?;
            weekly_sheet
                .write_number_with_format(wrow, 6, prod_ratio, &pct_fmt)
                .map_err(xlsx_err)?;
            weekly_sheet
                .write_string(wrow, 7, fmt_hms(avg_daily))
                .map_err(xlsx_err)?;
        }

        // Feature 9: Sparkline for weekly productivity trend (Prod Ratio col)
        if weeks.len() >= 2 {
            let spark_last_row = weeks.len() as u32; // 1-based data rows
            let sparkline = Sparkline::new()
                .set_range(("Weekly Summary", 1, 6, spark_last_row, 6))
                .set_type(SparklineType::Column)
                .show_high_point(true)
                .show_low_point(true);

            // Place sparkline in col 8 ("Trend") of row 1
            weekly_sheet
                .add_sparkline(1, 8, &sparkline)
                .map_err(xlsx_err)?;
        }

        // Conditional format on weekly prod ratio
        if !weeks.is_empty() {
            let week_last_row = weeks.len() as u32;
            let cond_w = ConditionalFormat3ColorScale::new()
                .set_minimum_color(Color::RGB(0xF8696B)) // red
                .set_midpoint_color(Color::RGB(0xFFEB84)) // yellow
                .set_maximum_color(Color::RGB(0x63BE7B)); // green
            weekly_sheet
                .add_conditional_format(1, 6, week_last_row, 6, &cond_w)
                .map_err(xlsx_err)?;
        }

        weekly_sheet.set_freeze_panes(1, 0).map_err(xlsx_err)?;
        weekly_sheet.autofit();

        // ====================================================================
        // Feature 7: App Breakdown sheet
        // ====================================================================
        let until_utc = bounds
            .until_utc
            .as_deref()
            .unwrap_or("9999-12-31T23:59:59+00:00");

        let top_apps = query_top_apps(&app.conn, &bounds.since_utc, until_utc, 20)?;

        let app_sheet = workbook.add_worksheet();
        app_sheet.set_name("App Breakdown").map_err(xlsx_err)?;

        let app_headers = [
            "Rank",
            "App Name",
            "Category",
            "Total Time",
            "% of Total",
            "Trend",
        ];
        for (col, h) in app_headers.iter().enumerate() {
            app_sheet
                .write_string_with_format(0, col as u16, *h, &header_fmt)
                .map_err(xlsx_err)?;
        }

        // Query total time from ALL apps, not just top-20, for accurate percentage
        let grand_total_ms: i64 = {
            let mut stmt = app.conn.prepare(
                "SELECT COALESCE(SUM(active_ms + COALESCE(passive_ms,0) + idle_ms), 0)
                 FROM events
                 WHERE timestamp >= ?1 AND timestamp < ?2",
            )?;
            stmt.query_row(params![&bounds.since_utc, until_utc], |r| r.get(0))?
        };

        for (aidx, app_row) in top_apps.iter().enumerate() {
            let arow = (aidx as u32).saturating_add(1);
            let pct_of_total = if grand_total_ms > 0 {
                app_row.total_ms as f64 / grand_total_ms as f64
            } else {
                0.0
            };

            app_sheet
                .write_number(arow, 0, (aidx + 1) as f64)
                .map_err(xlsx_err)?;
            app_sheet
                .write_string(arow, 1, &app_row.app_id)
                .map_err(xlsx_err)?;
            app_sheet
                .write_string(arow, 2, app_row.category.to_string())
                .map_err(xlsx_err)?;
            app_sheet
                .write_string(arow, 3, fmt_hms(app_row.total_ms))
                .map_err(xlsx_err)?;
            app_sheet
                .write_number_with_format(arow, 4, pct_of_total, &pct_fmt)
                .map_err(xlsx_err)?;
        }

        // Feature 9: Sparkline for app breakdown (% of Total column)
        if top_apps.len() >= 2 {
            let app_last_row = top_apps.len() as u32;
            let app_sparkline = Sparkline::new()
                .set_range(("App Breakdown", 1, 4, app_last_row, 4))
                .set_type(SparklineType::Column)
                .show_high_point(true);

            app_sheet
                .add_sparkline(1, 5, &app_sparkline)
                .map_err(xlsx_err)?;
        }

        app_sheet.set_freeze_panes(1, 0).map_err(xlsx_err)?;
        app_sheet.autofit();
    } // end of excel-extra-sheets feature block

    // ── Save ────────────────────────────────────────────────────────────
    workbook.save(path).map_err(xlsx_err)?;

    println!("Exported to {path}");
    Ok(())
}
