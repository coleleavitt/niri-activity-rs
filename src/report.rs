use std::collections::HashMap;
use std::str::FromStr;

use chrono::{Datelike, Local, LocalResult, NaiveDate, Timelike, Utc};
use owo_colors::OwoColorize;
use rusqlite::{params, Connection};
use rust_xlsxwriter::{Format, Workbook};
use serde::Serialize;

use crate::config::{get_data_dir, load_config, Category, Config};
use crate::db::{reclassify_all, run_migrations};
use crate::error::Error;
use crate::fmt::{
    cat_bar, cat_bar_fractional, cat_colored, cat_label, colored_bar, fmt_distance, fmt_duration,
    fmt_duration_compact, fmt_hms, pct, section_header, truncate,
};

pub struct App {
    pub config: Config,
    pub conn: Connection,
}

impl App {
    pub fn open() -> Result<App, Error> {
        let config = load_config()?;
        let db_path = get_data_dir()?.join("activity.db");
        let conn = Connection::open(&db_path)?;
        run_migrations(&conn, &config)?;
        reclassify_all(&conn, &config)?;
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
    /// Resolve the time range to concrete UTC timestamps
    pub fn resolve(&self) -> Result<TimeBounds, Error> {
        let now = Local::now();
        let today = now.date_naive();

        match self {
            TimeRange::Days(days) => {
                let since = now - chrono::Duration::days(*days as i64);
                let since_date = since.date_naive();
                let since_utc = local_day_start_utc(since_date)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: None,
                    since_str: since.format("%Y-%m-%d %H:%M").to_string(),
                    now_str: now.format("%Y-%m-%d %H:%M").to_string(),
                    start_date: since_date,
                    end_date: today,
                })
            }
            TimeRange::DaysAligned(days) => {
                // End at yesterday 23:59:59, go back N full days
                let end_date = today - chrono::Duration::days(1);
                let start_date = end_date - chrono::Duration::days(*days as i64 - 1);
                let since_utc = local_day_start_utc(start_date)?;
                let until_utc = local_day_end_utc(end_date)?;
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
                // Find last Monday (start of last week)
                let days_since_monday = today.weekday().num_days_from_monday();
                let this_monday = today - chrono::Duration::days(days_since_monday as i64);
                let last_monday = this_monday - chrono::Duration::days(7);
                let last_sunday = last_monday + chrono::Duration::days(6);
                let since_utc = local_day_start_utc(last_monday)?;
                let until_utc = local_day_end_utc(last_sunday)?;
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
                // This Monday 00:00 → now
                let days_since_monday = today.weekday().num_days_from_monday();
                let this_monday = today - chrono::Duration::days(days_since_monday as i64);
                let since_utc = local_day_start_utc(this_monday)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: None,
                    since_str: format!("{} 00:00", this_monday),
                    now_str: now.format("%Y-%m-%d %H:%M").to_string(),
                    start_date: this_monday,
                    end_date: today,
                })
            }
            TimeRange::LastMonth => {
                // First day of last month → last day of last month
                let first_of_this_month = today.with_day(1).ok_or_else(|| {
                    Error::NiriError("invalid date calculation".into())
                })?;
                let last_day_prev_month = first_of_this_month - chrono::Duration::days(1);
                let first_of_last_month = last_day_prev_month.with_day(1).ok_or_else(|| {
                    Error::NiriError("invalid date calculation".into())
                })?;
                let since_utc = local_day_start_utc(first_of_last_month)?;
                let until_utc = local_day_end_utc(last_day_prev_month)?;
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
                let first_of_month = today.with_day(1).ok_or_else(|| {
                    Error::NiriError("invalid date calculation".into())
                })?;
                let since_utc = local_day_start_utc(first_of_month)?;
                Ok(TimeBounds {
                    since_utc,
                    until_utc: None,
                    since_str: format!("{} 00:00", first_of_month),
                    now_str: now.format("%Y-%m-%d %H:%M").to_string(),
                    start_date: first_of_month,
                    end_date: today,
                })
            }
            TimeRange::Yesterday => {
                let yesterday = today - chrono::Duration::days(1);
                let since_utc = local_day_start_utc(yesterday)?;
                let until_utc = local_day_end_utc(yesterday)?;
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
                let since_utc = local_day_start_utc(*start)?;
                let until_utc = local_day_end_utc(*end)?;
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

pub struct TodayRow {
    pub app_id: String,
    pub category: Category,
    pub total_ms: i64,
}

pub struct TodayData {
    pub date: chrono::NaiveDate,
    pub rows: Vec<TodayRow>,
}

pub struct MetricsData {
    pub days: u32,
    pub total_ms: i64,
    pub productive_ms: i64,
    pub unproductive_ms: i64,
    pub neutral_ms: i64,
    pub productive_active_ms: i64,
    pub productive_passive_ms: i64,
    pub productive_idle_ms: i64,
}

pub struct TimelineBucket {
    pub hour: u32,
    pub minute: u32,
    pub productive_ms: i64,
    pub neutral_ms: i64,
    pub unproductive_ms: i64,
    pub idle_ms: i64,
    pub keystrokes: i64,
    pub dominant_app: String,
}

pub struct TimelineData {
    pub date: chrono::NaiveDate,
    pub bucket_min: u32,
    pub buckets: Vec<TimelineBucket>,
}

#[derive(Serialize)]
#[allow(dead_code)]
pub struct CategoryBreakdown {
    pub category: Category,
    pub total_ms: i64,
    pub active_ms: i64,
    pub idle_ms: i64,
}

#[derive(Serialize)]
pub struct AppBreakdown {
    pub app_id: String,
    pub category: Category,
    pub total_ms: i64,
    pub active_ms: i64,
    pub keys: i64,
    pub clicks: i64,
}

#[derive(Serialize)]
pub struct AppGroup {
    pub app_id: String,
    pub total_ms: i64,
    pub active_ms: i64,
    pub keys: i64,
    pub clicks: i64,
    pub children: Vec<AppBreakdown>,
}

#[derive(Serialize)]
pub struct DailyBreakdown {
    pub date: String,
    pub total_ms: i64,
    pub active_ms: i64,
    pub keystrokes: i64,
    pub switches: i64,
}

#[derive(Serialize)]
pub struct HourBreakdown {
    pub hour: u32,
    pub total_ms: i64,
    pub keystrokes: i64,
}

#[derive(Serialize)]
#[allow(dead_code)]
pub struct GapEntry {
    pub gap_type: GapType,
    pub start_time: String,
    pub end_time: String,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GapType {
    Sleep,
    LongBreak,
    ShortBreak,
}

impl GapType {
    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            GapType::Sleep => "Sleep",
            GapType::LongBreak => "Long Break",
            GapType::ShortBreak => "Short Break",
        }
    }
}

#[derive(Serialize)]
pub struct GapSummary {
    pub gap_type: GapType,
    pub count: i64,
    pub total_ms: i64,
    pub avg_ms: i64,
}

#[derive(Serialize)]
pub struct AwayData {
    pub summaries: Vec<GapSummary>,
    pub total_away_ms: i64,
    #[allow(dead_code)]
    pub entries: Vec<GapEntry>,
}

#[derive(Serialize)]
pub struct ScheduleBreakdown {
    pub work_label: String,
    pub work_total_ms: i64,
    pub work_active_ms: i64,
    pub work_keys: i64,
    pub after_total_ms: i64,
    pub after_active_ms: i64,
    pub after_keys: i64,
}

#[derive(Serialize)]
pub struct FocusStreak {
    pub app_id: String,
    pub start_time: String,
    pub duration_ms: i64,
    pub keystrokes: i64,
}

#[derive(Serialize)]
pub struct StreakSummary {
    pub longest_productive_ms: i64,
    pub longest_productive_app: String,
    pub avg_productive_streak_ms: i64,
    pub total_productive_streaks: i64,
    pub top_streaks: Vec<FocusStreak>,
}

#[derive(Serialize)]
pub struct ReportData {
    pub since_str: String,
    pub now_str: String,
    pub total_ms: i64,
    pub active_ms: i64,
    pub passive_ms: i64,
    pub idle_ms: i64,
    pub total_keys: i64,
    pub total_clicks: i64,
    pub total_scroll: i64,
    pub total_distance: i64,
    pub total_events: i64,
    pub jiggler_count: i64,
    pub categories: Vec<CategoryBreakdown>,
    pub top_apps: Vec<AppGroup>,
    pub daily: Vec<DailyBreakdown>,
    pub peak_hours: Vec<HourBreakdown>,
    pub schedule: Option<ScheduleBreakdown>,
    pub away: Option<AwayData>,
    pub streaks: Option<StreakSummary>,
}

fn local_day_start_utc(date: chrono::NaiveDate) -> Result<String, Error> {
    let day_start = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| Error::NiriError("invalid local day start".into()))?;
    let local_dt = match day_start.and_local_timezone(Local) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(dt, _) => dt,
        LocalResult::None => {
            return Err(Error::NiriError(
                "local day start is not representable".into(),
            ));
        }
    };
    Ok(local_dt.with_timezone(&Utc).to_rfc3339())
}

fn local_day_end_utc(date: chrono::NaiveDate) -> Result<String, Error> {
    let next_day = date + chrono::Duration::days(1);
    local_day_start_utc(next_day)
}

fn parse_timestamp_local(value: &str) -> Option<chrono::DateTime<Local>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Local))
        .ok()
}

pub fn query_today(app: &App) -> Result<TodayData, Error> {
    let date = Local::now().date_naive();
    let day_start_utc = local_day_start_utc(date)?;

    let mut stmt = app.conn.prepare(
        "SELECT app_id, category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) as total_ms 
         FROM events 
         WHERE timestamp >= ?1 
         GROUP BY app_id 
         ORDER BY total_ms DESC",
    )?;

    let rows = stmt
        .query_map([&day_start_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .map(|(app_id, category_raw, total_ms)| TodayRow {
            app_id,
            category: Category::from_str(&category_raw).unwrap_or(Category::Neutral),
            total_ms,
        })
        .collect();

    Ok(TodayData { date, rows })
}

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

#[derive(Default)]
struct Metrics {
    total_ms: i64,
    productive_ms: i64,
    unproductive_ms: i64,
    neutral_ms: i64,
    productive_active_ms: i64,
    productive_passive_ms: i64,
    productive_idle_ms: i64,
}

#[allow(dead_code)]
pub fn query_metrics(app: &App, days: u32) -> Result<MetricsData, Error> {
    query_metrics_range(app, TimeRange::Days(days))
}

pub fn query_metrics_range(app: &App, range: TimeRange) -> Result<MetricsData, Error> {
    let bounds = range.resolve()?;
    let time_filter = if let Some(ref until) = bounds.until_utc {
        format!("timestamp >= '{}' AND timestamp <= '{}'", bounds.since_utc, until)
    } else {
        format!("timestamp >= '{}'", bounds.since_utc)
    };
    let days = (bounds.end_date - bounds.start_date).num_days() as u32 + 1;

    let mut stmt = app.conn.prepare(&format!(
        "SELECT category, SUM(active_ms) as active, COALESCE(SUM(passive_ms),0) as passive, SUM(idle_ms) as idle
         FROM events 
         WHERE {} 
         GROUP BY category",
        time_filter
    ))?;

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

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    for row in rows {
        let (category_raw, active_ms, passive_ms, idle_ms) = row?;
        let total = active_ms + passive_ms + idle_ms;
        m.total_ms += total;

        match Category::from_str(&category_raw).unwrap_or(Category::Neutral) {
            Category::Productive => {
                m.productive_ms += total;
                m.productive_active_ms += active_ms;
                m.productive_passive_ms += passive_ms;
                m.productive_idle_ms += idle_ms;
            }
            Category::Unproductive => {
                m.unproductive_ms += total;
            }
            Category::Neutral => {
                m.neutral_ms += total;
            }
        }
    }

    Ok(m)
}

#[allow(dead_code)]
pub fn show_metrics(app: &App, days: u32) -> Result<(), Error> {
    show_metrics_range(app, TimeRange::Days(days))
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

pub fn query_timeline(app: &App, days_back: u32, bucket_min: u32) -> Result<TimelineData, Error> {
    assert!(bucket_min > 0, "bucket size must be positive");
    let date = Local::now().date_naive() - chrono::Duration::days(days_back as i64);
    let day_start_utc = local_day_start_utc(date)?;
    let day_end_utc = local_day_end_utc(date)?;

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
        .filter_map(|r| r.ok())
        .collect();

    struct BucketAcc {
        productive_ms: i64,
        neutral_ms: i64,
        unproductive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
        dominant_app: String,
        dominant_app_ms: i64,
    }

    let mut bucket_map: std::collections::BTreeMap<u32, BucketAcc> =
        std::collections::BTreeMap::new();

    for ev in &events {
        let Some(ts) = parse_timestamp_local(&ev.timestamp) else {
            continue;
        };

        let minutes_since_midnight = ts.hour() * 60 + ts.minute();
        let bucket_key = minutes_since_midnight / bucket_min * bucket_min;
        let total_ms = ev.active_ms + ev.passive_ms + ev.idle_ms;

        let b = bucket_map.entry(bucket_key).or_insert_with(|| BucketAcc {
            productive_ms: 0,
            neutral_ms: 0,
            unproductive_ms: 0,
            idle_ms: 0,
            keystrokes: 0,
            dominant_app: String::new(),
            dominant_app_ms: 0,
        });

        match Category::from_str(&ev.category).unwrap_or(Category::Neutral) {
            Category::Productive => b.productive_ms += total_ms,
            Category::Unproductive => b.unproductive_ms += total_ms,
            Category::Neutral => b.neutral_ms += total_ms,
        }
        b.idle_ms += ev.passive_ms + ev.idle_ms;
        b.keystrokes += ev.keystrokes;

        if total_ms > b.dominant_app_ms {
            b.dominant_app_ms = total_ms;
            b.dominant_app.clone_from(&ev.app_id);
        }
    }

    let buckets = bucket_map
        .into_iter()
        .map(|(key, b)| TimelineBucket {
            hour: key / 60,
            minute: key % 60,
            productive_ms: b.productive_ms,
            neutral_ms: b.neutral_ms,
            unproductive_ms: b.unproductive_ms,
            idle_ms: b.idle_ms,
            keystrokes: b.keystrokes,
            dominant_app: b.dominant_app,
        })
        .collect();

    Ok(TimelineData {
        date,
        bucket_min,
        buckets,
    })
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
        let total = b.productive_ms + b.neutral_ms + b.unproductive_ms;
        if total == 0 {
            continue;
        }

        let idle_pct = if total > 0 {
            b.idle_ms as f64 / total as f64
        } else {
            0.0
        };

        let prod_frac = b.productive_ms as f64 / total as f64;
        let neutral_frac = b.neutral_ms as f64 / total as f64;
        let unprod_frac = b.unproductive_ms as f64 / total as f64;

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

fn group_apps(flat: Vec<AppBreakdown>, limit: usize) -> Vec<AppGroup> {
    let mut map: Vec<(String, AppGroup)> = Vec::new();

    for entry in flat {
        if let Some((_, group)) = map.iter_mut().find(|(id, _)| *id == entry.app_id) {
            group.total_ms = group.total_ms.saturating_add(entry.total_ms);
            group.active_ms = group.active_ms.saturating_add(entry.active_ms);
            group.keys = group.keys.saturating_add(entry.keys);
            group.clicks = group.clicks.saturating_add(entry.clicks);
            group.children.push(entry);
        } else {
            let id = entry.app_id.clone();
            map.push((
                id,
                AppGroup {
                    app_id: entry.app_id.clone(),
                    total_ms: entry.total_ms,
                    active_ms: entry.active_ms,
                    keys: entry.keys,
                    clicks: entry.clicks,
                    children: vec![entry],
                },
            ));
        }
    }

    map.sort_by_key(|(_, g)| std::cmp::Reverse(g.total_ms));
    map.truncate(limit);
    map.into_iter().map(|(_, g)| g).collect()
}

fn classify_gap(start_hour: u32, gap_hours: f64) -> Option<GapType> {
    // Sleep: started 8pm-6am AND lasted 4+ hours
    // Long Break: 2+ hours (not sleep)
    // Short Break: 30min-2h
    // Gaps < 30min or > 24h are ignored
    if !(0.5..=24.0).contains(&gap_hours) {
        return None;
    }

    let is_night_start = !(6..20).contains(&start_hour);

    if is_night_start && gap_hours >= 4.0 {
        Some(GapType::Sleep)
    } else if gap_hours >= 2.0 {
        Some(GapType::LongBreak)
    } else {
        Some(GapType::ShortBreak)
    }
}

fn query_streaks(conn: &Connection, config: &Config, time_filter: &str) -> Result<StreakSummary, Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT timestamp, app_id, category, active_ms + COALESCE(passive_ms,0) + idle_ms as total_ms, keystrokes
         FROM events
         WHERE {}
         ORDER BY timestamp",
        time_filter
    ))?;

    struct EventRow {
        timestamp: String,
        app_id: String,
        category: String,
        total_ms: i64,
        keystrokes: i64,
    }

    let events: Vec<EventRow> = stmt
        .query_map([], |row| {
            Ok(EventRow {
                timestamp: row.get(0)?,
                app_id: row.get(1)?,
                category: row.get(2)?,
                total_ms: row.get(3)?,
                keystrokes: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut streaks: Vec<FocusStreak> = Vec::new();
    let mut current_streak_start: Option<String> = None;
    let mut current_streak_app = String::new();
    let mut current_streak_ms: i64 = 0;
    let mut current_streak_keys: i64 = 0;
    let mut in_productive_streak = false;

    for ev in &events {
        let cat = Category::from_str(&ev.category).unwrap_or(Category::Neutral);
        let is_productive = cat == Category::Productive;
        
        if is_productive {
            if in_productive_streak {
                current_streak_ms = current_streak_ms.saturating_add(ev.total_ms);
                current_streak_keys = current_streak_keys.saturating_add(ev.keystrokes);
                if ev.total_ms > 0 {
                    current_streak_app.clone_from(&ev.app_id);
                }
            } else {
                in_productive_streak = true;
                current_streak_start = Some(ev.timestamp.clone());
                current_streak_app.clone_from(&ev.app_id);
                current_streak_ms = ev.total_ms;
                current_streak_keys = ev.keystrokes;
            }
        } else if in_productive_streak && current_streak_ms >= 300_000 {
            let start_str = current_streak_start.take().unwrap_or_default();
            let start_local = parse_timestamp_local(&start_str)
                .map(|dt| dt.format("%m-%d %H:%M").to_string())
                .unwrap_or(start_str);
            
            streaks.push(FocusStreak {
                app_id: current_streak_app.clone(),
                start_time: start_local,
                duration_ms: current_streak_ms,
                keystrokes: current_streak_keys,
            });
            
            in_productive_streak = false;
            current_streak_ms = 0;
            current_streak_keys = 0;
        } else {
            in_productive_streak = false;
            current_streak_ms = 0;
            current_streak_keys = 0;
        }
    }

    if in_productive_streak && current_streak_ms >= 300_000 {
        let start_str = current_streak_start.take().unwrap_or_default();
        let start_local = parse_timestamp_local(&start_str)
            .map(|dt| dt.format("%m-%d %H:%M").to_string())
            .unwrap_or(start_str);
        
        streaks.push(FocusStreak {
            app_id: current_streak_app.clone(),
            start_time: start_local,
            duration_ms: current_streak_ms,
            keystrokes: current_streak_keys,
        });
    }

    streaks.sort_by_key(|s| std::cmp::Reverse(s.duration_ms));
    
    let total_streaks = streaks.len() as i64;
    let total_streak_ms: i64 = streaks.iter().map(|s| s.duration_ms).sum();
    let avg_streak_ms = if total_streaks > 0 {
        total_streak_ms / total_streaks
    } else {
        0
    };
    
    let longest = streaks.first();
    let longest_ms = longest.map(|s| s.duration_ms).unwrap_or(0);
    let longest_app = longest.map(|s| s.app_id.clone()).unwrap_or_default();
    
    streaks.truncate(5);
    
    let _ = config;
    
    Ok(StreakSummary {
        longest_productive_ms: longest_ms,
        longest_productive_app: longest_app,
        avg_productive_streak_ms: avg_streak_ms,
        total_productive_streaks: total_streaks,
        top_streaks: streaks,
    })
}

fn query_gaps(conn: &Connection, since_utc: &str) -> Result<AwayData, Error> {
    let mut stmt = conn.prepare(
        "WITH ordered AS (
            SELECT 
                timestamp,
                LAG(timestamp) OVER (ORDER BY timestamp) as prev_ts
            FROM events
            WHERE timestamp >= ?1
        ),
        gaps AS (
            SELECT 
                prev_ts as gap_start,
                timestamp as gap_end,
                CAST(strftime('%H', prev_ts, 'localtime') AS INT) as start_hour,
                (julianday(timestamp) - julianday(prev_ts)) * 24.0 as gap_hours
            FROM ordered
            WHERE prev_ts IS NOT NULL
        )
        SELECT gap_start, gap_end, start_hour, gap_hours
        FROM gaps
        WHERE gap_hours BETWEEN 0.5 AND 24.0
        ORDER BY gap_start",
    )?;

    let mut entries: Vec<GapEntry> = Vec::new();
    let mut sleep_total_ms: i64 = 0;
    let mut sleep_count: i64 = 0;
    let mut long_break_total_ms: i64 = 0;
    let mut long_break_count: i64 = 0;
    let mut short_break_total_ms: i64 = 0;
    let mut short_break_count: i64 = 0;

    let rows = stmt.query_map(params![since_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u32>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })?;

    for row in rows {
        let (gap_start, gap_end, start_hour, gap_hours) = row?;

        let Some(gap_type) = classify_gap(start_hour, gap_hours) else {
            continue;
        };

        let duration_ms = (gap_hours * 3_600_000.0) as i64;

        match gap_type {
            GapType::Sleep => {
                sleep_total_ms = sleep_total_ms.saturating_add(duration_ms);
                sleep_count += 1;
            }
            GapType::LongBreak => {
                long_break_total_ms = long_break_total_ms.saturating_add(duration_ms);
                long_break_count += 1;
            }
            GapType::ShortBreak => {
                short_break_total_ms = short_break_total_ms.saturating_add(duration_ms);
                short_break_count += 1;
            }
        }

        let start_local = parse_timestamp_local(&gap_start)
            .map(|dt| dt.format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| gap_start.clone());
        let end_local = parse_timestamp_local(&gap_end)
            .map(|dt| dt.format("%H:%M").to_string())
            .unwrap_or_else(|| gap_end.clone());

        entries.push(GapEntry {
            gap_type,
            start_time: start_local,
            end_time: end_local,
            duration_ms,
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

#[allow(dead_code)]
pub fn query_report(app: &App, days: u32) -> Result<ReportData, Error> {
    query_report_range(app, TimeRange::Days(days))
}

pub fn query_report_range(app: &App, range: TimeRange) -> Result<ReportData, Error> {
    let bounds = range.resolve()?;
    let since_utc = &bounds.since_utc;
    let since_str = bounds.since_str.clone();
    let now_str = bounds.now_str.clone();
    
    let time_filter = if let Some(ref until) = bounds.until_utc {
        format!("timestamp >= '{}' AND timestamp <= '{}'", since_utc, until)
    } else {
        format!("timestamp >= '{}'", since_utc)
    };

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
        &format!(
            "SELECT COALESCE(SUM(active_ms + COALESCE(passive_ms,0) + idle_ms),0),
                    COALESCE(SUM(active_ms),0),
                    COALESCE(SUM(passive_ms),0),
                    COALESCE(SUM(idle_ms),0), COALESCE(SUM(keystrokes),0),
                    COALESCE(SUM(mouse_clicks),0), COALESCE(SUM(scroll_events),0),
                    COALESCE(SUM(mouse_distance),0), COUNT(*),
                    COALESCE(SUM(CASE WHEN jiggler_detected = 1 THEN 1 ELSE 0 END),0)
             FROM events WHERE {}", time_filter
        ),
        [],
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

    let mut cat_stmt = app.conn.prepare(&format!(
        "SELECT category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms), SUM(active_ms), SUM(idle_ms)
         FROM events WHERE {}
         GROUP BY category ORDER BY SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) DESC",
        time_filter
    ))?;

    let categories: Vec<CategoryBreakdown> = cat_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .map(
            |(cat_raw, cat_total, cat_active, cat_idle)| CategoryBreakdown {
                category: Category::from_str(&cat_raw).unwrap_or(Category::Neutral),
                total_ms: cat_total,
                active_ms: cat_active,
                idle_ms: cat_idle,
            },
        )
        .collect();

    let mut app_stmt = app.conn.prepare(&format!(
        "SELECT app_id, category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms), SUM(active_ms), SUM(keystrokes), SUM(mouse_clicks)
         FROM events WHERE {}
         GROUP BY app_id, category ORDER BY SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) DESC",
        time_filter
    ))?;

    let flat_apps: Vec<AppBreakdown> = app_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .filter_map(|r| r.ok())
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

    let mut raw_stmt = app.conn.prepare(&format!(
        "SELECT timestamp, active_ms, COALESCE(passive_ms,0), idle_ms, keystrokes
         FROM events WHERE {}",
        time_filter
    ))?;

    struct RawRow {
        timestamp: String,
        active_ms: i64,
        passive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
    }

    let raw_rows: Vec<RawRow> = raw_stmt
        .query_map([], |row| {
            Ok(RawRow {
                timestamp: row.get(0)?,
                active_ms: row.get(1)?,
                passive_ms: row.get(2)?,
                idle_ms: row.get(3)?,
                keystrokes: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut daily_map: std::collections::BTreeMap<String, (i64, i64, i64, i64)> =
        std::collections::BTreeMap::new();
    let mut hourly: HashMap<u32, (i64, i64)> = HashMap::new();

    for row in &raw_rows {
        let Some(local_dt) = parse_timestamp_local(&row.timestamp) else {
            continue;
        };

        let day_key = local_dt.format("%Y-%m-%d").to_string();
        let total = row.active_ms + row.passive_ms + row.idle_ms;

        let d = daily_map.entry(day_key).or_insert((0, 0, 0, 0));
        d.0 += total;
        d.1 += row.active_ms;
        d.2 += row.keystrokes;
        d.3 += 1;

        // Split event duration across hour boundaries
        // ms_left_in_hour = ms until the next hour starts
        let minute = local_dt.minute() as i64;
        let second = local_dt.second() as i64;
        let ms_into_hour = (minute * 60 + second) * 1000;
        let ms_left_in_hour = 3_600_000_i64.saturating_sub(ms_into_hour);

        let mut remaining_ms = total;
        let mut current_hour = local_dt.hour();
        let mut first_chunk = true;

        while remaining_ms > 0 {
            let chunk_ms = if first_chunk {
                remaining_ms.min(ms_left_in_hour)
            } else {
                remaining_ms.min(3_600_000)
            };

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
            let Some(local_dt) = parse_timestamp_local(&row.timestamp) else {
                continue;
            };
            let total = row.active_ms + row.passive_ms + row.idle_ms;
            if app.config.schedule.is_in_schedule(&local_dt) {
                work_total_ms += total;
                work_active_ms += row.active_ms;
                work_keys += row.keystrokes;
            } else {
                after_total_ms += total;
                after_active_ms += row.active_ms;
                after_keys += row.keystrokes;
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

    let away = query_gaps(&app.conn, &since_utc).ok();
    let streaks = query_streaks(&app.conn, &app.config, &time_filter).ok();

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
    })
}

#[allow(dead_code)]
pub fn generate_report(app: &App, days: u32) -> Result<(), Error> {
    generate_report_range(app, TimeRange::Days(days))
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
    let period_line = format!(
        "  Period: {} → {}       ",
        data.since_str, data.now_str
    );
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
        let productive_ms = data.categories
            .iter()
            .find(|c| c.category == Category::Productive)
            .map(|c| c.total_ms)
            .unwrap_or(0);
        
        println!(
            "\n{}",
            section_header("── Goals ─────────────────────────────────────────────")
        );
        
        if let Some(daily_goal) = app.config.goals.daily_ms() {
            let days = (data.daily.len() as i64).max(1);
            let daily_avg = productive_ms / days;
            let daily_pct = (daily_avg as f64 / daily_goal as f64 * 100.0).min(999.0);
            let bar_len = ((daily_pct / 5.0).round() as usize).min(20);
            let progress_bar = "█".repeat(bar_len);
            let remaining_bar = "░".repeat(20 - bar_len);
            
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
            let weekly_pct = (productive_ms as f64 / weekly_goal as f64 * 100.0).min(999.0);
            let bar_len = ((weekly_pct / 5.0).round() as usize).min(20);
            let progress_bar = "█".repeat(bar_len);
            let remaining_bar = "░".repeat(20 - bar_len);
            
            let status = if weekly_pct >= 100.0 {
                format!("{}%", weekly_pct.round()).green().bold().to_string()
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
        let filled = (cat.total_ms as f64 / data.total_ms as f64 * 30.0).round() as usize;
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
    let max_app_ms = data
        .top_apps
        .first()
        .map(|g| g.total_ms)
        .unwrap_or(1)
        .max(1);

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
                    Category::Productive => prod_ms += child.total_ms,
                    Category::Neutral => neutral_ms += child.total_ms,
                    Category::Unproductive => unprod_ms += child.total_ms,
                }
            }
            let bar = format!(
                "{}{}",
                colored_bar(
                    prod_ms as f64 / group.total_ms as f64,
                    neutral_ms as f64 / group.total_ms as f64,
                    unprod_ms as f64 / group.total_ms as f64,
                    filled,
                ),
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

        // Visible col layout: "  X label:  " padded to col 20, then right-aligned duration
        // Using text symbols (1-col wide) to avoid emoji width inconsistencies
        let col_target: usize = 20;
        for summary in &away.summaries {
            let (icon, label_colored, visible_cols) = match summary.gap_type {
                GapType::Sleep => ("◑".blue().to_string(), "Sleep:".blue().to_string(), 8),
                GapType::LongBreak => {
                    ("◐".yellow().to_string(), "Long Break:".yellow().to_string(), 13)
                }
                GapType::ShortBreak => {
                    ("◌".dimmed().to_string(), "Short Break:".dimmed().to_string(), 14)
                }
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

    let max_hour_ms = data.peak_hours.first().map(|h| h.total_ms).unwrap_or(1);
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

    println!();

    Ok(())
}

#[allow(dead_code)]
pub fn export_csv(app: &App, days: u32) -> Result<(), Error> {
    export_csv_range(app, TimeRange::Days(days))
}

pub fn export_csv_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let bounds = range.resolve()?;
    let since_local = bounds.start_date;
    let today_local = bounds.end_date;

    println!(
        "Date,Screen Time (h:mm:ss),Productive (h:mm:ss),Unproductive (h:mm:ss),\
         Undefined (h:mm:ss),ProdActive (h:mm:ss),ProdPassive (h:mm:ss),ProdIdle (h:mm:ss),\
         Productive Ratio,Productive Active %"
    );

    let mut date = since_local;
    while date <= today_local {
        let day_start = local_day_start_utc(date)?;
        let day_end = local_day_end_utc(date)?;

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
            let total = active_ms.saturating_add(passive_ms).saturating_add(idle_ms);
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

#[allow(dead_code)]
pub fn export_xlsx(app: &App, days: u32, path: &str) -> Result<(), Error> {
    export_xlsx_range(app, TimeRange::Days(days), path)
}

pub fn export_xlsx_range(app: &App, range: TimeRange, path: &str) -> Result<(), Error> {
    let bounds = range.resolve()?;
    let since_local = bounds.start_date;
    let today_local = bounds.end_date;

    let mut workbook = Workbook::new();
    
    let header_fmt = Format::new().set_bold();
    let pct_fmt = Format::new().set_num_format("0.0%");
    let total_fmt = Format::new().set_bold().set_background_color(0xE0E0E0);

    let workdays = app.config.schedule.count_workdays(since_local, today_local);
    let days_header = format!("Days ({})", workdays);
    
    let headers = [
        "Date",
        "Screen Time",
        "Productive",
        "Unproductive",
        "Undefined",
        "Prod Active",
        "Prod Passive",
        "Prod Idle",
        "Productive Ratio",
        "Productive Active %",
        "Avg Total",
        "Avg Productive",
        &days_header,
        "Workday Prod Ratio",
        "Workday Prod Active %",
    ];

    let daily_sheet = workbook.add_worksheet();
    daily_sheet
        .set_name("Daily Summary")
        .map_err(|e| Error::NiriError(e.to_string()))?;

    for (col, header) in headers.iter().enumerate() {
        daily_sheet
            .write_string_with_format(0, col as u16, *header, &header_fmt)
            .map_err(|e| Error::NiriError(e.to_string()))?;
    }

    let mut row: u32 = 1;
    let mut date = since_local;
    let mut daily_metrics: Vec<(NaiveDate, Metrics, bool)> = Vec::new();

    while date <= today_local {
        let day_start = local_day_start_utc(date)?;
        let day_end = local_day_end_utc(date)?;

        let mut stmt = app.conn.prepare(
            "SELECT category, SUM(active_ms), COALESCE(SUM(passive_ms),0), SUM(idle_ms)
             FROM events
             WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY category",
        )?;

        let mut m = Metrics::default();

        let rows = stmt.query_map(params![&day_start, &day_end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;

        for r in rows {
            let (category_raw, active_ms, passive_ms, idle_ms) = r?;
            let total = active_ms.saturating_add(passive_ms).saturating_add(idle_ms);
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
            m.productive_ms as f64 / m.total_ms as f64
        } else {
            0.0
        };
        let prod_active_pct = if m.productive_ms > 0 {
            m.productive_active_ms as f64 / m.productive_ms as f64
        } else {
            0.0
        };

        daily_sheet
            .write_string(row, 0, &date.to_string())
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_string(row, 1, &fmt_hms(m.total_ms))
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_string(row, 2, &fmt_hms(m.productive_ms))
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_string(row, 3, &fmt_hms(m.unproductive_ms))
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_string(row, 4, &fmt_hms(m.neutral_ms))
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_string(row, 5, &fmt_hms(m.productive_active_ms))
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_string(row, 6, &fmt_hms(m.productive_passive_ms))
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_string(row, 7, &fmt_hms(m.productive_idle_ms))
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_number_with_format(row, 8, prod_ratio, &pct_fmt)
            .map_err(|e| Error::NiriError(e.to_string()))?;
        daily_sheet
            .write_number_with_format(row, 9, prod_active_pct, &pct_fmt)
            .map_err(|e| Error::NiriError(e.to_string()))?;

        let is_workday = app.config.schedule.is_workday(date);
        daily_metrics.push((date, m, is_workday));
        row += 1;
        date += chrono::Duration::days(1);
    }

    let mut daily_total = Metrics::default();
    let mut workday_total = Metrics::default();
    for (_, m, is_workday) in &daily_metrics {
        daily_total.total_ms = daily_total.total_ms.saturating_add(m.total_ms);
        daily_total.productive_ms = daily_total.productive_ms.saturating_add(m.productive_ms);
        daily_total.unproductive_ms = daily_total.unproductive_ms.saturating_add(m.unproductive_ms);
        daily_total.neutral_ms = daily_total.neutral_ms.saturating_add(m.neutral_ms);
        daily_total.productive_active_ms = daily_total.productive_active_ms.saturating_add(m.productive_active_ms);
        daily_total.productive_passive_ms = daily_total.productive_passive_ms.saturating_add(m.productive_passive_ms);
        daily_total.productive_idle_ms = daily_total.productive_idle_ms.saturating_add(m.productive_idle_ms);
        
        if *is_workday {
            workday_total.total_ms = workday_total.total_ms.saturating_add(m.total_ms);
            workday_total.productive_ms = workday_total.productive_ms.saturating_add(m.productive_ms);
            workday_total.productive_active_ms = workday_total.productive_active_ms.saturating_add(m.productive_active_ms);
        }
    }

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
    let workday_prod_ratio = if workday_total.total_ms > 0 {
        workday_total.productive_ms as f64 / workday_total.total_ms as f64
    } else {
        0.0
    };
    let workday_prod_active_pct = if workday_total.productive_ms > 0 {
        workday_total.productive_active_ms as f64 / workday_total.productive_ms as f64
    } else {
        0.0
    };

    let total_pct_fmt = Format::new().set_bold().set_background_color(0xE0E0E0).set_num_format("0.0%");

    let avg_total_ms = if workdays > 0 { daily_total.total_ms / workdays } else { 0 };
    let avg_productive_ms = if workdays > 0 { daily_total.productive_ms / workdays } else { 0 };

    daily_sheet
        .write_string_with_format(row, 0, "TOTAL", &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 1, &fmt_hms(daily_total.total_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 2, &fmt_hms(daily_total.productive_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 3, &fmt_hms(daily_total.unproductive_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 4, &fmt_hms(daily_total.neutral_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 5, &fmt_hms(daily_total.productive_active_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 6, &fmt_hms(daily_total.productive_passive_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 7, &fmt_hms(daily_total.productive_idle_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_number_with_format(row, 8, total_prod_ratio, &total_pct_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_number_with_format(row, 9, total_prod_active_pct, &total_pct_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 10, &fmt_hms(avg_total_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_string_with_format(row, 11, &fmt_hms(avg_productive_ms), &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_number_with_format(row, 12, workdays as f64, &total_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_number_with_format(row, 13, workday_prod_ratio, &total_pct_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;
    daily_sheet
        .write_number_with_format(row, 14, workday_prod_active_pct, &total_pct_fmt)
        .map_err(|e| Error::NiriError(e.to_string()))?;

    daily_sheet.autofit();

    workbook
        .save(path)
        .map_err(|e| Error::NiriError(e.to_string()))?;

    println!("Exported to {}", path);
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
    
    let productive_ms = data.categories
        .iter()
        .find(|c| c.category == Category::Productive)
        .map(|c| c.total_ms)
        .unwrap_or(0);
    
    let unproductive_ms = data.categories
        .iter()
        .find(|c| c.category == Category::Unproductive)
        .map(|c| c.total_ms)
        .unwrap_or(0);
    
    let ratio = if data.total_ms > 0 {
        (productive_ms as f64 / data.total_ms as f64 * 100.0).round() as i64
    } else {
        0
    };
    
    let top_app = data.top_apps
        .first()
        .map(|a| a.app_id.as_str())
        .unwrap_or("-");
    
    println!(
        "{}|{}|{}|{}|{}%|{}",
        data.since_str.split_whitespace().next().unwrap_or(&data.since_str),
        fmt_hms(data.total_ms),
        fmt_hms(productive_ms),
        fmt_hms(unproductive_ms),
        ratio,
        top_app
    );
    
    Ok(())
}

pub fn export_heatmap_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let bounds = range.resolve()?;
    let time_filter = if let Some(ref until) = bounds.until_utc {
        format!("timestamp >= '{}' AND timestamp <= '{}'", bounds.since_utc, until)
    } else {
        format!("timestamp >= '{}'", bounds.since_utc)
    };
    
    let mut stmt = app.conn.prepare(&format!(
        "SELECT timestamp, category, active_ms + COALESCE(passive_ms,0) + idle_ms as total_ms, keystrokes
         FROM events WHERE {}",
        time_filter
    ))?;
    
    let mut heatmap: std::collections::BTreeMap<(String, u32), HeatmapCell> = 
        std::collections::BTreeMap::new();
    
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    
    for row in rows {
        let (timestamp, category_raw, total_ms, keystrokes) = row?;
        let Some(local_dt) = parse_timestamp_local(&timestamp) else { continue };
        
        let date = local_dt.format("%Y-%m-%d").to_string();
        let hour = local_dt.hour();
        let key = (date.clone(), hour);
        
        let cell = heatmap.entry(key).or_insert_with(|| HeatmapCell {
            date: date.clone(),
            hour,
            productive_ms: 0,
            unproductive_ms: 0,
            neutral_ms: 0,
            total_ms: 0,
            keystrokes: 0,
        });
        
        match Category::from_str(&category_raw).unwrap_or(Category::Neutral) {
            Category::Productive => cell.productive_ms += total_ms,
            Category::Unproductive => cell.unproductive_ms += total_ms,
            Category::Neutral => cell.neutral_ms += total_ms,
        }
        cell.total_ms += total_ms;
        cell.keystrokes += keystrokes;
    }
    
    let cells: Vec<HeatmapCell> = heatmap.into_values().collect();
    let json = serde_json::to_string_pretty(&cells)
        .map_err(|e| Error::NiriError(format!("JSON serialization failed: {}", e)))?;
    println!("{}", json);
    Ok(())
}

/// Compare current period with previous period of same length
pub fn show_comparison(app: &App, range: TimeRange) -> Result<(), Error> {
    let current_bounds = range.resolve()?;
    let current_days = (current_bounds.end_date - current_bounds.start_date).num_days() + 1;
    
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
    
    println!("{:24} {:>12} {:>12} {:>10}", 
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
    } else { 0 };
    let previous_ratio = if previous.total_ms > 0 {
        (previous.productive_ms as f64 / previous.total_ms as f64 * 100.0).round() as i64
    } else { 0 };
    
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
