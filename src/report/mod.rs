//! Report generation and export module.

mod browser;
mod display;
mod excel;
mod export;
mod interval;
mod query;
mod types;

pub use browser::{show_activity, show_engagement, show_referrers};
use chrono::{Datelike, NaiveDate};
pub use display::{
    generate_report_range, show_comparison, show_metrics_range, show_timeline, show_today,
};
pub use excel::export_xlsx_range;
pub use export::{export_cron_summary, export_csv_range, export_heatmap_range, export_json_range};
pub use query::{query_metrics_range, query_report_range, query_timeline, query_today};
use rusqlite::Connection;
pub use types::{MetricsData, ReportData, TimelineData, TodayData};

use crate::config::{Config, get_data_dir, load_config};
use crate::db::{reclassify_all, run_migrations};
use crate::error::Error;

const UNTIL_SENTINEL: &str = "9999-12-31T23:59:59+00:00";
const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_MIN: i64 = 60_000;
const MIN_STREAK_MS: i64 = 300_000;

pub struct App {
    pub config: Config,
    pub conn: Connection,
}

impl App {
    pub fn open() -> Result<App, Error> {
        let mut config = load_config()?;
        // Must precede reclassify_all, which rewrites every stored category.
        config.load_browser_history();
        let db_path = get_data_dir()?.join("activity.db");
        let mut conn = Connection::open(&db_path)?;
        run_migrations(&mut conn, &config)?;
        reclassify_all(&mut conn, &config)?;
        Ok(App { config, conn })
    }
}

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

pub struct TimeBounds {
    pub since_utc: String,
    pub until_utc: Option<String>,
    pub since_str: String,
    pub now_str: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
}

impl TimeBounds {
    fn open(
        config: &Config,
        start: NaiveDate,
        end: NaiveDate,
        now_str: String,
    ) -> Result<Self, Error> {
        Ok(Self {
            since_utc: day_start_utc(config, start)?,
            until_utc: None,
            since_str: format!("{} 00:00", start),
            now_str,
            start_date: start,
            end_date: end,
        })
    }
    fn closed(config: &Config, start: NaiveDate, end: NaiveDate) -> Result<Self, Error> {
        Ok(Self {
            since_utc: day_start_utc(config, start)?,
            until_utc: Some(day_end_utc(config, end)?),
            since_str: format!("{} 00:00", start),
            now_str: format!("{} 23:59", end),
            start_date: start,
            end_date: end,
        })
    }
}

impl TimeRange {
    pub fn resolve(&self, config: &Config) -> Result<TimeBounds, Error> {
        let now = config.local_now();
        let today = now.date_naive();
        let now_str = now.format("%Y-%m-%d %H:%M").to_string();
        match self {
            TimeRange::Days(d) => TimeBounds::open(
                config,
                today - chrono::Duration::days(*d as i64),
                today,
                now_str,
            ),
            TimeRange::DaysAligned(d) => {
                let e = today - chrono::Duration::days(1);
                TimeBounds::closed(config, e - chrono::Duration::days(*d as i64 - 1), e)
            }
            TimeRange::LastWeek => {
                // Week runs Saturday 00:00:00 → Friday 23:59:59 (Josh's reporting schedule)
                let days_since_saturday = (today.weekday().num_days_from_monday() as i64 + 2) % 7;
                let this_saturday = today - chrono::Duration::days(days_since_saturday);
                let last_saturday = this_saturday - chrono::Duration::days(7);
                TimeBounds::closed(
                    config,
                    last_saturday,
                    last_saturday + chrono::Duration::days(6),
                )
            }
            TimeRange::ThisWeek => {
                // Week runs Saturday 00:00:00 → Friday 23:59:59 (Josh's reporting schedule)
                let days_since_saturday = (today.weekday().num_days_from_monday() as i64 + 2) % 7;
                let this_saturday = today - chrono::Duration::days(days_since_saturday);
                TimeBounds::open(config, this_saturday, today, now_str)
            }
            TimeRange::LastMonth => {
                let ft = today
                    .with_day(1)
                    .ok_or_else(|| Error::NiriError("invalid date".into()))?;
                let e = ft - chrono::Duration::days(1);
                let s = e
                    .with_day(1)
                    .ok_or_else(|| Error::NiriError("invalid date".into()))?;
                TimeBounds::closed(config, s, e)
            }
            TimeRange::ThisMonth => {
                let s = today
                    .with_day(1)
                    .ok_or_else(|| Error::NiriError("invalid date".into()))?;
                TimeBounds::open(config, s, today, now_str)
            }
            TimeRange::Yesterday => {
                let d = today - chrono::Duration::days(1);
                TimeBounds::closed(config, d, d)
            }
            TimeRange::DateRange(s, e) => TimeBounds::closed(config, *s, *e),
        }
    }
}

pub(super) fn day_start_utc(cfg: &Config, d: NaiveDate) -> Result<String, Error> {
    cfg.day_start_utc(d)
        .map(|dt| dt.to_rfc3339())
        .ok_or_else(|| Error::NiriError("day start is not representable".into()))
}

pub(super) fn day_end_utc(cfg: &Config, d: NaiveDate) -> Result<String, Error> {
    cfg.day_end_utc(d)
        .map(|dt| dt.to_rfc3339())
        .ok_or_else(|| Error::NiriError("day end is not representable".into()))
}
