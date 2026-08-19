//! Export functions for CSV, JSON, and heatmap output.

use chrono::Timelike;
use serde::Serialize;

use super::interval::load_human_intervals;
use super::query::query_report_range;
use super::{App, TimeRange, UNTIL_SENTINEL, day_end_utc, day_start_utc};
use crate::config::Category;
use crate::error::Error;
use crate::fmt::fmt_hms;

/// Export activity data as CSV for a date range.
pub fn export_csv_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let bounds = range.resolve(&app.config)?;
    println!(
        "Date,Screen Time (h:mm:ss),Productive (h:mm:ss),Unproductive (h:mm:ss),Undefined (h:mm:ss),ProdActive (h:mm:ss),ProdPassive (h:mm:ss),Productive Ratio,Productive Active %"
    );
    let mut date = bounds.start_date;
    let mut iterations = 0u32;
    while date <= bounds.end_date {
        iterations += 1;
        if iterations > 10_000 {
            return Err(Error::InvalidArgument(
                "date range exceeds 10,000 day limit".into(),
            ));
        }
        let start = day_start_utc(&app.config, date)?;
        let end = day_end_utc(&app.config, date)?;
        let m = super::query::metrics_between(&app.conn, &app.config, &start, &end)?;
        let prod_ratio = if m.total_ms > 0 {
            format!("{:.1}%", m.productive_ms as f64 / m.total_ms as f64 * 100.0)
        } else {
            "0.0%".to_string()
        };
        let prod_active_pct = if m.productive_ms > 0 {
            format!(
                "{:.1}%",
                m.productive_active_ms as f64 / m.productive_ms as f64 * 100.0
            )
        } else {
            "0.0%".to_string()
        };
        println!(
            "{},{},{},{},{},{},{},{},{}",
            date,
            fmt_hms(m.total_ms),
            fmt_hms(m.productive_ms),
            fmt_hms(m.unproductive_ms),
            fmt_hms(m.neutral_ms),
            fmt_hms(m.productive_active_ms),
            fmt_hms(m.productive_passive_ms),
            prod_ratio,
            prod_active_pct
        );
        date += chrono::Duration::days(1);
    }
    Ok(())
}

/// Export activity data as JSON for a date range.
pub fn export_json_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let data = query_report_range(app, range)?;
    let json = serde_json::to_string_pretty(&data)
        .map_err(|e| Error::NiriError(format!("JSON serialization failed: {}", e)))?;
    println!("{}", json);
    Ok(())
}

#[derive(Serialize)]
struct HeatmapCell {
    date: String,
    hour: u32,
    productive_ms: i64,
    unproductive_ms: i64,
    neutral_ms: i64,
    total_ms: i64,
    keystrokes: i64,
}

/// Export a cron-friendly summary of productivity metrics for a date range.
pub fn export_cron_summary(app: &App, range: TimeRange) -> Result<(), Error> {
    let data = query_report_range(app, range)?;
    let productive_ms = data
        .categories
        .iter()
        .find(|c| c.category == Category::Productive)
        .map_or(0, |c| c.total_ms);
    let unproductive_ms = data
        .categories
        .iter()
        .find(|c| c.category == Category::Unproductive)
        .map_or(0, |c| c.total_ms);
    let ratio = if data.total_ms > 0 {
        (productive_ms as f64 / data.total_ms as f64 * 100.0).round() as i64
    } else {
        0
    };
    let top_app = data.top_apps.first().map_or("-", |a| a.app_id.as_str());
    println!(
        "{}|{}|{}|{}|{}%|{}",
        data.since_str
            .split_whitespace()
            .next()
            .unwrap_or(&data.since_str),
        fmt_hms(data.total_ms),
        fmt_hms(productive_ms),
        fmt_hms(unproductive_ms),
        ratio,
        top_app
    );
    Ok(())
}

/// Export hourly activity heatmap as JSON for a date range.
pub fn export_heatmap_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let bounds = range.resolve(&app.config)?;
    let until_utc = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);
    let mut heatmap: std::collections::BTreeMap<(String, u32), HeatmapCell> =
        std::collections::BTreeMap::new();
    for event in load_human_intervals(&app.conn, &app.config, &bounds.since_utc, until_utc)? {
        for slice in event.minute_slices() {
            let Some(timestamp) = slice.local_start(&app.config) else {
                continue;
            };
            let date = timestamp.format("%Y-%m-%d").to_string();
            let hour = timestamp.hour();
            let total_ms = slice.active_ms.saturating_add(slice.passive_ms);
            let cell = heatmap
                .entry((date.clone(), hour))
                .or_insert_with(|| HeatmapCell {
                    date,
                    hour,
                    productive_ms: 0,
                    unproductive_ms: 0,
                    neutral_ms: 0,
                    total_ms: 0,
                    keystrokes: 0,
                });
            match slice.category {
                Category::Productive => {
                    cell.productive_ms = cell.productive_ms.saturating_add(total_ms);
                }
                Category::Unproductive => {
                    cell.unproductive_ms = cell.unproductive_ms.saturating_add(total_ms);
                }
                Category::Neutral => {
                    cell.neutral_ms = cell.neutral_ms.saturating_add(total_ms);
                }
            }
            cell.total_ms = cell.total_ms.saturating_add(total_ms);
            cell.keystrokes = cell.keystrokes.saturating_add(slice.keystrokes);
        }
    }
    let json = serde_json::to_string_pretty(&heatmap.into_values().collect::<Vec<_>>())
        .map_err(|e| Error::NiriError(format!("JSON serialization failed: {}", e)))?;
    println!("{}", json);
    Ok(())
}
