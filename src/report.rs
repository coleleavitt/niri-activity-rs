use std::collections::HashMap;
use std::str::FromStr;

use chrono::{Local, LocalResult, Timelike, Utc};
use owo_colors::OwoColorize;
use rusqlite::{params, Connection};

use crate::config::{get_data_dir, load_config, Category, Config};
use crate::db::{reclassify_all, run_migrations};
use crate::error::Error;
use crate::fmt::{
    cat_bar, cat_colored, cat_label, colored_bar, fmt_distance, fmt_duration, fmt_duration_compact,
    fmt_hms, pct, section_header, truncate,
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

#[allow(dead_code)]
pub struct CategoryBreakdown {
    pub category: Category,
    pub total_ms: i64,
    pub active_ms: i64,
    pub idle_ms: i64,
}

pub struct AppBreakdown {
    pub app_id: String,
    pub category: Category,
    pub total_ms: i64,
    pub active_ms: i64,
    pub keys: i64,
    pub clicks: i64,
}

/// Grouped view: one entry per app with per-category sub-entries.
pub struct AppGroup {
    pub app_id: String,
    pub total_ms: i64,
    pub active_ms: i64,
    pub keys: i64,
    pub clicks: i64,
    pub children: Vec<AppBreakdown>,
}

pub struct DailyBreakdown {
    pub date: String,
    pub total_ms: i64,
    pub active_ms: i64,
    pub keystrokes: i64,
    pub switches: i64,
}

pub struct HourBreakdown {
    pub hour: u32,
    pub total_ms: i64,
    pub keystrokes: i64,
}

pub struct ScheduleBreakdown {
    pub work_label: String,
    pub work_total_ms: i64,
    pub work_active_ms: i64,
    pub work_keys: i64,
    pub after_total_ms: i64,
    pub after_active_ms: i64,
    pub after_keys: i64,
}

pub struct ReportData {
    pub since_date: chrono::NaiveDate,
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

pub fn query_metrics(app: &App, days: u32) -> Result<MetricsData, Error> {
    let since_local = Local::now().date_naive() - chrono::Duration::days(days as i64);
    let since_utc = local_day_start_utc(since_local)?;

    let mut stmt = app.conn.prepare(
        "SELECT category, SUM(active_ms) as active, COALESCE(SUM(passive_ms),0) as passive, SUM(idle_ms) as idle
         FROM events 
         WHERE timestamp >= ?1 
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

    let rows = stmt.query_map([&since_utc], |row| {
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

pub fn show_metrics(app: &App, days: u32) -> Result<(), Error> {
    let m = query_metrics(app, days)?;

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

pub fn query_report(app: &App, days: u32) -> Result<ReportData, Error> {
    assert!(days > 0, "report must cover at least 1 day");
    let since_date = Local::now().date_naive() - chrono::Duration::days(days as i64);
    let since_utc = local_day_start_utc(since_date)?;
    let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();

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
         FROM events WHERE timestamp >= ?1",
        params![&since_utc],
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
         FROM events WHERE timestamp >= ?1
         GROUP BY category ORDER BY SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) DESC",
    )?;

    let categories: Vec<CategoryBreakdown> = cat_stmt
        .query_map(params![&since_utc], |row| {
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

    let mut app_stmt = app.conn.prepare(
        "SELECT app_id, category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms), SUM(active_ms), SUM(keystrokes), SUM(mouse_clicks)
         FROM events WHERE timestamp >= ?1
         GROUP BY app_id, category ORDER BY SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) DESC",
    )?;

    let flat_apps: Vec<AppBreakdown> = app_stmt
        .query_map(params![&since_utc], |row| {
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

    let mut raw_stmt = app.conn.prepare(
        "SELECT timestamp, active_ms, COALESCE(passive_ms,0), idle_ms, keystrokes
         FROM events WHERE timestamp >= ?1",
    )?;

    struct RawRow {
        timestamp: String,
        active_ms: i64,
        passive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
    }

    let raw_rows: Vec<RawRow> = raw_stmt
        .query_map(params![&since_utc], |row| {
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
        let hour_key = local_dt.hour();
        let total = row.active_ms + row.passive_ms + row.idle_ms;

        let d = daily_map.entry(day_key).or_insert((0, 0, 0, 0));
        d.0 += total;
        d.1 += row.active_ms;
        d.2 += row.keystrokes;
        d.3 += 1;

        let h = hourly.entry(hour_key).or_insert((0, 0));
        h.0 += total;
        h.1 += row.keystrokes;
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

    Ok(ReportData {
        since_date,
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
    })
}

pub fn generate_report(app: &App, days: u32) -> Result<(), Error> {
    let data = query_report(app, days)?;

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
        "  Period: {} → {}              ",
        data.since_date, data.now_str
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

    for group in &data.top_apps {
        if group.children.len() == 1 {
            let row = &group.children[0];
            let bar_len = (row.total_ms as f64 / data.total_ms as f64 * 25.0).round() as usize;
            let active_min = row.active_ms as f64 / 60_000.0;
            let keys_per_min = if active_min > 0.5 {
                format!("{:.0}/m", row.keys as f64 / active_min)
            } else {
                "-".to_string()
            };
            println!(
                "  {:<22} {} {:>8}  {:>5} keys ({:>5}) {:>3} clicks  ({})",
                cat_colored(row.category, &truncate(&row.app_id, 22)),
                cat_bar(row.category, bar_len),
                fmt_duration(row.total_ms).bold(),
                row.keys,
                keys_per_min.dimmed(),
                row.clicks,
                cat_label(row.category),
            );
        } else {
            let bar_len = (group.total_ms as f64 / data.total_ms as f64 * 25.0).round() as usize;
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
            let bar = colored_bar(
                prod_ms as f64 / group.total_ms as f64,
                neutral_ms as f64 / group.total_ms as f64,
                unprod_ms as f64 / group.total_ms as f64,
                bar_len,
            );

            println!(
                "  {:<22} {} {:>8}  {:>5} keys ({:>5}) {:>3} clicks",
                truncate(&group.app_id, 22).bold(),
                bar,
                fmt_duration(group.total_ms).bold(),
                group.keys,
                keys_per_min.dimmed(),
                group.clicks,
            );

            let last_idx = group.children.len().saturating_sub(1);
            for (i, child) in group.children.iter().enumerate() {
                let connector = if i == last_idx { "└─" } else { "├─" };
                println!(
                    "    {} {:<14} {:>8}  {:>5} keys  {:>3} clicks",
                    connector.dimmed(),
                    cat_label(child.category),
                    fmt_duration(child.total_ms),
                    child.keys,
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

    println!();

    Ok(())
}

pub fn export_csv(app: &App, days: u32) -> Result<(), Error> {
    assert!(days > 0, "export must cover at least 1 day");

    let since_local = Local::now().date_naive() - chrono::Duration::days(days as i64);
    let today_local = Local::now().date_naive();

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
