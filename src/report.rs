use std::collections::HashMap;
use std::str::FromStr;

use chrono::{Local, LocalResult, Timelike, Utc};
use rusqlite::{params, Connection};

use crate::config::{get_data_dir, load_config, Category, Config};
use crate::db::run_migrations;
use crate::error::Error;
use crate::fmt::{fmt_distance, fmt_duration, fmt_duration_compact, pct, truncate};

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
        Ok(App { config, conn })
    }
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
            ))
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

pub fn show_today(app: &App) -> Result<(), Error> {
    let _ = &app.config;
    let local_today = Local::now().date_naive();
    let day_start_utc = local_day_start_utc(local_today)?;

    let mut stmt = app.conn.prepare(
        "SELECT app_id, category, SUM(active_ms + idle_ms) as total_ms 
         FROM events 
         WHERE timestamp >= ?1 
         GROUP BY app_id 
         ORDER BY total_ms DESC",
    )?;

    let rows = stmt.query_map([&day_start_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    println!("Activity for today ({}):\n", local_today);
    println!("{:<30} {:>12} {:>10}", "Application", "Category", "Time");
    println!("{}", "-".repeat(54));

    for row in rows {
        let (app_id, category_raw, total_ms) = row?;
        let hours = total_ms / 3_600_000;
        let mins = (total_ms % 3_600_000) / 60_000;
        let secs = (total_ms % 60_000) / 1000;
        let category = Category::from_str(&category_raw).unwrap_or(Category::Neutral);

        println!(
            "{:<30} {:>12} {:>2}h {:>2}m {:>2}s",
            truncate(&app_id, 28),
            category,
            hours,
            mins,
            secs
        );
    }

    Ok(())
}

#[derive(Default)]
struct Metrics {
    total_ms: i64,
    productive_ms: i64,
    unproductive_ms: i64,
    productive_active_ms: i64,
    productive_idle_ms: i64,
}

pub fn show_metrics(app: &App, days: u32) -> Result<(), Error> {
    let since_local = Local::now().date_naive() - chrono::Duration::days(days as i64);
    let since_utc = local_day_start_utc(since_local)?;

    let mut stmt = app.conn.prepare(
        "SELECT category, SUM(active_ms) as active, SUM(idle_ms) as idle
         FROM events 
         WHERE timestamp >= ?1 
         GROUP BY category",
    )?;

    let mut m = Metrics::default();

    let rows = stmt.query_map([&since_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    for row in rows {
        let (category_raw, active_ms, idle_ms) = row?;
        let total = active_ms + idle_ms;
        m.total_ms += total;

        match Category::from_str(&category_raw).unwrap_or(Category::Neutral) {
            Category::Productive => {
                m.productive_ms += total;
                m.productive_active_ms += active_ms;
                m.productive_idle_ms += idle_ms;
            }
            Category::Unproductive => {
                m.unproductive_ms += total;
            }
            Category::Neutral => {}
        }
    }

    println!(
        "=== Productivity Metrics ({} day{}) ===\n",
        days,
        if days == 1 { "" } else { "s" }
    );

    println!("Total Time:              {}", fmt_duration(m.total_ms));
    println!(
        "Productive Time:         {} ({})",
        fmt_duration(m.productive_ms),
        pct(m.productive_ms, m.total_ms)
    );
    println!(
        "Unproductive Time:       {} ({})",
        fmt_duration(m.unproductive_ms),
        pct(m.unproductive_ms, m.total_ms)
    );
    println!();
    println!(
        "Productive Active Time:  {} ({})",
        fmt_duration(m.productive_active_ms),
        pct(m.productive_active_ms, m.productive_ms)
    );
    println!(
        "Productive Passive Time: {} ({})",
        fmt_duration(m.productive_idle_ms),
        pct(m.productive_idle_ms, m.productive_ms)
    );

    Ok(())
}

pub fn show_timeline(app: &App, days_back: u32, bucket_min: u32) -> Result<(), Error> {
    assert!(bucket_min > 0, "bucket size must be positive");
    let target_local = Local::now().date_naive() - chrono::Duration::days(days_back as i64);
    let day_start_utc = local_day_start_utc(target_local)?;
    let day_end_utc = local_day_end_utc(target_local)?;

    let mut stmt = app.conn.prepare(
        "SELECT timestamp, app_id, category, active_ms, idle_ms, keystrokes
         FROM events
         WHERE timestamp >= ?1 AND timestamp < ?2
         ORDER BY timestamp",
    )?;

    struct EventRow {
        timestamp: String,
        app_id: String,
        category: String,
        active_ms: i64,
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
                idle_ms: row.get(4)?,
                keystrokes: row.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    if events.is_empty() {
        println!("No activity recorded for {}", target_local);
        return Ok(());
    }

    struct Bucket {
        productive_ms: i64,
        neutral_ms: i64,
        unproductive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
        dominant_app: String,
        dominant_app_ms: i64,
    }

    let mut buckets: std::collections::BTreeMap<u32, Bucket> = std::collections::BTreeMap::new();

    for ev in &events {
        let Some(ts) = parse_timestamp_local(&ev.timestamp) else {
            continue;
        };

        let minutes_since_midnight = ts.hour() * 60 + ts.minute();
        let bucket_key = minutes_since_midnight / bucket_min * bucket_min;
        let total_ms = ev.active_ms + ev.idle_ms;

        let b = buckets.entry(bucket_key).or_insert_with(|| Bucket {
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
        b.idle_ms += ev.idle_ms;
        b.keystrokes += ev.keystrokes;

        if total_ms > b.dominant_app_ms {
            b.dominant_app_ms = total_ms;
            b.dominant_app.clone_from(&ev.app_id);
        }
    }

    println!(
        "=== Timeline for {} ({}min buckets) ===\n",
        target_local, bucket_min
    );

    let bar_width: usize = 20;

    for (&bucket_key, b) in &buckets {
        let hour = bucket_key / 60;
        let min = bucket_key % 60;
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

        let prod_chars = (prod_frac * bar_width as f64).round() as usize;
        let neutral_chars = (neutral_frac * bar_width as f64).round() as usize;
        let unprod_chars = (unprod_frac * bar_width as f64).round() as usize;
        let remaining = bar_width.saturating_sub(prod_chars + neutral_chars + unprod_chars);

        let bar = format!(
            "{}{}{}{}",
            "█".repeat(prod_chars),
            "▒".repeat(neutral_chars),
            "░".repeat(unprod_chars),
            " ".repeat(remaining),
        );

        let idle_marker = if idle_pct > 0.8 {
            " [AFK]"
        } else if idle_pct > 0.5 {
            " [mostly idle]"
        } else {
            ""
        };

        println!(
            "{:02}:{:02} {} {:>6} {:>4} keys  {:<20}{}",
            hour,
            min,
            bar,
            fmt_duration_compact(total),
            b.keystrokes,
            truncate(&b.dominant_app, 20),
            idle_marker,
        );
    }

    println!("\n  █ productive  ▒ neutral  ░ unproductive");

    Ok(())
}

pub fn generate_report(app: &App, days: u32) -> Result<(), Error> {
    assert!(days > 0, "report must cover at least 1 day");
    let since_local = Local::now().date_naive() - chrono::Duration::days(days as i64);
    let since_utc = local_day_start_utc(since_local)?;
    let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();

    println!("╔══════════════════════════════════════════════════════╗");
    println!("║           ACTIVITY REPORT                           ║");
    println!("║  Period: {} → {}              ║", since_local, now_str);
    println!("╚══════════════════════════════════════════════════════╝\n");

    let (
        total_ms,
        active_ms,
        idle_ms,
        total_keys,
        total_clicks,
        total_scroll,
        total_distance,
        total_events,
    ): (i64, i64, i64, i64, i64, i64, i64, i64) = app.conn.query_row(
        "SELECT COALESCE(SUM(active_ms + idle_ms),0), COALESCE(SUM(active_ms),0),
                COALESCE(SUM(idle_ms),0), COALESCE(SUM(keystrokes),0),
                COALESCE(SUM(mouse_clicks),0), COALESCE(SUM(scroll_events),0),
                COALESCE(SUM(mouse_distance),0), COUNT(*)
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
            ))
        },
    )?;

    if total_events == 0 {
        println!("No activity recorded for this period.");
        return Ok(());
    }

    println!("── Overview ──────────────────────────────────────────");
    println!("  Total Time:         {}", fmt_duration(total_ms));
    println!(
        "  Active:             {} ({})",
        fmt_duration(active_ms),
        pct(active_ms, total_ms)
    );
    println!(
        "  Idle/AFK:           {} ({})",
        fmt_duration(idle_ms),
        pct(idle_ms, total_ms)
    );
    println!("  Focus Switches:     {}", total_events);
    println!("  Keystrokes:         {}", total_keys);
    println!("  Mouse Clicks:       {}", total_clicks);
    println!("  Scroll Events:      {}", total_scroll);
    println!(
        "  Mouse Travel:       {}",
        fmt_distance(total_distance, app.config.mouse_dpi)
    );

    println!("\n── Productivity ──────────────────────────────────────");

    let mut cat_stmt = app.conn.prepare(
        "SELECT category, SUM(active_ms + idle_ms), SUM(active_ms), SUM(idle_ms)
         FROM events WHERE timestamp >= ?1
         GROUP BY category ORDER BY SUM(active_ms + idle_ms) DESC",
    )?;

    let cats: Vec<(String, i64, i64, i64)> = cat_stmt
        .query_map(params![&since_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (cat_raw, cat_total, cat_active, _cat_idle) in &cats {
        let category = Category::from_str(cat_raw).unwrap_or(Category::Neutral);
        let icon = match category {
            Category::Productive => "●",
            Category::Unproductive => "○",
            Category::Neutral => "◌",
        };
        let bar = crate::fmt::bar(*cat_total as f64 / total_ms as f64, 30);
        let bar = bar.trim_end();
        println!(
            "  {} {:<14} {} {} (active: {})",
            icon,
            category,
            bar,
            fmt_duration(*cat_total),
            pct(*cat_active, *cat_total),
        );
    }

    println!("\n── Top Applications ──────────────────────────────────");

    let mut app_stmt = app.conn.prepare(
        "SELECT app_id, category, SUM(active_ms + idle_ms), SUM(active_ms), SUM(keystrokes), SUM(mouse_clicks)
         FROM events WHERE timestamp >= ?1
         GROUP BY app_id, category ORDER BY SUM(active_ms + idle_ms) DESC
         LIMIT 15",
    )?;

    struct AppRow {
        app_id: String,
        category: Category,
        total_ms: i64,
        active_ms: i64,
        keys: i64,
        clicks: i64,
    }

    let app_rows: Vec<AppRow> = app_stmt
        .query_map(params![&since_utc], |row| {
            Ok(AppRow {
                app_id: row.get::<_, String>(0)?,
                category: Category::from_str(&row.get::<_, String>(1)?)
                    .unwrap_or(Category::Neutral),
                total_ms: row.get::<_, i64>(2)?,
                active_ms: row.get::<_, i64>(3)?,
                keys: row.get::<_, i64>(4)?,
                clicks: row.get::<_, i64>(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    for row in &app_rows {
        let bar_len = (row.total_ms as f64 / total_ms as f64 * 25.0).round() as usize;
        let active_min = row.active_ms as f64 / 60_000.0;
        let keys_per_min = if active_min > 0.5 {
            format!("{:.0}/m", row.keys as f64 / active_min)
        } else {
            "-".to_string()
        };
        println!(
            "  {:<22} {} {:>8}  {:>5} keys ({:>5}) {:>3} clicks  ({})",
            truncate(&row.app_id, 22),
            "█".repeat(bar_len),
            fmt_duration(row.total_ms),
            row.keys,
            keys_per_min,
            row.clicks,
            row.category,
        );
    }

    let mut raw_stmt = app.conn.prepare(
        "SELECT timestamp, active_ms, idle_ms, keystrokes
         FROM events WHERE timestamp >= ?1",
    )?;

    struct RawRow {
        timestamp: String,
        active_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
    }

    let raw_rows: Vec<RawRow> = raw_stmt
        .query_map(params![&since_utc], |row| {
            Ok(RawRow {
                timestamp: row.get(0)?,
                active_ms: row.get(1)?,
                idle_ms: row.get(2)?,
                keystrokes: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    println!("\n── Daily Breakdown ───────────────────────────────────");

    let mut daily: std::collections::BTreeMap<String, (i64, i64, i64, i64)> =
        std::collections::BTreeMap::new();
    let mut hourly: HashMap<u32, (i64, i64)> = HashMap::new();

    for row in &raw_rows {
        let Some(local_dt) = parse_timestamp_local(&row.timestamp) else {
            continue;
        };

        let day_key = local_dt.format("%Y-%m-%d").to_string();
        let hour_key = local_dt.hour();
        let total = row.active_ms + row.idle_ms;

        let d = daily.entry(day_key).or_insert((0, 0, 0, 0));
        d.0 += total;
        d.1 += row.active_ms;
        d.2 += row.keystrokes;
        d.3 += 1;

        let h = hourly.entry(hour_key).or_insert((0, 0));
        h.0 += total;
        h.1 += row.keystrokes;
    }

    for (day, (day_total, day_active, day_keys, switches)) in &daily {
        println!(
            "  {}  {:>8}  active: {}  {:>5} keys  {} switches",
            day,
            fmt_duration(*day_total),
            pct(*day_active, *day_total),
            day_keys,
            switches,
        );
    }

    if app.config.schedule.enabled {
        println!("\n── Schedule ──────────────────────────────────────────");

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
            let total = row.active_ms + row.idle_ms;
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

        let work_label = format!(
            "Work Hours ({}-{}):",
            app.config.schedule.start, app.config.schedule.end
        );

        println!(
            "  {:<27} {:>8}  active: {:>6}  {:>5} keys",
            work_label,
            fmt_duration(work_total_ms),
            pct(work_active_ms, work_total_ms),
            work_keys,
        );
        println!(
            "  {:<27} {:>8}  active: {:>6}  {:>5} keys",
            "After Hours:",
            fmt_duration(after_total_ms),
            pct(after_active_ms, after_total_ms),
            after_keys,
        );
    }

    println!("\n── Peak Hours ────────────────────────────────────────");

    let mut hour_vec: Vec<(u32, i64, i64)> = hourly
        .into_iter()
        .map(|(h, (total, keys))| (h, total, keys))
        .collect();
    hour_vec.sort_by_key(|item| std::cmp::Reverse(item.1));
    hour_vec.truncate(5);

    let max_hour_ms = hour_vec.first().map(|h| h.1).unwrap_or(1);
    for (hour, hour_total, hour_keys) in &hour_vec {
        let bar_len = (*hour_total as f64 / max_hour_ms as f64 * 20.0).round() as usize;
        println!(
            "  {:02}:00  {} {:>8}  {:>5} keys",
            hour,
            "█".repeat(bar_len),
            fmt_duration(*hour_total),
            hour_keys,
        );
    }

    println!();

    Ok(())
}
