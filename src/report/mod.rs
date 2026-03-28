//! Report generation and export module.
//!
//! This module handles all activity report generation, display, and export
//! functionality including Excel, CSV, JSON, and terminal output.

mod excel;
mod types;

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{Datelike, NaiveDate, Timelike};
// Re-export Excel functions
pub use excel::export_xlsx_range;
use owo_colors::OwoColorize;
use rusqlite::{Connection, params};
use serde::Serialize;
// Re-export internal types for crate use
pub(crate) use types::Metrics;
// Re-export all public types
pub use types::{
    AppBreakdown, AppGroup, AwayData, CategoryBreakdown, DailyBreakdown, FatigueIndicators,
    FatigueTrend, FlowSession, FlowSummary, FocusStreak, GapEntry, GapSummary, GapType,
    HourBreakdown, HourlyErrorRate, InputMetrics, MetricsData, ReportData, ScheduleBreakdown,
    StreakSummary, TimelineBucket, TimelineData, TodayData, TodayRow,
};

use crate::config::{Category, Config, get_data_dir, load_config};
use crate::db::{reclassify_all, run_migrations};
use crate::error::Error;
use crate::fmt::{
    cat_bar, cat_bar_fractional, cat_colored, cat_label, colored_bar, fmt_distance, fmt_duration,
    fmt_duration_compact, fmt_hms, pct, section_header, truncate,
};

/// Sentinel value for unbounded upper time range in parameterized queries.
const UNTIL_SENTINEL: &str = "9999-12-31T23:59:59+00:00";

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

pub struct App {
    pub config: Config,
    pub conn: Connection,
}

impl App {
    pub fn open() -> Result<App, Error> {
        let config = load_config()?;
        let db_path = get_data_dir()?.join("activity.db");
        let mut conn = Connection::open(&db_path)?;
        run_migrations(&mut conn, &config)?;
        reclassify_all(&mut conn, &config)?;
        Ok(App { config, conn })
    }
}

/// Time range specification for reports
#[derive(Debug, Clone)]
pub enum TimeRange {
    Days(u32),
    DaysAligned(u32),
    Yesterday,
    LastWeek,
    ThisWeek,
    LastMonth,
    ThisMonth,
    DateRange(NaiveDate, NaiveDate),
}

/// Resolved time boundaries for queries
pub struct TimeBounds {
    /// Start timestamp in RFC3339 UTC format for SQL queries
    pub since_utc: String,
    /// End timestamp in RFC3339 UTC format for SQL queries (None = now)
    pub until_utc: Option<String>,
    /// Display string for start (local time)
    pub since_str: String,
    /// Display string for end (local time)
    pub now_str: String,
    /// Start date for daily iteration
    pub start_date: NaiveDate,
    /// End date for daily iteration
    pub end_date: NaiveDate,
}

impl TimeRange {
    pub fn resolve(&self, config: &Config) -> Result<TimeBounds, Error> {
        let now_fixed = config.local_now();
        let today = now_fixed.date_naive();
        let now_str = now_fixed.format("%Y-%m-%d %H:%M").to_string();

        match self {
            TimeRange::Days(days) => {
                let since_date = today - chrono::Duration::days(*days as i64);
                let since_utc = config_day_start_utc(config, since_date)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: None,
                    since_str: format!("{} 00:00", since_date),
                    now_str,
                    start_date: since_date,
                    end_date: today,
                })
            }
            TimeRange::DaysAligned(days) => {
                let end_date = today - chrono::Duration::days(1);
                let start_date = end_date - chrono::Duration::days(*days as i64 - 1);
                let since_utc = config_day_start_utc(config, start_date)?;
                let until_utc = config_day_end_utc(config, end_date)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: Some(until_utc),
                    since_str: format!("{} 00:00", start_date),
                    now_str: format!("{} 23:59", end_date),
                    start_date,
                    end_date,
                })
            }
            TimeRange::LastWeek => {
                let days_since_monday = today.weekday().num_days_from_monday();
                let this_monday = today - chrono::Duration::days(days_since_monday as i64);
                let last_monday = this_monday - chrono::Duration::days(7);
                let last_sunday = last_monday + chrono::Duration::days(6);
                let since_utc = config_day_start_utc(config, last_monday)?;
                let until_utc = config_day_end_utc(config, last_sunday)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: Some(until_utc),
                    since_str: format!("{} 00:00", last_monday),
                    now_str: format!("{} 23:59", last_sunday),
                    start_date: last_monday,
                    end_date: last_sunday,
                })
            }
            TimeRange::ThisWeek => {
                let days_since_monday = today.weekday().num_days_from_monday();
                let this_monday = today - chrono::Duration::days(days_since_monday as i64);
                let since_utc = config_day_start_utc(config, this_monday)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: None,
                    since_str: format!("{} 00:00", this_monday),
                    now_str,
                    start_date: this_monday,
                    end_date: today,
                })
            }
            TimeRange::LastMonth => {
                let first_of_this_month = today
                    .with_day(1)
                    .ok_or_else(|| Error::NiriError("invalid date calculation".into()))?;
                let last_day_prev_month = first_of_this_month - chrono::Duration::days(1);
                let first_of_last_month = last_day_prev_month
                    .with_day(1)
                    .ok_or_else(|| Error::NiriError("invalid date calculation".into()))?;
                let since_utc = config_day_start_utc(config, first_of_last_month)?;
                let until_utc = config_day_end_utc(config, last_day_prev_month)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: Some(until_utc),
                    since_str: format!("{} 00:00", first_of_last_month),
                    now_str: format!("{} 23:59", last_day_prev_month),
                    start_date: first_of_last_month,
                    end_date: last_day_prev_month,
                })
            }
            TimeRange::ThisMonth => {
                let first_of_month = today
                    .with_day(1)
                    .ok_or_else(|| Error::NiriError("invalid date calculation".into()))?;
                let since_utc = config_day_start_utc(config, first_of_month)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: None,
                    since_str: format!("{} 00:00", first_of_month),
                    now_str,
                    start_date: first_of_month,
                    end_date: today,
                })
            }
            TimeRange::Yesterday => {
                let yesterday = today - chrono::Duration::days(1);
                let since_utc = config_day_start_utc(config, yesterday)?;
                let until_utc = config_day_end_utc(config, yesterday)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: Some(until_utc),
                    since_str: format!("{} 00:00", yesterday),
                    now_str: format!("{} 23:59", yesterday),
                    start_date: yesterday,
                    end_date: yesterday,
                })
            }
            TimeRange::DateRange(start, end) => {
                let since_utc = config_day_start_utc(config, *start)?;
                let until_utc = config_day_end_utc(config, *end)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: Some(until_utc),
                    since_str: format!("{} 00:00", start),
                    now_str: format!("{} 23:59", end),
                    start_date: *start,
                    end_date: *end,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Time helpers
// ---------------------------------------------------------------------------

pub(super) fn config_day_start_utc(
    config: &Config,
    date: chrono::NaiveDate,
) -> Result<String, Error> {
    config
        .day_start_utc(date)
        .map(|dt| dt.to_rfc3339())
        .ok_or_else(|| Error::NiriError("day start is not representable".into()))
}

pub(super) fn config_day_end_utc(
    config: &Config,
    date: chrono::NaiveDate,
) -> Result<String, Error> {
    config
        .day_end_utc(date)
        .map(|dt| dt.to_rfc3339())
        .ok_or_else(|| Error::NiriError("day end is not representable".into()))
}

// ---------------------------------------------------------------------------
// Query functions
// ---------------------------------------------------------------------------

pub fn query_today(app: &App) -> Result<TodayData, Error> {
    let date = app.config.local_date_today();
    let day_start_utc = config_day_start_utc(&app.config, date)?;
    let day_end_utc = config_day_end_utc(&app.config, date)?;
    let mut stmt = app.conn.prepare(
        "SELECT app_id, category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) as total_ms 
         FROM events 
         WHERE timestamp >= ?1 AND timestamp < ?2
         GROUP BY app_id 
         ORDER BY total_ms DESC",
    )?;

    let rows = stmt
        .query_map(params![&day_start_utc, &day_end_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(app_id, category_raw, total_ms)| TodayRow {
            app_id,
            category: Category::from_str(&category_raw).unwrap_or(Category::Neutral),
            total_ms,
        })
        .collect();

    Ok(TodayData { date, rows })
}

pub fn query_metrics_range(app: &App, range: TimeRange) -> Result<MetricsData, Error> {
    let bounds = range.resolve(&app.config)?;
    let until_utc = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);
    let days = u32::try_from((bounds.end_date - bounds.start_date).num_days())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let mut stmt = app.conn.prepare(
        "SELECT category, SUM(active_ms) as active, COALESCE(SUM(passive_ms),0) as passive, SUM(idle_ms) as idle
         FROM events
         WHERE timestamp >= ?1 AND timestamp < ?2
         GROUP BY category",
    )?;
    let mut m = MetricsData {
        days,
        total_ms: 0,
        productive_ms: 0,
        unproductive_ms: 0,
        neutral_ms: 0,
        productive_active_ms: 0,
        productive_passive_ms: 0,
        productive_idle_ms: 0,
    };

    let rows = stmt.query_map(params![&bounds.since_utc, until_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    for row in rows {
        let (category_raw, active_ms, passive_ms, idle_ms) = row?;
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

    Ok(m)
}

pub fn query_timeline(app: &App, days_back: u32, bucket_min: u32) -> Result<TimelineData, Error> {
    if bucket_min == 0 {
        return Err(Error::NiriError("bucket size must be positive".into()));
    }
    let date = app.config.local_date_today() - chrono::Duration::days(days_back as i64);
    let day_start_utc = config_day_start_utc(&app.config, date)?;
    let day_end_utc = config_day_end_utc(&app.config, date)?;

    let mut stmt = app.conn.prepare(
        "SELECT timestamp, app_id, category, active_ms, COALESCE(passive_ms,0), idle_ms, keystrokes
         FROM events
         WHERE timestamp >= ?1 AND timestamp < ?2
         ORDER BY timestamp",
    )?;

    struct EventRow {
        timestamp: String,
        app_id: String,
        category: String,
        active_ms: i64,
        passive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
    }

    let events: Vec<EventRow> = stmt
        .query_map(params![&day_start_utc, &day_end_utc], |row| {
            Ok(EventRow {
                timestamp: row.get(0)?,
                app_id: row.get(1)?,
                category: row.get(2)?,
                active_ms: row.get(3)?,
                passive_ms: row.get(4)?,
                idle_ms: row.get(5)?,
                keystrokes: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    struct BucketAcc {
        productive_ms: i64,
        neutral_ms: i64,
        unproductive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
        /// Cumulative time per app_id, for computing true dominant app
        app_totals: HashMap<String, i64>,
    }

    let mut bucket_map: std::collections::BTreeMap<u32, BucketAcc> =
        std::collections::BTreeMap::new();

    for ev in &events {
        let Some(ts) = app.config.parse_timestamp_to_local(&ev.timestamp) else {
            continue;
        };

        let minutes_since_midnight = ts.hour() * 60 + ts.minute();
        let bucket_key = minutes_since_midnight / bucket_min * bucket_min;
        let total_ms = ev
            .active_ms
            .saturating_add(ev.passive_ms)
            .saturating_add(ev.idle_ms);

        let b = bucket_map.entry(bucket_key).or_insert_with(|| BucketAcc {
            productive_ms: 0,
            neutral_ms: 0,
            unproductive_ms: 0,
            idle_ms: 0,
            keystrokes: 0,
            app_totals: HashMap::new(),
        });

        match Category::from_str(&ev.category).unwrap_or(Category::Neutral) {
            Category::Productive => b.productive_ms = b.productive_ms.saturating_add(total_ms),
            Category::Unproductive => {
                b.unproductive_ms = b.unproductive_ms.saturating_add(total_ms);
            }
            Category::Neutral => b.neutral_ms = b.neutral_ms.saturating_add(total_ms),
        }
        // Only count actual idle time, not passive (passive is already included in
        // category totals)
        b.idle_ms = b.idle_ms.saturating_add(ev.idle_ms);
        b.keystrokes = b.keystrokes.saturating_add(ev.keystrokes);

        // Accumulate per-app time so dominant app reflects total time, not single-event
        // max
        let app_entry = b.app_totals.entry(ev.app_id.clone()).or_insert(0);
        *app_entry = app_entry.saturating_add(total_ms);
    }

    let buckets = bucket_map
        .into_iter()
        .map(|(key, b)| {
            let dominant_app = b
                .app_totals
                .iter()
                .max_by_key(|(_, ms)| *ms)
                .map(|(app, _)| app.clone())
                .unwrap_or_default();
            TimelineBucket {
                hour: key / 60,
                minute: key % 60,
                productive_ms: b.productive_ms,
                neutral_ms: b.neutral_ms,
                unproductive_ms: b.unproductive_ms,
                idle_ms: b.idle_ms,
                keystrokes: b.keystrokes,
                dominant_app,
            }
        })
        .collect();

    Ok(TimelineData {
        date,
        bucket_min,
        buckets,
    })
}

pub fn query_report_range(app: &App, range: TimeRange) -> Result<ReportData, Error> {
    let bounds = range.resolve(&app.config)?;
    let since_utc = &bounds.since_utc;
    let since_str = bounds.since_str.clone();
    let now_str = bounds.now_str.clone();

    let until_utc = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);

    let (
        total_ms,
        active_ms,
        passive_ms,
        idle_ms,
        total_keys,
        total_clicks,
        total_scroll,
        total_distance,
        total_events,
        jiggler_count,
    ): (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = app.conn.query_row(
        "SELECT COALESCE(SUM(active_ms + COALESCE(passive_ms,0) + idle_ms),0),
                COALESCE(SUM(active_ms),0),
                COALESCE(SUM(passive_ms),0),
                COALESCE(SUM(idle_ms),0), COALESCE(SUM(keystrokes),0),
                COALESCE(SUM(mouse_clicks),0), COALESCE(SUM(scroll_events),0),
                COALESCE(SUM(mouse_distance),0), COUNT(*),
                COALESCE(SUM(CASE WHEN jiggler_detected = 1 THEN 1 ELSE 0 END),0)
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2",
        params![since_utc, until_utc],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        },
    )?;

    let mut cat_stmt = app.conn.prepare(
        "SELECT category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms), SUM(active_ms), SUM(idle_ms)
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2
         GROUP BY category ORDER BY SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) DESC",
    )?;

    let categories: Vec<CategoryBreakdown> = cat_stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(
            |(cat_raw, cat_total, cat_active, cat_idle)| CategoryBreakdown {
                category: Category::from_str(&cat_raw).unwrap_or(Category::Neutral),
                total_ms: cat_total,
                active_ms: cat_active,
                idle_ms: cat_idle,
            },
        )
        .collect();

    let mut app_stmt = app.conn.prepare(
        "SELECT app_id, category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms), SUM(active_ms), SUM(keystrokes), SUM(mouse_clicks)
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2
         GROUP BY app_id, category ORDER BY SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) DESC",
    )?;

    let flat_apps: Vec<AppBreakdown> = app_stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(
            |(app_id, cat_raw, app_total, app_active, keys, clicks)| AppBreakdown {
                app_id,
                category: Category::from_str(&cat_raw).unwrap_or(Category::Neutral),
                total_ms: app_total,
                active_ms: app_active,
                keys,
                clicks,
            },
        )
        .collect();

    let top_apps = group_apps(flat_apps, 15);

    let mut raw_stmt = app.conn.prepare(
        "SELECT timestamp, active_ms, COALESCE(passive_ms,0), idle_ms, keystrokes
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2",
    )?;

    struct RawRow {
        timestamp: String,
        active_ms: i64,
        passive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
    }

    let raw_rows: Vec<RawRow> = raw_stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok(RawRow {
                timestamp: row.get(0)?,
                active_ms: row.get(1)?,
                passive_ms: row.get(2)?,
                idle_ms: row.get(3)?,
                keystrokes: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut daily_map: std::collections::BTreeMap<String, (i64, i64, i64, i64)> =
        std::collections::BTreeMap::new();
    let mut hourly: HashMap<u32, (i64, i64)> = HashMap::new();

    for row in &raw_rows {
        let Some(local_dt) = app.config.parse_timestamp_to_local(&row.timestamp) else {
            continue;
        };

        let day_key = local_dt.format("%Y-%m-%d").to_string();
        let total = row
            .active_ms
            .saturating_add(row.passive_ms)
            .saturating_add(row.idle_ms);

        let d = daily_map.entry(day_key).or_insert((0, 0, 0, 0));
        d.0 = d.0.saturating_add(total);
        d.1 = d.1.saturating_add(row.active_ms);
        d.2 = d.2.saturating_add(row.keystrokes);
        d.3 = d.3.saturating_add(1);

        // Split event duration across hour boundaries
        // ms_left_in_hour = ms until the next hour starts
        let minute = local_dt.minute() as i64;
        let second = local_dt.second() as i64;
        let millis = (local_dt.nanosecond() / 1_000_000) as i64;
        let ms_into_hour = (minute * 60 + second) * 1000 + millis;
        let ms_left_in_hour = 3_600_000_i64.saturating_sub(ms_into_hour).max(1);

        let mut remaining_ms = total;
        let mut current_hour = local_dt.hour();
        let mut first_chunk = true;

        while remaining_ms > 0 {
            let chunk_ms = if first_chunk {
                remaining_ms.min(ms_left_in_hour)
            } else {
                remaining_ms.min(3_600_000)
            };
            if chunk_ms <= 0 {
                break; // guard against infinite loop if ms_left_in_hour is 0
            }

            let h = hourly.entry(current_hour).or_insert((0, 0));
            h.0 += chunk_ms;
            if first_chunk {
                h.1 += row.keystrokes;
            }

            remaining_ms = remaining_ms.saturating_sub(chunk_ms);
            current_hour = (current_hour + 1) % 24;
            first_chunk = false;
        }
    }

    let daily: Vec<DailyBreakdown> = daily_map
        .into_iter()
        .map(
            |(date, (day_total, day_active, keys, switches))| DailyBreakdown {
                date,
                total_ms: day_total,
                active_ms: day_active,
                keystrokes: keys,
                switches,
            },
        )
        .collect();

    let mut hour_vec: Vec<(u32, i64, i64)> = hourly
        .into_iter()
        .map(|(h, (total, keys))| (h, total, keys))
        .collect();
    hour_vec.sort_by_key(|item| std::cmp::Reverse(item.1));
    hour_vec.truncate(5);

    let peak_hours: Vec<HourBreakdown> = hour_vec
        .into_iter()
        .map(|(hour, hour_total, keys)| HourBreakdown {
            hour,
            total_ms: hour_total,
            keystrokes: keys,
        })
        .collect();

    let schedule = if app.config.schedule.enabled {
        let mut work_total_ms: i64 = 0;
        let mut work_active_ms: i64 = 0;
        let mut work_keys: i64 = 0;
        let mut after_total_ms: i64 = 0;
        let mut after_active_ms: i64 = 0;
        let mut after_keys: i64 = 0;

        for row in &raw_rows {
            let Some(local_dt) = app.config.parse_timestamp_to_local(&row.timestamp) else {
                continue;
            };
            let total = row
                .active_ms
                .saturating_add(row.passive_ms)
                .saturating_add(row.idle_ms);
            if app.config.schedule.is_in_schedule(&local_dt) {
                work_total_ms = work_total_ms.saturating_add(total);
                work_active_ms = work_active_ms.saturating_add(row.active_ms);
                work_keys = work_keys.saturating_add(row.keystrokes);
            } else {
                after_total_ms = after_total_ms.saturating_add(total);
                after_active_ms = after_active_ms.saturating_add(row.active_ms);
                after_keys = after_keys.saturating_add(row.keystrokes);
            }
        }

        Some(ScheduleBreakdown {
            work_label: format!(
                "Work Hours ({}-{}):",
                app.config.schedule.start, app.config.schedule.end
            ),
            work_total_ms,
            work_active_ms,
            work_keys,
            after_total_ms,
            after_active_ms,
            after_keys,
        })
    } else {
        None
    };

    let away = query_gaps(
        &app.conn,
        &app.config,
        since_utc,
        until_utc,
        &app.config.sleep,
    )
    .map_err(|e| eprintln!("[report] query_gaps failed: {e}"))
    .ok();
    let streaks = query_streaks(&app.conn, &app.config, since_utc, until_utc)
        .map_err(|e| eprintln!("[report] query_streaks failed: {e}"))
        .ok();

    let input_metrics = query_input_metrics(&app.conn, since_utc, until_utc, total_keys)
        .map_err(|e| eprintln!("[report] query_input_metrics failed: {e}"))
        .ok();

    let flow = query_flow_sessions(&app.conn, &app.config, since_utc, until_utc)
        .map_err(|e| eprintln!("[report] query_flow_sessions failed: {e}"))
        .ok();

    let fatigue = query_fatigue_indicators(&app.conn, &app.config, since_utc, until_utc)
        .map_err(|e| eprintln!("[report] query_fatigue_indicators failed: {e}"))
        .ok();

    Ok(ReportData {
        since_str,
        now_str,
        total_ms,
        active_ms,
        passive_ms,
        idle_ms,
        total_keys,
        total_clicks,
        total_scroll,
        total_distance,
        total_events,
        jiggler_count,
        categories,
        top_apps,
        daily,
        peak_hours,
        schedule,
        away,
        streaks,
        input_metrics,
        flow,
        fatigue,
    })
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn group_apps(flat: Vec<AppBreakdown>, limit: usize) -> Vec<AppGroup> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<AppGroup> = Vec::new();

    for entry in flat {
        if let Some(&idx) = index.get(&entry.app_id) {
            let group = &mut groups[idx];
            group.total_ms = group.total_ms.saturating_add(entry.total_ms);
            group.active_ms = group.active_ms.saturating_add(entry.active_ms);
            group.keys = group.keys.saturating_add(entry.keys);
            group.clicks = group.clicks.saturating_add(entry.clicks);
            group.children.push(entry);
        } else {
            let idx = groups.len();
            let app_id = entry.app_id.clone();
            index.insert(app_id.clone(), idx);
            groups.push(AppGroup {
                app_id,
                total_ms: entry.total_ms,
                active_ms: entry.active_ms,
                keys: entry.keys,
                clicks: entry.clicks,
                children: vec![entry],
            });
        }
    }

    groups.sort_by_key(|g| std::cmp::Reverse(g.total_ms));
    groups.truncate(limit);
    groups
}

fn classify_gap(
    start_hour: u32,
    end_hour: u32,
    gap_hours: f64,
    sleep: &crate::config::SleepConfig,
) -> Option<GapType> {
    // Filter by configured min/max gap
    if !(sleep.gap_min_hours..=sleep.gap_max_hours).contains(&gap_hours) {
        return None;
    }

    // Rule 1: Overnight auto-detect - any gap ≥ overnight_auto_hours spanning
    // midnight = sleep
    let spans_midnight = start_hour > end_hour;
    if sleep.overnight_auto_hours > 0.0 && gap_hours >= sleep.overnight_auto_hours && spans_midnight
    {
        return Some(GapType::Sleep);
    }

    // Rule 2: Night window check - gap starting in sleep window + min_hours = sleep
    // Handle wraparound: earliest_hour (18) > latest_hour (8) means 18:00-08:00
    let in_night_window = if sleep.earliest_hour > sleep.latest_hour {
        // Window wraps midnight: 18:00 -> 08:00
        start_hour >= sleep.earliest_hour || start_hour < sleep.latest_hour
    } else {
        // Normal window (shouldn't happen with defaults, but support it)
        start_hour >= sleep.earliest_hour && start_hour < sleep.latest_hour
    };

    if in_night_window && gap_hours >= sleep.min_hours {
        return Some(GapType::Sleep);
    }

    // Rule 3: Duration-based fallback
    if gap_hours >= sleep.long_break_min_hours {
        Some(GapType::LongBreak)
    } else {
        Some(GapType::ShortBreak)
    }
}

fn dominant_app(app_ms: &HashMap<String, i64>) -> String {
    app_ms
        .iter()
        .max_by_key(|(_, ms)| *ms)
        .map(|(app, _)| app.clone())
        .unwrap_or_default()
}

fn flush_streak(
    streaks: &mut Vec<FocusStreak>,
    start: &mut Option<String>,
    app_ms: &mut HashMap<String, i64>,
    productive_ms: i64,
    keys: i64,
    config: &Config,
) {
    if productive_ms >= 300_000 {
        let start_str = start.take().unwrap_or_default();
        let start_local = config
            .parse_timestamp_to_local(&start_str)
            .map(|dt| dt.format("%m-%d %H:%M").to_string())
            .unwrap_or(start_str);
        streaks.push(FocusStreak {
            app_id: dominant_app(app_ms),
            start_time: start_local,
            duration_ms: productive_ms,
            keystrokes: keys,
        });
    } else {
        let _ = start.take();
    }
    app_ms.clear();
}

fn query_streaks(
    conn: &Connection,
    config: &Config,
    since_utc: &str,
    until_utc: &str,
) -> Result<StreakSummary, Error> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, app_id, category, active_ms, keystrokes, mouse_clicks
         FROM events
         WHERE timestamp >= ?1 AND timestamp < ?2
         ORDER BY timestamp",
    )?;

    struct EventRow {
        timestamp: String,
        app_id: String,
        category: String,
        active_ms: i64,
        keystrokes: i64,
        mouse_clicks: i64,
    }

    let events: Vec<EventRow> = stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok(EventRow {
                timestamp: row.get(0)?,
                app_id: row.get(1)?,
                category: row.get(2)?,
                active_ms: row.get(3)?,
                keystrokes: row.get(4)?,
                mouse_clicks: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    #[allow(clippy::cast_possible_wrap)] // Config values are small positive numbers, no wrap risk
    let away_ms = config
        .away_threshold_secs
        .cast_signed()
        .saturating_mul(1_000);
    #[allow(clippy::cast_possible_wrap)]
    let tolerance_ms = config
        .streak_break_tolerance_secs
        .cast_signed()
        .saturating_mul(1_000);
    #[allow(clippy::cast_possible_wrap)]
    let idle_timeout_ms = config
        .streak_idle_timeout_secs
        .cast_signed()
        .saturating_mul(1_000);

    let mut streaks: Vec<FocusStreak> = Vec::new();
    let mut streak_start: Option<String> = None;
    let mut streak_app_ms: HashMap<String, i64> = HashMap::new();
    let mut streak_productive_ms: i64 = 0;
    let mut streak_keys: i64 = 0;
    let mut pending_unproductive_ms: i64 = 0;
    let mut in_streak = false;
    let mut last_input_ts: Option<chrono::DateTime<chrono::FixedOffset>> = None;

    for ev in &events {
        let cat = Category::from_str(&ev.category).unwrap_or(Category::Neutral);
        let is_productive = cat == Category::Productive;
        let has_input = ev.keystrokes > 0 || ev.mouse_clicks > 0;
        let cur_dt = config.parse_timestamp_to_local(&ev.timestamp);

        if in_streak {
            let input_idle = match (last_input_ts, cur_dt) {
                (Some(last), Some(cur)) => (cur - last).num_milliseconds() > idle_timeout_ms,
                _ => false,
            };

            let wall_gap = match (last_input_ts.or(cur_dt), cur_dt) {
                (Some(prev), Some(cur)) if last_input_ts.is_some() => {
                    (cur - prev).num_milliseconds() > away_ms
                }
                _ => false,
            };

            if input_idle || wall_gap {
                flush_streak(
                    &mut streaks,
                    &mut streak_start,
                    &mut streak_app_ms,
                    streak_productive_ms,
                    streak_keys,
                    config,
                );
                in_streak = false;
                streak_productive_ms = 0;
                streak_keys = 0;
                pending_unproductive_ms = 0;
            }
        }

        if is_productive {
            if in_streak {
                if pending_unproductive_ms > tolerance_ms {
                    flush_streak(
                        &mut streaks,
                        &mut streak_start,
                        &mut streak_app_ms,
                        streak_productive_ms,
                        streak_keys,
                        config,
                    );
                    in_streak = false;
                    streak_productive_ms = 0;
                    streak_keys = 0;
                }
                pending_unproductive_ms = 0;
            }

            if in_streak {
                streak_productive_ms = streak_productive_ms.saturating_add(ev.active_ms);
                streak_keys = streak_keys.saturating_add(ev.keystrokes);
                // Avoid clone on hot path: use entry API with reference check first
                if let Some(entry) = streak_app_ms.get_mut(&ev.app_id) {
                    *entry = entry.saturating_add(ev.active_ms);
                } else {
                    streak_app_ms.insert(ev.app_id.clone(), ev.active_ms);
                }
            } else {
                in_streak = true;
                streak_start = Some(ev.timestamp.clone());
                streak_productive_ms = ev.active_ms;
                streak_keys = ev.keystrokes;
                pending_unproductive_ms = 0;
                streak_app_ms.clear();
                streak_app_ms.insert(ev.app_id.clone(), ev.active_ms);
            }
        } else if in_streak {
            pending_unproductive_ms = pending_unproductive_ms.saturating_add(ev.active_ms);
        }

        if has_input {
            last_input_ts = cur_dt;
        }
    }

    if in_streak {
        flush_streak(
            &mut streaks,
            &mut streak_start,
            &mut streak_app_ms,
            streak_productive_ms,
            streak_keys,
            config,
        );
    }

    streaks.sort_by_key(|s| std::cmp::Reverse(s.duration_ms));

    #[allow(clippy::cast_possible_wrap)] // Streak count will never approach i64::MAX
    let total_streaks = streaks.len() as i64;
    let total_streak_ms: i64 = streaks.iter().map(|s| s.duration_ms).sum();
    let avg_streak_ms = if total_streaks > 0 {
        total_streak_ms / total_streaks
    } else {
        0
    };

    let longest = streaks.first();
    let longest_ms = longest.map_or(0, |s| s.duration_ms);
    let longest_app = longest.map(|s| s.app_id.clone()).unwrap_or_default();

    streaks.truncate(5);

    Ok(StreakSummary {
        longest_productive_ms: longest_ms,
        longest_productive_app: longest_app,
        avg_productive_streak_ms: avg_streak_ms,
        total_productive_streaks: total_streaks,
        top_streaks: streaks,
    })
}

fn query_gaps(
    conn: &Connection,
    config: &Config,
    since_utc: &str,
    until_utc: &str,
    sleep: &crate::config::SleepConfig,
) -> Result<AwayData, Error> {
    let mut stmt = conn.prepare(
        "WITH ordered AS (
            SELECT 
                timestamp,
                LAG(timestamp) OVER (ORDER BY timestamp) as prev_ts
            FROM events
            WHERE timestamp >= ?1 AND timestamp < ?2
        ),
        gaps AS (
            SELECT 
                prev_ts as gap_start,
                timestamp as gap_end,
                CAST(strftime('%H', prev_ts, 'localtime') AS INT) as start_hour,
                CAST(strftime('%H', timestamp, 'localtime') AS INT) as end_hour,
                (julianday(timestamp) - julianday(prev_ts)) * 24.0 as gap_hours
            FROM ordered
            WHERE prev_ts IS NOT NULL
        )
        SELECT gap_start, gap_end, start_hour, end_hour, gap_hours
        FROM gaps
        ORDER BY gap_start",
    )?;

    // Intermediate structure for gap merging
    struct RawGap {
        gap_start: String,
        gap_end: String,
        gap_type: GapType,
        duration_ms: i64,
    }

    let mut raw_gaps: Vec<RawGap> = Vec::new();

    let rows = stmt.query_map(params![since_utc, until_utc], |row| {
        Ok((
            row.get::<_, String>(0)?, // gap_start
            row.get::<_, String>(1)?, // gap_end
            row.get::<_, u32>(2)?,    // start_hour
            row.get::<_, u32>(3)?,    // end_hour
            row.get::<_, f64>(4)?,    // gap_hours
        ))
    })?;

    // First pass: classify all gaps
    for row in rows {
        let (gap_start, gap_end, start_hour, end_hour, gap_hours) = row?;

        // Filter by configured gap range
        if !(sleep.gap_min_hours..=sleep.gap_max_hours).contains(&gap_hours) {
            continue;
        }

        let Some(gap_type) = classify_gap(start_hour, end_hour, gap_hours, sleep) else {
            continue;
        };

        let duration_ms = if gap_hours.is_finite()
            && gap_hours >= 0.0
            && gap_hours <= (i64::MAX as f64 / 3_600_000.0)
        {
            (gap_hours * 3_600_000.0) as i64
        } else {
            i64::MAX
        };

        raw_gaps.push(RawGap {
            gap_start,
            gap_end,
            gap_type,
            duration_ms,
        });
    }

    // Second pass: merge consecutive sleep gaps separated by short activity
    // If merge_window_min > 0 and two Sleep gaps are close, combine them
    let merge_window_ms = i64::from(sleep.merge_window_min).saturating_mul(60_000);
    let mut merged_gaps: Vec<RawGap> = Vec::new();

    for gap in raw_gaps {
        let should_merge = merge_window_ms > 0
            && gap.gap_type == GapType::Sleep
            && merged_gaps.last().is_some_and(|prev| {
                if prev.gap_type != GapType::Sleep {
                    return false;
                }
                // Check if time between prev.gap_end and gap.gap_start is < merge_window
                let Some(prev_end) = config.parse_timestamp_to_local(&prev.gap_end) else {
                    return false;
                };
                let Some(curr_start) = config.parse_timestamp_to_local(&gap.gap_start) else {
                    return false;
                };
                let between_ms = curr_start
                    .signed_duration_since(prev_end)
                    .num_milliseconds();
                between_ms >= 0 && between_ms < merge_window_ms
            });

        if should_merge {
            // Merge with previous sleep gap
            if let Some(prev) = merged_gaps.last_mut() {
                prev.gap_end.clone_from(&gap.gap_end);
                // Recalculate duration from merged start to new end
                if let (Some(start), Some(end)) = (
                    config.parse_timestamp_to_local(&prev.gap_start),
                    config.parse_timestamp_to_local(&gap.gap_end),
                ) {
                    prev.duration_ms = end.signed_duration_since(start).num_milliseconds();
                } else {
                    // Fallback: just add durations (less accurate but safe)
                    prev.duration_ms = prev.duration_ms.saturating_add(gap.duration_ms);
                }
            }
        } else {
            merged_gaps.push(gap);
        }
    }

    // Third pass: build entries and summaries
    let mut entries: Vec<GapEntry> = Vec::new();
    let mut sleep_total_ms: i64 = 0;
    let mut sleep_count: i64 = 0;
    let mut long_break_total_ms: i64 = 0;
    let mut long_break_count: i64 = 0;
    let mut short_break_total_ms: i64 = 0;
    let mut short_break_count: i64 = 0;

    for gap in merged_gaps {
        match gap.gap_type {
            GapType::Sleep => {
                sleep_total_ms = sleep_total_ms.saturating_add(gap.duration_ms);
                sleep_count += 1;
            }
            GapType::LongBreak => {
                long_break_total_ms = long_break_total_ms.saturating_add(gap.duration_ms);
                long_break_count += 1;
            }
            GapType::ShortBreak => {
                short_break_total_ms = short_break_total_ms.saturating_add(gap.duration_ms);
                short_break_count += 1;
            }
        }

        let start_local = config.parse_timestamp_to_local(&gap.gap_start).map_or_else(
            || gap.gap_start.clone(),
            |dt| dt.format("%m-%d %H:%M").to_string(),
        );
        let end_local = config
            .parse_timestamp_to_local(&gap.gap_end)
            .map_or_else(|| gap.gap_end.clone(), |dt| dt.format("%H:%M").to_string());

        entries.push(GapEntry {
            gap_type: gap.gap_type,
            start_time: start_local,
            end_time: end_local,
            duration_ms: gap.duration_ms,
        });
    }
    let mut summaries = Vec::new();

    if sleep_count > 0 {
        summaries.push(GapSummary {
            gap_type: GapType::Sleep,
            count: sleep_count,
            total_ms: sleep_total_ms,
            avg_ms: sleep_total_ms.checked_div(sleep_count).unwrap_or(0),
        });
    }

    if long_break_count > 0 {
        summaries.push(GapSummary {
            gap_type: GapType::LongBreak,
            count: long_break_count,
            total_ms: long_break_total_ms,
            avg_ms: long_break_total_ms
                .checked_div(long_break_count)
                .unwrap_or(0),
        });
    }

    if short_break_count > 0 {
        summaries.push(GapSummary {
            gap_type: GapType::ShortBreak,
            count: short_break_count,
            total_ms: short_break_total_ms,
            avg_ms: short_break_total_ms
                .checked_div(short_break_count)
                .unwrap_or(0),
        });
    }

    let total_away_ms = sleep_total_ms
        .saturating_add(long_break_total_ms)
        .saturating_add(short_break_total_ms);

    Ok(AwayData {
        summaries,
        total_away_ms,
        entries,
    })
}

fn query_input_metrics(
    conn: &Connection,
    since_utc: &str,
    until_utc: &str,
    total_keys: i64,
) -> Result<InputMetrics, Error> {
    let (
        backspace_count,
        modifier_count,
        left_clicks,
        right_clicks,
        middle_clicks,
        scroll_up,
        scroll_down,
        scroll_horizontal,
    ): (i64, i64, i64, i64, i64, i64, i64, i64) = conn.query_row(
        "SELECT 
            COALESCE(SUM(backspace_count), 0),
            COALESCE(SUM(modifier_count), 0),
            COALESCE(SUM(left_clicks), 0),
            COALESCE(SUM(right_clicks), 0),
            COALESCE(SUM(middle_clicks), 0),
            COALESCE(SUM(scroll_up), 0),
            COALESCE(SUM(scroll_down), 0),
            COALESCE(SUM(scroll_horizontal), 0)
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2",
        params![since_utc, until_utc],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        },
    )?;

    let backspace_rate = if total_keys > 0 {
        (backspace_count as f64 / total_keys as f64) * 100.0
    } else {
        0.0
    };

    let modifier_rate = if total_keys > 0 {
        (modifier_count as f64 / total_keys as f64) * 100.0
    } else {
        0.0
    };

    Ok(InputMetrics {
        backspace_count,
        modifier_count,
        left_clicks,
        right_clicks,
        middle_clicks,
        scroll_up,
        scroll_down,
        scroll_horizontal,
        backspace_rate,
        modifier_rate,
    })
}

fn query_flow_sessions(
    conn: &Connection,
    config: &Config,
    since_utc: &str,
    until_utc: &str,
) -> Result<FlowSummary, Error> {
    const FLOW_MIN_KEYS_PER_MIN: f64 = 50.0;
    const FLOW_MIN_DURATION_MS: i64 = 5 * 60 * 1000;

    let mut stmt = conn.prepare(
        "SELECT app_id, timestamp, active_ms, keystrokes, category
         FROM events 
         WHERE timestamp >= ?1 AND timestamp < ?2 AND keystrokes > 0
         ORDER BY timestamp",
    )?;

    struct EventRow {
        app_id: String,
        timestamp: String,
        active_ms: i64,
        keystrokes: i64,
        category: String,
    }

    let rows: Vec<EventRow> = stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok(EventRow {
                app_id: row.get(0)?,
                timestamp: row.get(1)?,
                active_ms: row.get(2)?,
                keystrokes: row.get(3)?,
                category: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut sessions: Vec<FlowSession> = Vec::new();
    let mut current_app: Option<String> = None;
    let mut session_start: Option<String> = None;
    let mut session_duration_ms: i64 = 0;
    let mut session_keys: i64 = 0;

    let try_finalize_session = |sessions: &mut Vec<FlowSession>,
                                current_app: &Option<String>,
                                session_start: &mut Option<String>,
                                session_duration_ms: i64,
                                session_keys: i64| {
        if session_duration_ms >= FLOW_MIN_DURATION_MS {
            let keys_per_min = if session_duration_ms > 0 {
                (session_keys as f64) / (session_duration_ms as f64 / 60_000.0)
            } else {
                0.0
            };
            if keys_per_min >= FLOW_MIN_KEYS_PER_MIN
                && let Some(start) = session_start.take()
            {
                let start_local = config
                    .parse_timestamp_to_local(&start)
                    .map_or_else(|| start.clone(), |dt| dt.format("%m-%d %H:%M").to_string());
                sessions.push(FlowSession {
                    app_id: current_app.clone().unwrap_or_default(),
                    start_time: start_local,
                    duration_ms: session_duration_ms,
                    keystrokes: session_keys,
                    keys_per_min,
                });
            }
        }
    };

    for row in &rows {
        if row.category != "productive" {
            try_finalize_session(
                &mut sessions,
                &current_app,
                &mut session_start,
                session_duration_ms,
                session_keys,
            );
            current_app = None;
            session_start = None;
            session_duration_ms = 0;
            session_keys = 0;
            continue;
        }

        let keys_per_min_current = if row.active_ms > 0 {
            (row.keystrokes as f64) / (row.active_ms as f64 / 60_000.0)
        } else {
            0.0
        };

        if keys_per_min_current >= FLOW_MIN_KEYS_PER_MIN {
            if current_app.as_ref() == Some(&row.app_id) || current_app.is_none() {
                if session_start.is_none() {
                    session_start = Some(row.timestamp.clone());
                    current_app = Some(row.app_id.clone());
                }
                session_duration_ms += row.active_ms;
                session_keys += row.keystrokes;
            } else {
                try_finalize_session(
                    &mut sessions,
                    &current_app,
                    &mut session_start,
                    session_duration_ms,
                    session_keys,
                );
                current_app = Some(row.app_id.clone());
                session_start = Some(row.timestamp.clone());
                session_duration_ms = row.active_ms;
                session_keys = row.keystrokes;
            }
        } else {
            try_finalize_session(
                &mut sessions,
                &current_app,
                &mut session_start,
                session_duration_ms,
                session_keys,
            );
            current_app = None;
            session_start = None;
            session_duration_ms = 0;
            session_keys = 0;
        }
    }

    try_finalize_session(
        &mut sessions,
        &current_app,
        &mut session_start,
        session_duration_ms,
        session_keys,
    );

    sessions.sort_by_key(|s| std::cmp::Reverse(s.duration_ms));

    let total_flow_ms: i64 = sessions.iter().map(|s| s.duration_ms).sum();
    let flow_sessions = i64::try_from(sessions.len()).unwrap_or(i64::MAX);
    let avg_flow_duration_ms = if flow_sessions > 0 {
        total_flow_ms / flow_sessions
    } else {
        0
    };
    let peak_keys_per_min = sessions
        .iter()
        .map(|s| s.keys_per_min)
        .fold(0.0_f64, f64::max);

    let top_sessions: Vec<FlowSession> = sessions.into_iter().take(5).collect();

    Ok(FlowSummary {
        total_flow_ms,
        flow_sessions,
        avg_flow_duration_ms,
        peak_keys_per_min,
        top_sessions,
    })
}

fn query_fatigue_indicators(
    conn: &Connection,
    config: &Config,
    since_utc: &str,
    until_utc: &str,
) -> Result<FatigueIndicators, Error> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, keystrokes, backspace_count
         FROM events 
         WHERE timestamp >= ?1 AND timestamp < ?2 AND keystrokes > 0
         ORDER BY timestamp",
    )?;

    struct Row {
        timestamp: String,
        keystrokes: i64,
        backspace_count: i64,
    }

    let rows: Vec<Row> = stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok(Row {
                timestamp: row.get(0)?,
                keystrokes: row.get(1)?,
                backspace_count: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut hourly_keys: HashMap<u32, i64> = HashMap::new();
    let mut hourly_backspaces: HashMap<u32, i64> = HashMap::new();

    for row in &rows {
        if let Some(dt) = config.parse_timestamp_to_local(&row.timestamp) {
            let hour = dt.hour();
            *hourly_keys.entry(hour).or_insert(0) += row.keystrokes;
            *hourly_backspaces.entry(hour).or_insert(0) += row.backspace_count;
        }
    }

    let mut hourly_rates: Vec<HourlyErrorRate> = Vec::new();
    for hour in 0..24 {
        let keys = hourly_keys.get(&hour).copied().unwrap_or(0);
        let backspaces = hourly_backspaces.get(&hour).copied().unwrap_or(0);
        if keys > 100 {
            let rate = (backspaces as f64 / keys as f64) * 100.0;
            hourly_rates.push(HourlyErrorRate {
                hour,
                backspace_rate: rate,
                keystrokes: keys,
            });
        }
    }

    if hourly_rates.len() < 2 {
        return Ok(FatigueIndicators {
            trend: FatigueTrend::Insufficient,
            early_error_rate: 0.0,
            late_error_rate: 0.0,
            hourly_rates,
            recommendation: None,
        });
    }

    hourly_rates.sort_by_key(|r| r.hour);

    let mid = hourly_rates.len() / 2;
    let early_rates: Vec<f64> = hourly_rates[..mid]
        .iter()
        .map(|r| r.backspace_rate)
        .collect();
    let late_rates: Vec<f64> = hourly_rates[mid..]
        .iter()
        .map(|r| r.backspace_rate)
        .collect();

    let early_avg = if early_rates.is_empty() {
        0.0
    } else {
        early_rates.iter().sum::<f64>() / early_rates.len() as f64
    };

    let late_avg = if late_rates.is_empty() {
        0.0
    } else {
        late_rates.iter().sum::<f64>() / late_rates.len() as f64
    };

    let trend = if late_avg > early_avg * 1.3 {
        FatigueTrend::Increasing
    } else if late_avg < early_avg * 0.7 {
        FatigueTrend::Decreasing
    } else {
        FatigueTrend::Stable
    };

    let recommendation = match trend {
        FatigueTrend::Increasing => {
            Some("Error rate increasing later in day. Consider more frequent breaks.".to_string())
        }
        FatigueTrend::Decreasing => {
            Some("Strong finish - error rate decreased as day progressed.".to_string())
        }
        FatigueTrend::Stable | FatigueTrend::Insufficient => None,
    };

    Ok(FatigueIndicators {
        trend,
        early_error_rate: early_avg,
        late_error_rate: late_avg,
        hourly_rates,
        recommendation,
    })
}

// ---------------------------------------------------------------------------
// Display functions
// ---------------------------------------------------------------------------

pub fn show_today(app: &App) -> Result<(), Error> {
    let data = query_today(app)?;

    println!(
        "{}\n",
        format!("Activity for today ({}):", data.date).cyan().bold()
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "Application".bold(),
        "Category".bold(),
        "Time".bold()
    );
    println!("{}", "─".repeat(54).dimmed());

    for row in &data.rows {
        let hours = row.total_ms / 3_600_000;
        let mins = (row.total_ms % 3_600_000) / 60_000;
        let secs = (row.total_ms % 60_000) / 1000;

        let time_str = format!("{:>2}h {:>2}m {:>2}s", hours, mins, secs);
        println!(
            "{:<30} {:>12} {}",
            cat_colored(row.category, &truncate(&row.app_id, 28)),
            cat_label(row.category),
            time_str.bold(),
        );
    }

    Ok(())
}

pub fn show_metrics_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let m = query_metrics_range(app, range)?;

    println!(
        "{}\n",
        format!(
            "═══ Productivity Metrics ({} day{}) ═══",
            m.days,
            if m.days == 1 { "" } else { "s" }
        )
        .cyan()
        .bold()
    );

    println!(
        "Total Time:              {}",
        fmt_duration(m.total_ms).bold()
    );
    println!(
        "Productive Time:         {} {}",
        fmt_duration(m.productive_ms).green().bold(),
        pct(m.productive_ms, m.total_ms).dimmed()
    );
    println!(
        "Unproductive Time:       {} {}",
        fmt_duration(m.unproductive_ms).red().bold(),
        pct(m.unproductive_ms, m.total_ms).dimmed()
    );
    println!(
        "Undefined Time:          {} {}",
        fmt_duration(m.neutral_ms).yellow().bold(),
        pct(m.neutral_ms, m.total_ms).dimmed()
    );
    println!();
    println!(
        "Productive Active:       {} {}",
        fmt_duration(m.productive_active_ms).green(),
        pct(m.productive_active_ms, m.productive_ms).dimmed()
    );
    println!(
        "Productive Passive:      {} {}",
        fmt_duration(m.productive_passive_ms).yellow(),
        pct(m.productive_passive_ms, m.productive_ms).dimmed()
    );
    println!(
        "Productive Idle:         {} {}",
        fmt_duration(m.productive_idle_ms).dimmed(),
        pct(m.productive_idle_ms, m.productive_ms).dimmed()
    );

    Ok(())
}

pub fn show_timeline(app: &App, days_back: u32, bucket_min: u32) -> Result<(), Error> {
    let data = query_timeline(app, days_back, bucket_min)?;

    if data.buckets.is_empty() {
        println!("No activity recorded for {}", data.date);
        return Ok(());
    }

    println!(
        "{}\n",
        format!(
            "═══ Timeline for {} ({}min buckets) ═══",
            data.date, data.bucket_min
        )
        .cyan()
        .bold()
    );

    let bar_width: usize = 20;

    for b in &data.buckets {
        let total = b
            .productive_ms
            .saturating_add(b.neutral_ms)
            .saturating_add(b.unproductive_ms);
        if total == 0 {
            continue;
        }

        let idle_pct = if total > 0 {
            b.idle_ms as f64 / total as f64
        } else {
            0.0
        };

        let prod_frac = if total > 0 {
            b.productive_ms as f64 / total as f64
        } else {
            0.0
        };
        let neutral_frac = if total > 0 {
            b.neutral_ms as f64 / total as f64
        } else {
            0.0
        };
        let unprod_frac = if total > 0 {
            b.unproductive_ms as f64 / total as f64
        } else {
            0.0
        };

        let bar = colored_bar(prod_frac, neutral_frac, unprod_frac, bar_width);

        let idle_marker = if idle_pct > 0.8 {
            format!(" {}", "[AFK]".red().bold())
        } else if idle_pct > 0.5 {
            format!(" {}", "[mostly idle]".yellow())
        } else {
            String::new()
        };

        println!(
            "{} {} {:>6} {:>4} keys  {:<20}{}",
            format!("{:02}:{:02}", b.hour, b.minute).dimmed(),
            bar,
            fmt_duration_compact(total),
            b.keystrokes,
            truncate(&b.dominant_app, 20),
            idle_marker,
        );
    }

    println!(
        "\n  {} productive  {} neutral  {} unproductive",
        "█".green(),
        "█".yellow(),
        "█".red()
    );

    Ok(())
}

pub fn generate_report_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let data = query_report_range(app, range)?;

    println!(
        "{}",
        "╔══════════════════════════════════════════════════════╗"
            .cyan()
            .bold()
    );
    println!(
        "{}{}{}",
        "║".cyan().bold(),
        "           ACTIVITY REPORT                           "
            .white()
            .bold(),
        "║".cyan().bold()
    );
    let period_line = format!("  Period: {} → {}       ", data.since_str, data.now_str);
    println!("{}{}{}", "║".cyan().bold(), period_line, "║".cyan().bold());
    println!(
        "{}\n",
        "╚══════════════════════════════════════════════════════╝"
            .cyan()
            .bold()
    );

    if data.total_events == 0 {
        println!("No activity recorded for this period.");
        return Ok(());
    }

    println!(
        "{}",
        section_header("── Overview ──────────────────────────────────────────")
    );
    println!(
        "  Total Time:         {}",
        fmt_duration(data.total_ms).bold()
    );
    println!(
        "  Active:             {} {}",
        fmt_duration(data.active_ms).green().bold(),
        pct(data.active_ms, data.total_ms).dimmed()
    );
    println!(
        "  Passive:            {} {}",
        fmt_duration(data.passive_ms).yellow(),
        pct(data.passive_ms, data.total_ms).dimmed()
    );
    println!(
        "  Idle/AFK:           {} {}",
        fmt_duration(data.idle_ms).dimmed(),
        pct(data.idle_ms, data.total_ms).dimmed()
    );
    println!("  Focus Switches:     {}", data.total_events);
    println!("  Keystrokes:         {}", data.total_keys);
    println!("  Mouse Clicks:       {}", data.total_clicks);
    println!("  Scroll Events:      {}", data.total_scroll);
    println!(
        "  Mouse Travel:       {}",
        fmt_distance(data.total_distance, app.config.mouse_dpi)
    );
    if data.jiggler_count > 0 {
        println!(
            "  Jiggler Events:     {}",
            format!("{} (artificial input detected)", data.jiggler_count)
                .red()
                .bold()
        );
    }

    if app.config.goals.enabled {
        let productive_ms = data
            .categories
            .iter()
            .find(|c| c.category == Category::Productive)
            .map_or(0, |c| c.total_ms);

        println!(
            "\n{}",
            section_header("── Goals ─────────────────────────────────────────────")
        );

        if let Some(daily_goal) = app.config.goals.daily_ms() {
            #[allow(clippy::cast_possible_wrap)] // Daily count is small
            let days = (data.daily.len() as i64).max(1);
            let daily_avg = productive_ms.checked_div(days).unwrap_or(0);
            let daily_pct = if daily_goal > 0 {
                (daily_avg as f64 / daily_goal as f64 * 100.0).min(999.0)
            } else {
                0.0
            };
            let bar_len = ((daily_pct / 5.0).round() as usize).min(20);
            let progress_bar = "█".repeat(bar_len);
            let remaining_bar = "░".repeat(20_usize.saturating_sub(bar_len));

            let status = if daily_pct >= 100.0 {
                format!("{}%", daily_pct.round()).green().bold().to_string()
            } else if daily_pct >= 75.0 {
                format!("{}%", daily_pct.round()).yellow().to_string()
            } else {
                format!("{}%", daily_pct.round()).red().to_string()
            };

            println!(
                "  Daily Goal:         {}{} {} (avg {} / {})",
                progress_bar.green(),
                remaining_bar.dimmed(),
                status,
                fmt_duration(daily_avg).bold(),
                app.config.goals.daily,
            );
        }

        if let Some(weekly_goal) = app.config.goals.weekly_ms() {
            let weekly_pct = if weekly_goal > 0 {
                (productive_ms as f64 / weekly_goal as f64 * 100.0).min(999.0)
            } else {
                0.0
            };
            let bar_len = ((weekly_pct / 5.0).round() as usize).min(20);
            let progress_bar = "█".repeat(bar_len);
            let remaining_bar = "░".repeat(20_usize.saturating_sub(bar_len));

            let status = if weekly_pct >= 100.0 {
                format!("{}%", weekly_pct.round())
                    .green()
                    .bold()
                    .to_string()
            } else if weekly_pct >= 75.0 {
                format!("{}%", weekly_pct.round()).yellow().to_string()
            } else {
                format!("{}%", weekly_pct.round()).red().to_string()
            };

            println!(
                "  Weekly Goal:        {}{} {} ({} / {})",
                progress_bar.green(),
                remaining_bar.dimmed(),
                status,
                fmt_duration(productive_ms).bold(),
                app.config.goals.weekly,
            );
        }
    }

    println!(
        "\n{}",
        section_header("── Productivity ──────────────────────────────────────")
    );

    for cat in &data.categories {
        let icon = match cat.category {
            Category::Productive => "●".green().to_string(),
            Category::Unproductive => "○".red().to_string(),
            Category::Neutral => "◌".yellow().to_string(),
        };
        let filled = if data.total_ms > 0 {
            (cat.total_ms as f64 / data.total_ms as f64 * 30.0).round() as usize
        } else {
            0
        };
        let bar = cat_bar(cat.category, filled);
        println!(
            "  {} {:<14} {} {} {}",
            icon,
            cat_label(cat.category),
            bar,
            fmt_duration(cat.total_ms).bold(),
            format!("(active: {})", pct(cat.active_ms, cat.total_ms)).dimmed(),
        );
    }

    println!(
        "\n{}",
        section_header("── Top Applications ──────────────────────────────────")
    );

    const BAR_WIDTH: usize = 20;
    let max_app_ms = data.top_apps.first().map_or(1, |g| g.total_ms).max(1);

    for group in &data.top_apps {
        if group.children.len() == 1 {
            let row = &group.children[0];
            let frac_blocks =
                (row.total_ms as f64 / max_app_ms as f64 * BAR_WIDTH as f64).max(0.125);
            let bar = cat_bar_fractional(row.category, frac_blocks, BAR_WIDTH);
            let active_min = row.active_ms as f64 / 60_000.0;
            let keys_per_min = if active_min > 0.5 {
                format!("{:.0}/m", row.keys as f64 / active_min)
            } else {
                "-".to_string()
            };
            let name = format!("{:<22}", truncate(&row.app_id, 22));
            println!(
                "  {} {} {:>8}  {:>5} keys ({:>5}) {:>3} clicks  ({})",
                cat_colored(row.category, &name),
                bar,
                fmt_duration(row.total_ms).bold(),
                row.keys,
                keys_per_min.dimmed(),
                row.clicks,
                cat_label(row.category),
            );
        } else {
            let filled =
                (group.total_ms as f64 / max_app_ms as f64 * BAR_WIDTH as f64).round() as usize;
            let filled = filled.max(1);
            let active_min = group.active_ms as f64 / 60_000.0;
            let keys_per_min = if active_min > 0.5 {
                format!("{:.0}/m", group.keys as f64 / active_min)
            } else {
                "-".to_string()
            };

            let mut prod_ms: i64 = 0;
            let mut neutral_ms: i64 = 0;
            let mut unprod_ms: i64 = 0;
            for child in &group.children {
                match child.category {
                    Category::Productive => prod_ms = prod_ms.saturating_add(child.total_ms),
                    Category::Neutral => neutral_ms = neutral_ms.saturating_add(child.total_ms),
                    Category::Unproductive => unprod_ms = unprod_ms.saturating_add(child.total_ms),
                }
            }
            let (prod_frac, neutral_frac, unprod_frac) = if group.total_ms > 0 {
                (
                    prod_ms as f64 / group.total_ms as f64,
                    neutral_ms as f64 / group.total_ms as f64,
                    unprod_ms as f64 / group.total_ms as f64,
                )
            } else {
                (0.0, 0.0, 0.0)
            };
            let bar = format!(
                "{}{}",
                colored_bar(prod_frac, neutral_frac, unprod_frac, filled),
                " ".repeat(BAR_WIDTH.saturating_sub(filled))
            );

            let name = format!("{:<22}", truncate(&group.app_id, 22));
            println!(
                "  {} {} {:>8}  {:>5} keys ({:>5}) {:>3} clicks",
                name.bold(),
                bar,
                fmt_duration(group.total_ms).bold(),
                group.keys,
                keys_per_min.dimmed(),
                group.clicks,
            );

            let last_idx = group.children.len().saturating_sub(1);
            for (i, child) in group.children.iter().enumerate() {
                let connector = if i == last_idx { "└─" } else { "├─" };
                let active_min = child.active_ms as f64 / 60_000.0;
                let keys_per_min = if active_min > 0.5 {
                    format!("{:.0}/m", child.keys as f64 / active_min)
                } else {
                    "-".to_string()
                };
                let label_raw = match child.category {
                    Category::Productive => "productive",
                    Category::Unproductive => "unproductive",
                    Category::Neutral => "neutral",
                };
                // Sub layout: "    {conn} {label:<name_col} {bar_pad} {:>8}"
                // Parent: "  " (2) + name (22) + " " (1) + bar (20) + " " (1) + dur (8) = 54
                // Sub:    "    " (4) + conn (2) + " " (1) + label + pad + dur = 54
                // label sits in the name column; remaining space pads through bar area
                let dur = fmt_duration(child.total_ms);
                let prefix = 4 + 2 + 1;
                let dur_len = dur.len();
                let gap = 54_usize.saturating_sub(prefix + label_raw.len() + dur_len);
                let label_colored = cat_colored(child.category, label_raw);
                println!(
                    "    {} {}{}{}  {:>5} keys ({:>5}) {:>3} clicks",
                    connector.dimmed(),
                    label_colored,
                    " ".repeat(gap),
                    dur.bold(),
                    child.keys,
                    keys_per_min.dimmed(),
                    child.clicks,
                );
            }
        }
    }

    println!(
        "\n{}",
        section_header("── Daily Breakdown ───────────────────────────────────")
    );

    for d in &data.daily {
        let active_pct_val = if d.total_ms > 0 {
            d.active_ms as f64 / d.total_ms as f64 * 100.0
        } else {
            0.0
        };
        let active_str = pct(d.active_ms, d.total_ms);
        let active_colored = if active_pct_val >= 60.0 {
            active_str.green().to_string()
        } else if active_pct_val >= 30.0 {
            active_str.yellow().to_string()
        } else {
            active_str.red().to_string()
        };
        println!(
            "  {}  {:>8}  active: {}  {:>5} keys  {} switches",
            d.date.dimmed(),
            fmt_duration(d.total_ms).bold(),
            active_colored,
            d.keystrokes,
            d.switches,
        );
    }

    if let Some(sched) = &data.schedule {
        println!(
            "\n{}",
            section_header("── Schedule ──────────────────────────────────────────")
        );

        println!(
            "  {:<27} {:>8}  active: {:>6}  {:>5} keys",
            sched.work_label.green(),
            fmt_duration(sched.work_total_ms).bold(),
            pct(sched.work_active_ms, sched.work_total_ms).green(),
            sched.work_keys,
        );
        println!(
            "  {:<27} {:>8}  active: {:>6}  {:>5} keys",
            "After Hours:".yellow(),
            fmt_duration(sched.after_total_ms).bold(),
            pct(sched.after_active_ms, sched.after_total_ms).yellow(),
            sched.after_keys,
        );
    }

    if let Some(away) = &data.away
        && !away.summaries.is_empty()
    {
        println!(
            "\n{}",
            section_header("── Away Time ─────────────────────────────────────────")
        );

        // Visible col layout: "  X label:  " padded to col 20, then right-aligned
        // duration Using text symbols (1-col wide) to avoid emoji width
        // inconsistencies
        let col_target: usize = 20;
        for summary in &away.summaries {
            let (icon, label_colored, visible_cols) = match summary.gap_type {
                GapType::Sleep => ("◑".blue().to_string(), "Sleep:".blue().to_string(), 8),
                GapType::LongBreak => (
                    "◐".yellow().to_string(),
                    "Long Break:".yellow().to_string(),
                    13,
                ),
                GapType::ShortBreak => (
                    "◌".dimmed().to_string(),
                    "Short Break:".dimmed().to_string(),
                    14,
                ),
            };
            let total_str = fmt_duration(summary.total_ms);
            let avg_str = fmt_duration(summary.avg_ms);
            let pad = col_target.saturating_sub(visible_cols);

            println!(
                "  {} {}{}{:>8} total  ({} × {} avg)",
                icon,
                label_colored,
                " ".repeat(pad),
                total_str.bold(),
                summary.count,
                avg_str.dimmed(),
            );
        }

        let pad = col_target.saturating_sub(13) + 1;
        println!(
            "  {}{}{:>8}",
            "Total Away:".dimmed(),
            " ".repeat(pad),
            fmt_duration(away.total_away_ms).dimmed(),
        );
    }

    println!(
        "\n{}",
        section_header("── Peak Hours ────────────────────────────────────────")
    );

    let max_hour_ms = data.peak_hours.first().map_or(1, |h| h.total_ms);
    for h in &data.peak_hours {
        let bar_len = (h.total_ms as f64 / max_hour_ms as f64 * 20.0).round() as usize;
        println!(
            "  {}  {} {:>8}  {:>5} keys",
            format!("{:02}:00", h.hour).dimmed(),
            "█".repeat(bar_len).cyan(),
            fmt_duration(h.total_ms).bold(),
            h.keystrokes,
        );
    }

    if let Some(streaks) = &data.streaks
        && streaks.total_productive_streaks > 0
    {
        println!(
            "\n{}",
            section_header("── Focus Streaks ─────────────────────────────────────")
        );

        println!(
            "  Longest Streak:     {} in {}",
            fmt_duration(streaks.longest_productive_ms).green().bold(),
            streaks.longest_productive_app.bold()
        );
        println!(
            "  Average Streak:     {}",
            fmt_duration(streaks.avg_productive_streak_ms)
        );
        println!(
            "  Total Streaks:      {} (5+ min productive sessions)",
            streaks.total_productive_streaks
        );

        if !streaks.top_streaks.is_empty() {
            println!();
            for (i, streak) in streaks.top_streaks.iter().enumerate() {
                let rank = format!("{}.", i + 1);
                println!(
                    "  {:>3} {:>8}  {}  {} keys  {}",
                    rank.dimmed(),
                    fmt_duration(streak.duration_ms).green(),
                    streak.start_time.dimmed(),
                    streak.keystrokes,
                    truncate(&streak.app_id, 20),
                );
            }
        }
    }

    if let Some(input) = &data.input_metrics
        && (input.backspace_count > 0
            || input.modifier_count > 0
            || input.left_clicks > 0
            || input.scroll_up > 0)
    {
        println!(
            "\n{}",
            section_header("── Input Metrics ─────────────────────────────────────")
        );

        println!(
            "  Backspace/Delete:   {} ({:.2}% of keystrokes)",
            input.backspace_count.to_string().bold(),
            input.backspace_rate
        );
        println!(
            "  Modifier Keys:      {} ({:.2}% of keystrokes)",
            input.modifier_count.to_string().bold(),
            input.modifier_rate
        );
        println!();
        println!(
            "  Mouse Clicks:       {} left, {} right, {} middle",
            input.left_clicks.to_string().bold(),
            input.right_clicks,
            input.middle_clicks
        );
        println!(
            "  Scroll Events:      {} up, {} down, {} horizontal",
            input.scroll_up.to_string().bold(),
            input.scroll_down,
            input.scroll_horizontal
        );
    }

    if let Some(flow) = &data.flow
        && flow.flow_sessions > 0
    {
        println!(
            "\n{}",
            section_header("── Flow State ────────────────────────────────────────")
        );

        println!(
            "  Total Flow Time:    {}",
            fmt_duration(flow.total_flow_ms).green().bold()
        );
        println!(
            "  Flow Sessions:      {} (avg {})",
            flow.flow_sessions,
            fmt_duration(flow.avg_flow_duration_ms)
        );
        println!(
            "  Peak Intensity:     {:.0} keys/min",
            flow.peak_keys_per_min
        );

        if !flow.top_sessions.is_empty() {
            println!();
            for (i, session) in flow.top_sessions.iter().enumerate() {
                let rank = format!("{}.", i + 1);
                println!(
                    "  {:>3} {:>8}  {}  {:.0} keys/min  {}",
                    rank.dimmed(),
                    fmt_duration(session.duration_ms).green(),
                    session.start_time.dimmed(),
                    session.keys_per_min,
                    truncate(&session.app_id, 20),
                );
            }
        }
    }

    if let Some(fatigue) = &data.fatigue
        && fatigue.trend != FatigueTrend::Insufficient
    {
        println!(
            "\n{}",
            section_header("── Fatigue Indicators ────────────────────────────────")
        );

        let trend_str = match fatigue.trend {
            FatigueTrend::Increasing => "↑ Increasing".red().to_string(),
            FatigueTrend::Stable => "→ Stable".green().to_string(),
            FatigueTrend::Decreasing => "↓ Decreasing".green().bold().to_string(),
            FatigueTrend::Insufficient => "Insufficient data".dimmed().to_string(),
        };
        println!("  Error Rate Trend:   {}", trend_str);
        println!(
            "  Early Session:      {:.2}% backspace rate",
            fatigue.early_error_rate
        );
        println!(
            "  Late Session:       {:.2}% backspace rate",
            fatigue.late_error_rate
        );

        if let Some(rec) = &fatigue.recommendation {
            println!();
            println!("  💡 {}", rec.cyan());
        }
    }

    println!();

    Ok(())
}

pub fn show_comparison(app: &App, range: TimeRange) -> Result<(), Error> {
    let current_bounds = range.resolve(&app.config)?;
    let current_days = (current_bounds.end_date - current_bounds.start_date)
        .num_days()
        .saturating_add(1);

    // Calculate previous period
    let prev_end = current_bounds.start_date - chrono::Duration::days(1);
    let prev_start = prev_end - chrono::Duration::days(current_days - 1);
    let prev_range = TimeRange::DateRange(prev_start, prev_end);

    let current = query_metrics_range(app, range)?;
    let previous = query_metrics_range(app, prev_range)?;

    println!(
        "{}",
        "╔══════════════════════════════════════════════════════╗"
            .cyan()
            .bold()
    );
    println!(
        "{}{}{}",
        "║".cyan().bold(),
        "          PERIOD COMPARISON                           "
            .white()
            .bold(),
        "║".cyan().bold()
    );
    println!(
        "{}\n",
        "╚══════════════════════════════════════════════════════╝"
            .cyan()
            .bold()
    );

    println!(
        "  Current:  {} → {}",
        current_bounds.since_str.green(),
        current_bounds.now_str.green()
    );
    println!(
        "  Previous: {} → {}\n",
        format!("{} 00:00", prev_start).dimmed(),
        format!("{} 23:59", prev_end).dimmed()
    );

    fn delta_str(current: i64, previous: i64) -> String {
        if previous == 0 {
            if current > 0 {
                return "+∞".green().bold().to_string();
            }
            return "—".dimmed().to_string();
        }
        let pct = ((current as f64 - previous as f64) / previous as f64) * 100.0;
        if pct > 0.0 {
            format!("+{:.1}%", pct).green().to_string()
        } else if pct < 0.0 {
            format!("{:.1}%", pct).red().to_string()
        } else {
            "0%".dimmed().to_string()
        }
    }

    fn delta_str_inverse(current: i64, previous: i64) -> String {
        // For unproductive time, less is better (green = decrease)
        if previous == 0 {
            if current > 0 {
                return "+∞".red().bold().to_string();
            }
            return "—".dimmed().to_string();
        }
        let pct = ((current as f64 - previous as f64) / previous as f64) * 100.0;
        if pct > 0.0 {
            format!("+{:.1}%", pct).red().to_string()
        } else if pct < 0.0 {
            format!("{:.1}%", pct).green().to_string()
        } else {
            "0%".dimmed().to_string()
        }
    }

    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Metric".bold(),
        "Current".bold(),
        "Previous".bold(),
        "Change".bold()
    );
    println!("{}", "─".repeat(60).dimmed());

    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Total Time",
        fmt_duration(current.total_ms),
        fmt_duration(previous.total_ms),
        delta_str(current.total_ms, previous.total_ms)
    );

    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Productive".green(),
        fmt_duration(current.productive_ms).green(),
        fmt_duration(previous.productive_ms),
        delta_str(current.productive_ms, previous.productive_ms)
    );

    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Unproductive".red(),
        fmt_duration(current.unproductive_ms).red(),
        fmt_duration(previous.unproductive_ms),
        delta_str_inverse(current.unproductive_ms, previous.unproductive_ms)
    );

    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Neutral".yellow(),
        fmt_duration(current.neutral_ms).yellow(),
        fmt_duration(previous.neutral_ms),
        delta_str(current.neutral_ms, previous.neutral_ms)
    );

    // Calculate productivity ratios
    let current_ratio = if current.total_ms > 0 {
        (current.productive_ms as f64 / current.total_ms as f64 * 100.0).round() as i64
    } else {
        0
    };
    let previous_ratio = if previous.total_ms > 0 {
        (previous.productive_ms as f64 / previous.total_ms as f64 * 100.0).round() as i64
    } else {
        0
    };

    println!("{}", "─".repeat(60).dimmed());
    println!(
        "{:24} {:>11}% {:>11}% {:>10}",
        "Productivity Ratio".bold(),
        current_ratio,
        previous_ratio,
        delta_str(current_ratio, previous_ratio)
    );

    // Daily averages
    let current_daily_avg = current.productive_ms / current.days.max(1) as i64;
    let previous_daily_avg = previous.productive_ms / previous.days.max(1) as i64;

    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Daily Avg (productive)",
        fmt_duration(current_daily_avg),
        fmt_duration(previous_daily_avg),
        delta_str(current_daily_avg, previous_daily_avg)
    );

    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Export functions
// ---------------------------------------------------------------------------

pub fn export_csv_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let bounds = range.resolve(&app.config)?;
    let since_local = bounds.start_date;
    let today_local = bounds.end_date;

    println!(
        "Date,Screen Time (h:mm:ss),Productive (h:mm:ss),Unproductive (h:mm:ss),\
         Undefined (h:mm:ss),ProdActive (h:mm:ss),ProdPassive (h:mm:ss),ProdIdle (h:mm:ss),\
         Productive Ratio,Productive Active %"
    );

    let mut date = since_local;
    let mut csv_iterations = 0u32;
    while date <= today_local {
        csv_iterations = csv_iterations.saturating_add(1);
        if csv_iterations > 10_000 {
            break; // safety bound
        }
        let day_start = config_day_start_utc(&app.config, date)?;
        let day_end = config_day_end_utc(&app.config, date)?;

        let mut stmt = app.conn.prepare(
            "SELECT category, SUM(active_ms), COALESCE(SUM(passive_ms),0), SUM(idle_ms)
             FROM events
             WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY category",
        )?;

        let mut m = Metrics::default();

        let rows = stmt.query_map(params![&day_start, &day_end], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        for row in rows {
            let (category_raw, active_ms, passive_ms, idle_ms) = row?;
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
            "{},{},{},{},{},{},{},{},{},{}",
            date,
            fmt_hms(m.total_ms),
            fmt_hms(m.productive_ms),
            fmt_hms(m.unproductive_ms),
            fmt_hms(m.neutral_ms),
            fmt_hms(m.productive_active_ms),
            fmt_hms(m.productive_passive_ms),
            fmt_hms(m.productive_idle_ms),
            prod_ratio,
            prod_active_pct,
        );

        date += chrono::Duration::days(1);
    }

    Ok(())
}

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

pub fn export_heatmap_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let bounds = range.resolve(&app.config)?;
    let until_utc = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);

    let mut stmt = app.conn.prepare(
        "SELECT timestamp, category, active_ms + COALESCE(passive_ms,0) + idle_ms as total_ms, keystrokes
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2",
    )?;

    let mut heatmap: std::collections::BTreeMap<(String, u32), HeatmapCell> =
        std::collections::BTreeMap::new();

    let rows = stmt.query_map(params![&bounds.since_utc, until_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    for row in rows {
        let (timestamp, category_raw, total_ms, keystrokes) = row?;
        let Some(local_dt) = app.config.parse_timestamp_to_local(&timestamp) else {
            continue;
        };

        let date = local_dt.format("%Y-%m-%d").to_string();
        let hour = local_dt.hour();

        // Avoid double clone: check if key exists first, only clone for insert
        if let Some(cell) = heatmap.get_mut(&(date.clone(), hour)) {
            match Category::from_str(&category_raw).unwrap_or(Category::Neutral) {
                Category::Productive => {
                    cell.productive_ms = cell.productive_ms.saturating_add(total_ms);
                }
                Category::Unproductive => {
                    cell.unproductive_ms = cell.unproductive_ms.saturating_add(total_ms);
                }
                Category::Neutral => cell.neutral_ms = cell.neutral_ms.saturating_add(total_ms),
            }
            cell.total_ms = cell.total_ms.saturating_add(total_ms);
            cell.keystrokes = cell.keystrokes.saturating_add(keystrokes);
        } else {
            let mut cell = HeatmapCell {
                date: date.clone(),
                hour,
                productive_ms: 0,
                unproductive_ms: 0,
                neutral_ms: 0,
                total_ms,
                keystrokes,
            };
            match Category::from_str(&category_raw).unwrap_or(Category::Neutral) {
                Category::Productive => cell.productive_ms = total_ms,
                Category::Unproductive => cell.unproductive_ms = total_ms,
                Category::Neutral => cell.neutral_ms = total_ms,
            }
            heatmap.insert((date, hour), cell);
        }
    }

    let cells: Vec<HeatmapCell> = heatmap.into_values().collect();
    let json = serde_json::to_string_pretty(&cells)
        .map_err(|e| Error::NiriError(format!("JSON serialization failed: {}", e)))?;
    println!("{}", json);
    Ok(())
}
