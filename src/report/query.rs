//! Query functions for activity data retrieval.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::Timelike;
use rusqlite::{Connection, params};

use super::types::{
    AppBreakdown, AppGroup, AwayData, CategoryBreakdown, DailyBreakdown, FatigueIndicators,
    FatigueTrend, FlowQuality, FlowSession, FlowSummary, FocusStreak, GapEntry, GapSummary,
    GapType, HourBreakdown, HourlyErrorRate, InputMetrics, ReportData, ScheduleBreakdown,
    StreakSummary, TimelineBucket, TimelineData, TodayData, TodayRow,
};
use super::{
    App, MIN_STREAK_MS, MS_PER_HOUR, MS_PER_MIN, TimeRange, UNTIL_SENTINEL, day_end_utc,
    day_start_utc,
};
use crate::config::{Category, Config};
use crate::error::Error;

pub fn query_today(app: &App) -> Result<TodayData, Error> {
    let date = app.config.local_date_today();
    let start = day_start_utc(&app.config, date)?;
    let end = day_end_utc(&app.config, date)?;
    let mut stmt = app.conn.prepare(
        "SELECT app_id, category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) as total_ms 
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2
         GROUP BY app_id ORDER BY total_ms DESC",
    )?;
    let rows = stmt
        .query_map(params![&start, &end], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(app_id, cat_raw, total_ms)| TodayRow {
            app_id,
            category: Category::from_str(&cat_raw).unwrap_or(Category::Neutral),
            total_ms,
        })
        .collect();
    Ok(TodayData { date, rows })
}

pub fn query_metrics_range(
    app: &App,
    range: TimeRange,
) -> Result<super::types::MetricsData, Error> {
    let bounds = range.resolve(&app.config)?;
    let until_utc = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);
    let days = u32::try_from((bounds.end_date - bounds.start_date).num_days())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let mut stmt = app.conn.prepare(
        "SELECT category, SUM(active_ms), COALESCE(SUM(passive_ms),0), SUM(idle_ms)
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2 GROUP BY category",
    )?;
    let mut m = super::types::MetricsData {
        days,
        total_ms: 0,
        productive_ms: 0,
        unproductive_ms: 0,
        neutral_ms: 0,
        productive_active_ms: 0,
        productive_passive_ms: 0,
        productive_idle_ms: 0,
    };
    for row in stmt.query_map(params![&bounds.since_utc, until_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })? {
        let (cat_raw, active_ms, passive_ms, idle_ms) = row?;
        let total = active_ms.saturating_add(passive_ms);
        m.total_ms = m.total_ms.saturating_add(total);
        match Category::from_str(&cat_raw).unwrap_or(Category::Neutral) {
            Category::Productive => {
                m.productive_ms = m.productive_ms.saturating_add(total);
                m.productive_active_ms = m.productive_active_ms.saturating_add(active_ms);
                m.productive_passive_ms = m.productive_passive_ms.saturating_add(passive_ms);
                m.productive_idle_ms = m.productive_idle_ms.saturating_add(idle_ms);
            }
            Category::Unproductive => m.unproductive_ms = m.unproductive_ms.saturating_add(total),
            Category::Neutral => m.neutral_ms = m.neutral_ms.saturating_add(total),
        }
    }
    Ok(m)
}

pub fn query_timeline(app: &App, days_back: u32, bucket_min: u32) -> Result<TimelineData, Error> {
    if bucket_min == 0 {
        return Err(Error::NiriError("bucket size must be positive".into()));
    }
    let date = app.config.local_date_today() - chrono::Duration::days(days_back as i64);
    let start = day_start_utc(&app.config, date)?;
    let end = day_end_utc(&app.config, date)?;
    let mut stmt = app.conn.prepare(
        "SELECT timestamp, app_id, category, active_ms, COALESCE(passive_ms,0), idle_ms, keystrokes
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp",
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
        .query_map(params![&start, &end], |row| {
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
        app_totals: HashMap<String, i64>,
    }
    let mut bucket_map: std::collections::BTreeMap<u32, BucketAcc> =
        std::collections::BTreeMap::new();
    for ev in &events {
        let Some(ts) = app.config.parse_timestamp_to_local(&ev.timestamp) else {
            continue;
        };
        let mins = ts.hour() * 60 + ts.minute();
        let key = mins / bucket_min * bucket_min;
        let total_ms = ev
            .active_ms
            .saturating_add(ev.passive_ms)
            .saturating_add(ev.idle_ms);
        let b = bucket_map.entry(key).or_insert_with(|| BucketAcc {
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
                b.unproductive_ms = b.unproductive_ms.saturating_add(total_ms)
            }
            Category::Neutral => b.neutral_ms = b.neutral_ms.saturating_add(total_ms),
        }
        b.idle_ms = b.idle_ms.saturating_add(ev.idle_ms);
        b.keystrokes = b.keystrokes.saturating_add(ev.keystrokes);
        *b.app_totals.entry(ev.app_id.clone()).or_insert(0) += total_ms;
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

    let (total_ms, active_ms, passive_ms, idle_ms, total_keys, total_clicks, total_scroll, total_distance, total_events, jiggler_count): (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = app.conn.query_row(
        "SELECT COALESCE(SUM(active_ms + COALESCE(passive_ms,0) + idle_ms),0), COALESCE(SUM(active_ms),0), COALESCE(SUM(passive_ms),0),
                COALESCE(SUM(idle_ms),0), COALESCE(SUM(keystrokes),0), COALESCE(SUM(mouse_clicks),0), COALESCE(SUM(scroll_events),0),
                COALESCE(SUM(mouse_distance),0), COUNT(*), COALESCE(SUM(CASE WHEN jiggler_detected = 1 THEN 1 ELSE 0 END),0)
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2",
        params![since_utc, until_utc],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?)),
    )?;

    let categories: Vec<CategoryBreakdown> = {
        let mut stmt = app.conn.prepare("SELECT category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms), SUM(active_ms), SUM(idle_ms)
             FROM events WHERE timestamp >= ?1 AND timestamp < ?2 GROUP BY category ORDER BY SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) DESC")?;
        stmt.query_map(params![since_utc, until_utc], |row| {
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
        .collect()
    };

    let top_apps = {
        let mut stmt = app.conn.prepare("SELECT app_id, category, SUM(active_ms + COALESCE(passive_ms,0) + idle_ms), SUM(active_ms), SUM(keystrokes), SUM(mouse_clicks)
             FROM events WHERE timestamp >= ?1 AND timestamp < ?2 GROUP BY app_id, category ORDER BY SUM(active_ms + COALESCE(passive_ms,0) + idle_ms) DESC")?;
        let flat: Vec<AppBreakdown> = stmt
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
        group_apps(flat, 15)
    };

    struct RawRow {
        timestamp: String,
        active_ms: i64,
        passive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
    }
    let raw_rows: Vec<RawRow> = {
        let mut stmt = app.conn.prepare("SELECT timestamp, active_ms, COALESCE(passive_ms,0), idle_ms, keystrokes FROM events WHERE timestamp >= ?1 AND timestamp < ?2")?;
        stmt.query_map(params![since_utc, until_utc], |row| {
            Ok(RawRow {
                timestamp: row.get(0)?,
                active_ms: row.get(1)?,
                passive_ms: row.get(2)?,
                idle_ms: row.get(3)?,
                keystrokes: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

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
        d.0 += total;
        d.1 += row.active_ms;
        d.2 += row.keystrokes;
        d.3 += 1;
        let minute = local_dt.minute() as i64;
        let second = local_dt.second() as i64;
        let millis = (local_dt.nanosecond() / 1_000_000) as i64;
        let ms_into_hour = (minute * 60 + second) * 1000 + millis;
        let ms_left = MS_PER_HOUR.saturating_sub(ms_into_hour).max(1);
        let mut remaining = total;
        let mut cur_hour = local_dt.hour();
        let mut first = true;
        while remaining > 0 {
            let chunk = if first {
                remaining.min(ms_left)
            } else {
                remaining.min(MS_PER_HOUR)
            };
            if chunk <= 0 {
                break;
            }
            let h = hourly.entry(cur_hour).or_insert((0, 0));
            h.0 += chunk;
            if first {
                h.1 += row.keystrokes;
            }
            remaining -= chunk;
            cur_hour = (cur_hour + 1) % 24;
            first = false;
        }
    }
    let daily: Vec<DailyBreakdown> = daily_map
        .into_iter()
        .map(|(date, (t, a, k, s))| DailyBreakdown {
            date,
            total_ms: t,
            active_ms: a,
            keystrokes: k,
            switches: s,
        })
        .collect();
    let mut hour_vec: Vec<_> = hourly.into_iter().map(|(h, (t, k))| (h, t, k)).collect();
    hour_vec.sort_by_key(|i| std::cmp::Reverse(i.1));
    hour_vec.truncate(5);
    let peak_hours: Vec<HourBreakdown> = hour_vec
        .into_iter()
        .map(|(h, t, k)| HourBreakdown {
            hour: h,
            total_ms: t,
            keystrokes: k,
        })
        .collect();

    let schedule = if app.config.schedule.enabled {
        let (mut wt, mut wa, mut wk, mut at, mut aa, mut ak) = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
        for row in &raw_rows {
            let Some(dt) = app.config.parse_timestamp_to_local(&row.timestamp) else {
                continue;
            };
            let t = row
                .active_ms
                .saturating_add(row.passive_ms)
                .saturating_add(row.idle_ms);
            if app.config.schedule.is_in_schedule(&dt) {
                wt += t;
                wa += row.active_ms;
                wk += row.keystrokes;
            } else {
                at += t;
                aa += row.active_ms;
                ak += row.keystrokes;
            }
        }
        Some(ScheduleBreakdown {
            work_label: format!(
                "Work Hours ({}-{}):",
                app.config.schedule.start, app.config.schedule.end
            ),
            work_total_ms: wt,
            work_active_ms: wa,
            work_keys: wk,
            after_total_ms: at,
            after_active_ms: aa,
            after_keys: ak,
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
    .ok();
    let streaks = query_streaks(&app.conn, &app.config, since_utc, until_utc).ok();
    let input_metrics = query_input_metrics(&app.conn, since_utc, until_utc, total_keys).ok();
    let flow = query_flow_sessions(&app.conn, &app.config, since_utc, until_utc).ok();
    let fatigue = query_fatigue_indicators(&app.conn, &app.config, since_utc, until_utc).ok();

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

fn group_apps(flat: Vec<AppBreakdown>, limit: usize) -> Vec<AppGroup> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<AppGroup> = Vec::new();
    for entry in flat {
        if let Some(&idx) = index.get(&entry.app_id) {
            let g = &mut groups[idx];
            g.total_ms += entry.total_ms;
            g.active_ms += entry.active_ms;
            g.keys += entry.keys;
            g.clicks += entry.clicks;
            g.children.push(entry);
        } else {
            let idx = groups.len();
            index.insert(entry.app_id.clone(), idx);
            groups.push(AppGroup {
                app_id: entry.app_id.clone(),
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
    if !(sleep.gap_min_hours..=sleep.gap_max_hours).contains(&gap_hours) {
        return None;
    }
    let spans_midnight = start_hour > end_hour;
    if sleep.overnight_auto_hours > 0.0 && gap_hours >= sleep.overnight_auto_hours && spans_midnight
    {
        return Some(GapType::Sleep);
    }
    let in_night = if sleep.earliest_hour > sleep.latest_hour {
        start_hour >= sleep.earliest_hour || start_hour < sleep.latest_hour
    } else {
        start_hour >= sleep.earliest_hour && start_hour < sleep.latest_hour
    };
    if in_night && gap_hours >= sleep.min_hours {
        return Some(GapType::Sleep);
    }
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
    if productive_ms >= MIN_STREAK_MS {
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
    struct EventRow {
        timestamp: String,
        app_id: String,
        category: String,
        active_ms: i64,
        keystrokes: i64,
        mouse_clicks: i64,
    }
    let mut stmt = conn.prepare("SELECT timestamp, app_id, category, active_ms, keystrokes, mouse_clicks FROM events WHERE timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp")?;
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
    #[allow(clippy::cast_possible_wrap)]
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
        let is_prod = cat == Category::Productive;
        let has_input = ev.keystrokes > 0 || ev.mouse_clicks > 0;
        let cur_dt = config.parse_timestamp_to_local(&ev.timestamp);
        if in_streak {
            let input_idle = matches!((last_input_ts, cur_dt), (Some(last), Some(cur)) if (cur - last).num_milliseconds() > idle_timeout_ms);
            let wall_gap = matches!((last_input_ts.or(cur_dt), cur_dt), (Some(prev), Some(cur)) if last_input_ts.is_some() && (cur - prev).num_milliseconds() > away_ms);
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
        if is_prod {
            if in_streak && pending_unproductive_ms > tolerance_ms {
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
            if in_streak {
                pending_unproductive_ms = 0;
            }
            if in_streak {
                streak_productive_ms += ev.active_ms;
                streak_keys += ev.keystrokes;
                *streak_app_ms.entry(ev.app_id.clone()).or_insert(0) += ev.active_ms;
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
            pending_unproductive_ms += ev.active_ms;
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
    #[allow(clippy::cast_possible_wrap)]
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
        "WITH ordered AS (SELECT timestamp, LAG(timestamp) OVER (ORDER BY timestamp) as prev_ts FROM events WHERE timestamp >= ?1 AND timestamp < ?2),
         gaps AS (SELECT prev_ts as gap_start, timestamp as gap_end, CAST(strftime('%H', prev_ts, 'localtime') AS INT) as start_hour,
                  CAST(strftime('%H', timestamp, 'localtime') AS INT) as end_hour, (julianday(timestamp) - julianday(prev_ts)) * 24.0 as gap_hours FROM ordered WHERE prev_ts IS NOT NULL)
         SELECT gap_start, gap_end, start_hour, end_hour, gap_hours FROM gaps ORDER BY gap_start")?;
    struct RawGap {
        gap_start: String,
        gap_end: String,
        gap_type: GapType,
        duration_ms: i64,
    }
    let mut raw_gaps: Vec<RawGap> = Vec::new();
    for row in stmt.query_map(params![since_utc, until_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u32>(2)?,
            row.get::<_, u32>(3)?,
            row.get::<_, f64>(4)?,
        ))
    })? {
        let (gap_start, gap_end, start_hour, end_hour, gap_hours) = row?;
        if !(sleep.gap_min_hours..=sleep.gap_max_hours).contains(&gap_hours) {
            continue;
        }
        let Some(gap_type) = classify_gap(start_hour, end_hour, gap_hours, sleep) else {
            continue;
        };
        let duration_ms = if gap_hours.is_finite()
            && gap_hours >= 0.0
            && gap_hours <= (i64::MAX as f64 / MS_PER_HOUR as f64)
        {
            (gap_hours * MS_PER_HOUR as f64) as i64
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
    let merge_window_ms = i64::from(sleep.merge_window_min).saturating_mul(MS_PER_MIN);
    let mut merged: Vec<RawGap> = Vec::new();
    for gap in raw_gaps {
        let should_merge = merge_window_ms > 0
            && gap.gap_type == GapType::Sleep
            && merged.last().is_some_and(|prev| {
                if prev.gap_type != GapType::Sleep {
                    return false;
                }
                let Some(prev_end) = config.parse_timestamp_to_local(&prev.gap_end) else {
                    return false;
                };
                let Some(curr_start) = config.parse_timestamp_to_local(&gap.gap_start) else {
                    return false;
                };
                let between = curr_start
                    .signed_duration_since(prev_end)
                    .num_milliseconds();
                between >= 0 && between < merge_window_ms
            });
        if should_merge {
            if let Some(prev) = merged.last_mut() {
                prev.gap_end.clone_from(&gap.gap_end);
                if let (Some(s), Some(e)) = (
                    config.parse_timestamp_to_local(&prev.gap_start),
                    config.parse_timestamp_to_local(&gap.gap_end),
                ) {
                    prev.duration_ms = e.signed_duration_since(s).num_milliseconds();
                } else {
                    prev.duration_ms += gap.duration_ms;
                }
            }
        } else {
            merged.push(gap);
        }
    }
    let mut entries: Vec<GapEntry> = Vec::new();
    let (mut sleep_ms, mut sleep_n, mut long_ms, mut long_n, mut short_ms, mut short_n) =
        (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    for gap in merged {
        match gap.gap_type {
            GapType::Sleep => {
                sleep_ms += gap.duration_ms;
                sleep_n += 1;
            }
            GapType::LongBreak => {
                long_ms += gap.duration_ms;
                long_n += 1;
            }
            GapType::ShortBreak => {
                short_ms += gap.duration_ms;
                short_n += 1;
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
    if sleep_n > 0 {
        summaries.push(GapSummary {
            gap_type: GapType::Sleep,
            count: sleep_n,
            total_ms: sleep_ms,
            avg_ms: sleep_ms / sleep_n,
        });
    }
    if long_n > 0 {
        summaries.push(GapSummary {
            gap_type: GapType::LongBreak,
            count: long_n,
            total_ms: long_ms,
            avg_ms: long_ms / long_n,
        });
    }
    if short_n > 0 {
        summaries.push(GapSummary {
            gap_type: GapType::ShortBreak,
            count: short_n,
            total_ms: short_ms,
            avg_ms: short_ms / short_n,
        });
    }
    Ok(AwayData {
        summaries,
        total_away_ms: sleep_ms + long_ms + short_ms,
        entries,
    })
}

fn query_input_metrics(
    conn: &Connection,
    since_utc: &str,
    until_utc: &str,
    total_keys: i64,
) -> Result<InputMetrics, Error> {
    let (backspace_count, modifier_count, left_clicks, right_clicks, middle_clicks, scroll_up, scroll_down, scroll_horizontal): (i64, i64, i64, i64, i64, i64, i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(backspace_count),0), COALESCE(SUM(modifier_count),0), COALESCE(SUM(left_clicks),0), COALESCE(SUM(right_clicks),0),
                COALESCE(SUM(middle_clicks),0), COALESCE(SUM(scroll_up),0), COALESCE(SUM(scroll_down),0), COALESCE(SUM(scroll_horizontal),0)
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2", params![since_utc, until_utc],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)))?;
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
    const GAP_TOLERANCE_MS: i64 = 2 * 60 * 1000;
    const MIN_SESSION_MS: i64 = 5 * 60 * 1000;
    const MIN_KEYS_THRESHOLD: f64 = 30.0;
    const OPTIMAL_KEYS: f64 = 80.0;
    struct EventRow {
        app_id: String,
        timestamp: String,
        active_ms: i64,
        keystrokes: i64,
        backspace_count: i64,
        category: String,
    }
    let mut stmt = conn.prepare("SELECT app_id, timestamp, active_ms, keystrokes, backspace_count, category FROM events WHERE timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp")?;
    let rows: Vec<EventRow> = stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok(EventRow {
                app_id: row.get(0)?,
                timestamp: row.get(1)?,
                active_ms: row.get(2)?,
                keystrokes: row.get(3)?,
                backspace_count: row.get(4)?,
                category: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    struct SessionBuilder {
        app_id: String,
        start_ts: String,
        duration_ms: i64,
        keystrokes: i64,
        backspaces: i64,
        event_rates: Vec<f64>,
        last_event_ts: Option<chrono::DateTime<chrono::FixedOffset>>,
    }
    let mut sessions: Vec<FlowSession> = Vec::new();
    let mut current: Option<SessionBuilder> = None;
    let finalize = |b: SessionBuilder, cfg: &Config, out: &mut Vec<FlowSession>| {
        if b.duration_ms < MIN_SESSION_MS {
            return;
        }
        let kpm = if b.duration_ms > 0 {
            (b.keystrokes as f64) / (b.duration_ms as f64 / MS_PER_MIN as f64)
        } else {
            0.0
        };
        if kpm < MIN_KEYS_THRESHOLD {
            return;
        }
        let br = if b.keystrokes > 0 {
            (b.backspaces as f64 / b.keystrokes as f64) * 100.0
        } else {
            0.0
        };
        let cons = compute_consistency(&b.event_rates);
        let rate_sc = if kpm <= 0.0 {
            0.0
        } else if kpm >= OPTIMAL_KEYS {
            100.0
        } else {
            (kpm / OPTIMAL_KEYS * 100.0).min(100.0)
        };
        let dur_sc = if b.duration_ms >= 30 * 60 * 1000 {
            100.0
        } else {
            (b.duration_ms as f64 / (30.0 * 60.0 * 1000.0) * 100.0).min(100.0)
        };
        let err_sc = if br <= 5.0 {
            100.0
        } else if br >= 20.0 {
            0.0
        } else {
            ((20.0 - br) / 15.0 * 100.0).clamp(0.0, 100.0)
        };
        #[allow(clippy::suboptimal_flops)]
        let flow_score =
            (rate_sc * 0.30 + cons as f64 * 0.30 + dur_sc * 0.20 + err_sc * 0.20) as u8;
        let start_local = cfg.parse_timestamp_to_local(&b.start_ts).map_or_else(
            || b.start_ts.clone(),
            |dt| dt.format("%m-%d %H:%M").to_string(),
        );
        out.push(FlowSession {
            app_id: b.app_id,
            start_time: start_local,
            duration_ms: b.duration_ms,
            keystrokes: b.keystrokes,
            keys_per_min: kpm,
            flow_score_0_to_100: flow_score,
            typing_consistency_0_to_100: cons,
            backspace_rate_pct: br,
        });
    };
    for row in &rows {
        let is_prod = row.category == "productive";
        let cur_dt = config.parse_timestamp_to_local(&row.timestamp);
        let gap_exceeded = current.as_ref().is_some_and(|c| matches!((c.last_event_ts, cur_dt), (Some(last), Some(cur)) if (cur - last).num_milliseconds() > GAP_TOLERANCE_MS));
        if (gap_exceeded || (!is_prod && current.is_some()))
            && let Some(b) = current.take()
        {
            finalize(b, config, &mut sessions);
        }
        if !is_prod || row.keystrokes == 0 {
            if let Some(ref mut c) = current {
                c.last_event_ts = cur_dt;
            }
            continue;
        }
        let event_rate = if row.active_ms > 0 {
            (row.keystrokes as f64) / (row.active_ms as f64 / MS_PER_MIN as f64)
        } else {
            0.0
        };
        if let Some(ref mut c) = current {
            if c.app_id == row.app_id {
                c.duration_ms += row.active_ms;
                c.keystrokes += row.keystrokes;
                c.backspaces += row.backspace_count;
                c.event_rates.push(event_rate);
                c.last_event_ts = cur_dt;
            } else {
                let b = current.take().unwrap();
                finalize(b, config, &mut sessions);
                current = Some(SessionBuilder {
                    app_id: row.app_id.clone(),
                    start_ts: row.timestamp.clone(),
                    duration_ms: row.active_ms,
                    keystrokes: row.keystrokes,
                    backspaces: row.backspace_count,
                    event_rates: vec![event_rate],
                    last_event_ts: cur_dt,
                });
            }
        } else {
            current = Some(SessionBuilder {
                app_id: row.app_id.clone(),
                start_ts: row.timestamp.clone(),
                duration_ms: row.active_ms,
                keystrokes: row.keystrokes,
                backspaces: row.backspace_count,
                event_rates: vec![event_rate],
                last_event_ts: cur_dt,
            });
        }
    }
    if let Some(b) = current.take() {
        finalize(b, config, &mut sessions);
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.flow_score_0_to_100));
    let total_flow_ms: i64 = sessions.iter().map(|s| s.duration_ms).sum();
    let flow_sessions = sessions.len() as i64;
    let avg_flow_duration_ms = if flow_sessions > 0 {
        total_flow_ms / flow_sessions
    } else {
        0
    };
    let peak_keys_per_min = sessions
        .iter()
        .map(|s| s.keys_per_min)
        .fold(0.0_f64, f64::max);
    let (mut deep_ms, mut mod_ms, mut light_ms) = (0i64, 0i64, 0i64);
    for s in &sessions {
        match FlowQuality::from_score(s.flow_score_0_to_100) {
            FlowQuality::Deep => deep_ms += s.duration_ms,
            FlowQuality::Moderate => mod_ms += s.duration_ms,
            FlowQuality::Light => light_ms += s.duration_ms,
            FlowQuality::Shallow => {}
        }
    }
    let overall = if sessions.is_empty() {
        0
    } else {
        let ws: f64 = sessions
            .iter()
            .map(|s| s.flow_score_0_to_100 as f64 * s.duration_ms as f64)
            .sum();
        let td: f64 = sessions.iter().map(|s| s.duration_ms as f64).sum();
        if td > 0.0 { (ws / td) as u8 } else { 0 }
    };
    let dom = if deep_ms >= mod_ms && deep_ms >= light_ms {
        FlowQuality::Deep
    } else if mod_ms >= light_ms {
        FlowQuality::Moderate
    } else if light_ms > 0 {
        FlowQuality::Light
    } else {
        FlowQuality::Shallow
    };
    let top_sessions: Vec<FlowSession> = sessions.into_iter().take(5).collect();
    Ok(FlowSummary {
        total_flow_ms,
        flow_sessions,
        avg_flow_duration_ms,
        peak_keys_per_min,
        overall_flow_score: overall,
        dominant_quality: dom,
        deep_flow_ms: deep_ms,
        moderate_flow_ms: mod_ms,
        light_flow_ms: light_ms,
        top_sessions,
    })
}

fn compute_consistency(rates: &[f64]) -> u8 {
    if rates.len() < 2 {
        return 50;
    }
    let mean: f64 = rates.iter().sum::<f64>() / rates.len() as f64;
    if mean < 1.0 {
        return 50;
    }
    let variance: f64 = rates.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rates.len() as f64;
    let cv = variance.sqrt() / mean;
    ((1.0 - cv.min(1.0)) * 100.0).clamp(0.0, 100.0) as u8
}

fn query_fatigue_indicators(
    conn: &Connection,
    config: &Config,
    since_utc: &str,
    until_utc: &str,
) -> Result<FatigueIndicators, Error> {
    struct Row {
        timestamp: String,
        keystrokes: i64,
        backspace_count: i64,
    }
    let mut stmt = conn.prepare("SELECT timestamp, keystrokes, backspace_count FROM events WHERE timestamp >= ?1 AND timestamp < ?2 AND keystrokes > 0 ORDER BY timestamp")?;
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
            let h = dt.hour();
            *hourly_keys.entry(h).or_insert(0) += row.keystrokes;
            *hourly_backspaces.entry(h).or_insert(0) += row.backspace_count;
        }
    }
    let mut hourly_rates: Vec<HourlyErrorRate> = Vec::new();
    for hour in 0..24 {
        let keys = hourly_keys.get(&hour).copied().unwrap_or(0);
        let bs = hourly_backspaces.get(&hour).copied().unwrap_or(0);
        if keys > 100 {
            hourly_rates.push(HourlyErrorRate {
                hour,
                backspace_rate: (bs as f64 / keys as f64) * 100.0,
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
    let early_avg: f64 = hourly_rates[..mid]
        .iter()
        .map(|r| r.backspace_rate)
        .sum::<f64>()
        / mid as f64;
    let late_avg: f64 = hourly_rates[mid..]
        .iter()
        .map(|r| r.backspace_rate)
        .sum::<f64>()
        / (hourly_rates.len() - mid) as f64;
    let trend = if late_avg > early_avg * 1.3 {
        FatigueTrend::Increasing
    } else if late_avg < early_avg * 0.7 {
        FatigueTrend::Decreasing
    } else {
        FatigueTrend::Stable
    };
    let rec = match trend {
        FatigueTrend::Increasing => {
            Some("Error rate increasing later in day. Consider more frequent breaks.".to_string())
        }
        FatigueTrend::Decreasing => {
            Some("Strong finish - error rate decreased as day progressed.".to_string())
        }
        _ => None,
    };
    Ok(FatigueIndicators {
        trend,
        early_error_rate: early_avg,
        late_error_rate: late_avg,
        hourly_rates,
        recommendation: rec,
    })
}
