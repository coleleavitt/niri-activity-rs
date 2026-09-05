//! Query functions for activity data retrieval.

use std::collections::HashMap;

use chrono::{DateTime, Timelike, Utc};
use rusqlite::{Connection, params};

use super::interval::{EventInterval, load_human_intervals, load_overlapping};
use super::types::{
    AppBreakdown, AppGroup, AwayData, CategoryBreakdown, DailyBreakdown, FatigueIndicators,
    FatigueTrend, FlowQuality, FlowSession, FlowSummary, FocusStreak, GapEntry, GapSummary,
    GapType, HourBreakdown, HourlyErrorRate, InputMetrics, Metrics, ProjectBreakdown, ReportData,
    ScheduleBreakdown, StreakSummary, TimelineBucket, TimelineData, TodayData, TodayRow,
};
use super::{
    App, MIN_STREAK_MS, MS_PER_HOUR, MS_PER_MIN, TimeRange, UNTIL_SENTINEL, day_end_utc,
    day_start_utc,
};
use crate::config::{Category, Config};
use crate::db::normalize_input_offsets;
use crate::error::Error;

/// Query today's activity breakdown by application.
pub fn query_today(app: &App) -> Result<TodayData, Error> {
    let date = app.config.local_date_today();
    let start = day_start_utc(&app.config, date)?;
    let end = day_end_utc(&app.config, date)?;
    let mut totals: HashMap<(String, Category), i64> = HashMap::new();
    for event in load_overlapping(&app.conn, &start, &end)? {
        let event_total = event.total_ms();
        let key = (event.app_id, event.category);
        let total = totals.entry(key).or_default();
        *total = total.saturating_add(event_total);
    }
    let mut rows = totals
        .into_iter()
        .map(|((app_id, category), total_ms)| TodayRow {
            app_id,
            category,
            total_ms,
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| std::cmp::Reverse(row.total_ms));
    Ok(TodayData { date, rows })
}

/// Query productivity metrics for a date range.
pub fn query_metrics_range(
    app: &App,
    range: TimeRange,
) -> Result<super::types::MetricsData, Error> {
    let bounds = range.resolve(&app.config)?;
    let until_utc = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);
    let days = u32::try_from((bounds.end_date - bounds.start_date).num_days())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let totals = metrics_between(&app.conn, &app.config, &bounds.since_utc, until_utc)?;
    Ok(super::types::MetricsData {
        days,
        total_ms: totals.total_ms,
        productive_ms: totals.productive_ms,
        unproductive_ms: totals.unproductive_ms,
        neutral_ms: totals.neutral_ms,
        productive_active_ms: totals.productive_active_ms,
        productive_passive_ms: totals.productive_passive_ms,
        productive_idle_ms: totals.productive_idle_ms,
    })
}

/// Total per-category time for a window, with agent time reattributed.
///
/// Shared by every surface that reports productivity so the terminal report,
/// the spreadsheet, and the CSV export cannot disagree with each other.
pub(crate) fn metrics_between(
    conn: &Connection,
    config: &Config,
    start: &str,
    end: &str,
) -> Result<Metrics, Error> {
    let mut m = Metrics::default();
    for event in load_human_intervals(conn, config, start, end)? {
        let total = event.total_ms();
        m.total_ms = m.total_ms.saturating_add(total);

        let credited = StateCredit::for_event(config, &event);
        match event.category {
            Category::Productive => {
                m.productive_ms = m.productive_ms.saturating_add(total);
                m.productive_active_ms = m.productive_active_ms.saturating_add(event.active_ms);
                m.productive_passive_ms = m.productive_passive_ms.saturating_add(event.passive_ms);
                m.productive_idle_ms = m.productive_idle_ms.saturating_add(event.idle_ms);
            }
            Category::Unproductive => {
                m.unproductive_ms = m.unproductive_ms.saturating_add(total - credited.total_ms);
                m.productive_ms = m.productive_ms.saturating_add(credited.total_ms);
                m.productive_active_ms = m.productive_active_ms.saturating_add(credited.active_ms);
                m.productive_passive_ms =
                    m.productive_passive_ms.saturating_add(credited.passive_ms);
                m.productive_idle_ms = m.productive_idle_ms.saturating_add(credited.idle_ms);
            }
            Category::Neutral => {
                m.neutral_ms = m.neutral_ms.saturating_add(total - credited.total_ms);
                m.productive_ms = m.productive_ms.saturating_add(credited.total_ms);
                m.productive_active_ms = m.productive_active_ms.saturating_add(credited.active_ms);
                m.productive_passive_ms =
                    m.productive_passive_ms.saturating_add(credited.passive_ms);
                m.productive_idle_ms = m.productive_idle_ms.saturating_add(credited.idle_ms);
            }
        }
    }
    Ok(m)
}

/// Time to move from `category` into `productive` because an agent was working.
///
/// Clamped to the category's own total: `agent_ms` is measured against a
/// session's full span while these figures exclude idle, so an unclamped
/// credit could exceed the time it is drawn from and push a bucket negative.
fn agent_credit(config: &Config, category: Category, agent_ms: i64, total_ms: i64) -> i64 {
    if category == Category::Productive || !config.agent_activity.counts_as_productive {
        return 0;
    }
    agent_ms.clamp(0, total_ms)
}

fn split_agent_credit(credit: i64, states: (i64, i64, i64)) -> (i64, i64, i64) {
    let states = (states.0.max(0), states.1.max(0), states.2.max(0));
    let total = states.0.saturating_add(states.1).saturating_add(states.2);
    if credit <= 0 || total <= 0 {
        return (0, 0, 0);
    }
    let credit = credit.min(total);
    let share = |part: i64| {
        i64::try_from(i128::from(credit) * i128::from(part) / i128::from(total)).unwrap_or(i64::MAX)
    };
    let active = share(states.0);
    let active_passive = share(states.0.saturating_add(states.1));
    let passive = active_passive.saturating_sub(active);
    let idle = credit.saturating_sub(active_passive);
    (active, passive, idle)
}

#[derive(Clone, Copy, Default)]
#[allow(clippy::struct_field_names)]
struct StateCredit {
    total_ms: i64,
    active_ms: i64,
    passive_ms: i64,
    idle_ms: i64,
}

impl StateCredit {
    fn for_event(config: &Config, event: &EventInterval) -> Self {
        let total_ms = agent_credit(
            config,
            event.category,
            event.agent_ms.unwrap_or(0),
            event.total_ms(),
        );
        let (active_ms, passive_ms, idle_ms) =
            split_agent_credit(total_ms, (event.active_ms, event.passive_ms, event.idle_ms));
        Self {
            total_ms,
            active_ms,
            passive_ms,
            idle_ms,
        }
    }

    fn add(&mut self, other: Self) {
        self.total_ms = self.total_ms.saturating_add(other.total_ms);
        self.active_ms = self.active_ms.saturating_add(other.active_ms);
        self.passive_ms = self.passive_ms.saturating_add(other.passive_ms);
        self.idle_ms = self.idle_ms.saturating_add(other.idle_ms);
    }
}

/// Move agent-concurrent time out of the other categories and into
/// `productive`, preserving each category's own `agent_ms` for display.
fn reattribute_agent_time_with_credits(
    mut categories: Vec<CategoryBreakdown>,
    credits: &HashMap<Category, StateCredit>,
) -> Vec<CategoryBreakdown> {
    let mut credited = StateCredit::default();
    for cat in &mut categories {
        let credit = credits.get(&cat.category).copied().unwrap_or_default();
        if credit.total_ms == 0 {
            continue;
        }
        credited.add(credit);
        cat.total_ms = cat.total_ms.saturating_sub(credit.total_ms);
        cat.active_ms = cat.active_ms.saturating_sub(credit.active_ms);
        cat.idle_ms = cat.idle_ms.saturating_sub(credit.idle_ms);
        cat.agent_ms = cat.agent_ms.saturating_sub(credit.total_ms);
    }

    if credited.total_ms == 0 {
        return categories;
    }

    if let Some(productive) = categories
        .iter_mut()
        .find(|c| c.category == Category::Productive)
    {
        productive.total_ms = productive.total_ms.saturating_add(credited.total_ms);
        productive.active_ms = productive.active_ms.saturating_add(credited.active_ms);
        productive.idle_ms = productive.idle_ms.saturating_add(credited.idle_ms);
        productive.agent_ms = productive.agent_ms.saturating_add(credited.total_ms);
    } else {
        categories.push(CategoryBreakdown {
            category: Category::Productive,
            total_ms: credited.total_ms,
            active_ms: credited.active_ms,
            idle_ms: credited.idle_ms,
            agent_ms: credited.total_ms,
        });
    }

    categories.sort_by_key(|c| std::cmp::Reverse(c.total_ms));
    categories.retain(|c| c.total_ms > 0);
    categories
}

#[cfg(test)]
fn reattribute_agent_time(
    config: &Config,
    categories: Vec<CategoryBreakdown>,
) -> Vec<CategoryBreakdown> {
    let mut credits = HashMap::new();
    for category in &categories {
        let total_ms = agent_credit(
            config,
            category.category,
            category.agent_ms,
            category.total_ms,
        );
        let passive_ms = category
            .total_ms
            .saturating_sub(category.active_ms)
            .saturating_sub(category.idle_ms);
        let (active_ms, passive_ms, idle_ms) =
            split_agent_credit(total_ms, (category.active_ms, passive_ms, category.idle_ms));
        credits.insert(
            category.category,
            StateCredit {
                total_ms,
                active_ms,
                passive_ms,
                idle_ms,
            },
        );
    }
    reattribute_agent_time_with_credits(categories, &credits)
}

/// Query hourly activity timeline for the past N days, bucketed by minute
/// intervals.
pub fn query_timeline(app: &App, days_back: u32, bucket_min: u32) -> Result<TimelineData, Error> {
    if bucket_min == 0 {
        return Err(Error::InvalidArgument(
            "bucket size must be positive".into(),
        ));
    }
    let date = app.config.local_date_today() - chrono::Duration::days(days_back as i64);
    let start = day_start_utc(&app.config, date)?;
    let end = day_end_utc(&app.config, date)?;
    let events = load_human_intervals(&app.conn, &app.config, &start, &end)?;

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
    for event in &events {
        for slice in event.minute_slices() {
            let Some(timestamp) = slice.local_start(&app.config) else {
                continue;
            };
            let minutes = timestamp.hour() * 60 + timestamp.minute();
            let key = minutes / bucket_min * bucket_min;
            let total_ms = slice.active_ms.saturating_add(slice.passive_ms);
            let bucket = bucket_map.entry(key).or_insert_with(|| BucketAcc {
                productive_ms: 0,
                neutral_ms: 0,
                unproductive_ms: 0,
                idle_ms: 0,
                keystrokes: 0,
                app_totals: HashMap::new(),
            });
            match slice.category {
                Category::Productive => {
                    bucket.productive_ms = bucket.productive_ms.saturating_add(total_ms);
                }
                Category::Unproductive => {
                    bucket.unproductive_ms = bucket.unproductive_ms.saturating_add(total_ms);
                }
                Category::Neutral => {
                    bucket.neutral_ms = bucket.neutral_ms.saturating_add(total_ms);
                }
            }
            bucket.idle_ms = bucket.idle_ms.saturating_add(slice.idle_ms);
            bucket.keystrokes = bucket.keystrokes.saturating_add(slice.keystrokes);
            *bucket.app_totals.entry(slice.app_id).or_insert(0) += total_ms;
        }
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

/// Query comprehensive report data for a date range including metrics, apps,
/// and gaps.
pub fn query_report_range(app: &App, range: TimeRange) -> Result<ReportData, Error> {
    let bounds = range.resolve(&app.config)?;
    let since_str = bounds.since_str;
    let now_str = bounds.now_str;
    let since_utc = &bounds.since_utc;
    let until_utc = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);

    let events = load_human_intervals(&app.conn, &app.config, since_utc, until_utc)?;
    let mut total_ms = 0i64;
    let mut active_ms = 0i64;
    let mut passive_ms = 0i64;
    let mut idle_ms = 0i64;
    let mut total_keys = 0i64;
    let mut total_clicks = 0i64;
    let mut total_scroll = 0i64;
    let mut total_distance = 0i64;
    let total_events = i64::try_from(
        events
            .iter()
            .filter(|event| event.start_ms == event.source_start_ms)
            .count(),
    )
    .unwrap_or(i64::MAX);
    let mut jiggler_count = 0i64;
    let mut agent_ms = 0i64;
    let mut unmeasured_agent_ms = 0i64;
    let mut category_map: HashMap<Category, CategoryBreakdown> = HashMap::new();
    let mut category_credits: HashMap<Category, StateCredit> = HashMap::new();
    let mut app_map: HashMap<(String, Category), AppBreakdown> = HashMap::new();
    let mut project_map: HashMap<String, ProjectBreakdown> = HashMap::new();

    for event in &events {
        let event_total = event.total_ms();
        total_ms = total_ms.saturating_add(event_total);
        active_ms = active_ms.saturating_add(event.active_ms);
        passive_ms = passive_ms.saturating_add(event.passive_ms);
        idle_ms = idle_ms.saturating_add(event.idle_ms);
        total_keys = total_keys.saturating_add(event.keystrokes);
        total_clicks = total_clicks.saturating_add(event.mouse_clicks);
        total_scroll = total_scroll.saturating_add(event.scroll_events);
        total_distance = total_distance.saturating_add(event.mouse_distance);
        if event.start_ms == event.source_start_ms {
            jiggler_count = jiggler_count.saturating_add(i64::from(event.jiggler_detected));
        }
        if let Some(measured_agent_ms) = event.agent_ms {
            agent_ms = agent_ms.saturating_add(measured_agent_ms);
        } else {
            unmeasured_agent_ms = unmeasured_agent_ms.saturating_add(event_total);
        }
        category_credits
            .entry(event.category)
            .or_default()
            .add(StateCredit::for_event(&app.config, event));

        let category = category_map
            .entry(event.category)
            .or_insert(CategoryBreakdown {
                category: event.category,
                total_ms: 0,
                active_ms: 0,
                idle_ms: 0,
                agent_ms: 0,
            });
        category.total_ms = category.total_ms.saturating_add(event_total);
        category.active_ms = category.active_ms.saturating_add(event.active_ms);
        category.idle_ms = category.idle_ms.saturating_add(event.idle_ms);
        category.agent_ms = category
            .agent_ms
            .saturating_add(event.agent_ms.unwrap_or(0));

        let app_entry = app_map
            .entry((event.app_id.clone(), event.category))
            .or_insert_with(|| AppBreakdown {
                app_id: event.app_id.clone(),
                category: event.category,
                total_ms: 0,
                active_ms: 0,
                keys: 0,
                clicks: 0,
            });
        app_entry.total_ms = app_entry.total_ms.saturating_add(event_total);
        app_entry.active_ms = app_entry.active_ms.saturating_add(event.active_ms);
        app_entry.keys = app_entry.keys.saturating_add(event.keystrokes);
        app_entry.clicks = app_entry.clicks.saturating_add(event.mouse_clicks);

        if let Some(project) = &event.project {
            let project_entry =
                project_map
                    .entry(project.clone())
                    .or_insert_with(|| ProjectBreakdown {
                        project: project.clone(),
                        total_ms: 0,
                        active_ms: 0,
                        keys: 0,
                        clicks: 0,
                        productive_ms: 0,
                        neutral_ms: 0,
                        unproductive_ms: 0,
                    });
            project_entry.total_ms = project_entry.total_ms.saturating_add(event_total);
            project_entry.active_ms = project_entry.active_ms.saturating_add(event.active_ms);
            project_entry.keys = project_entry.keys.saturating_add(event.keystrokes);
            project_entry.clicks = project_entry.clicks.saturating_add(event.mouse_clicks);
            match event.category {
                Category::Productive => {
                    project_entry.productive_ms =
                        project_entry.productive_ms.saturating_add(event_total);
                }
                Category::Neutral => {
                    project_entry.neutral_ms = project_entry.neutral_ms.saturating_add(event_total);
                }
                Category::Unproductive => {
                    project_entry.unproductive_ms =
                        project_entry.unproductive_ms.saturating_add(event_total);
                }
            }
        }
    }

    let categories = category_map.into_values().collect::<Vec<_>>();
    let credited_before = category_credits
        .values()
        .map(|credit| credit.total_ms)
        .sum();
    let categories = reattribute_agent_time_with_credits(categories, &category_credits);

    let top_apps = group_apps(app_map.into_values().collect(), 15);
    let mut projects = project_map.into_values().collect::<Vec<_>>();
    projects.sort_by_key(|project| std::cmp::Reverse(project.total_ms));
    projects.truncate(15);

    let mut daily_map: std::collections::BTreeMap<String, (i64, i64, i64, i64)> =
        std::collections::BTreeMap::new();
    let mut hourly: HashMap<u32, (i64, i64)> = HashMap::new();
    let mut schedule_totals = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    for event in &events {
        if event.source_start_ms == event.start_ms
            && let Some(timestamp) = event.local_start(&app.config)
        {
            let day = timestamp.format("%Y-%m-%d").to_string();
            daily_map.entry(day).or_insert((0, 0, 0, 0)).3 += 1;
        }

        for slice in event.minute_slices() {
            let Some(timestamp) = slice.local_start(&app.config) else {
                continue;
            };
            let slice_total = slice.total_ms();
            let day = timestamp.format("%Y-%m-%d").to_string();
            let daily = daily_map.entry(day).or_insert((0, 0, 0, 0));
            daily.0 = daily.0.saturating_add(slice_total);
            daily.1 = daily.1.saturating_add(slice.active_ms);
            daily.2 = daily.2.saturating_add(slice.keystrokes);

            let hour = hourly.entry(timestamp.hour()).or_insert((0, 0));
            hour.0 = hour.0.saturating_add(slice_total);
            hour.1 = hour.1.saturating_add(slice.keystrokes);

            if app.config.schedule.enabled {
                if app.config.schedule.is_in_schedule(&timestamp) {
                    schedule_totals.0 = schedule_totals.0.saturating_add(slice_total);
                    schedule_totals.1 = schedule_totals.1.saturating_add(slice.active_ms);
                    schedule_totals.2 = schedule_totals.2.saturating_add(slice.keystrokes);
                } else {
                    schedule_totals.3 = schedule_totals.3.saturating_add(slice_total);
                    schedule_totals.4 = schedule_totals.4.saturating_add(slice.active_ms);
                    schedule_totals.5 = schedule_totals.5.saturating_add(slice.keystrokes);
                }
            }
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
        Some(ScheduleBreakdown {
            work_label: format!(
                "Work Hours ({}-{}):",
                app.config.schedule.start, app.config.schedule.end
            ),
            work_total_ms: schedule_totals.0,
            work_active_ms: schedule_totals.1,
            work_keys: schedule_totals.2,
            after_total_ms: schedule_totals.3,
            after_active_ms: schedule_totals.4,
            after_keys: schedule_totals.5,
        })
    } else {
        None
    };

    let away = Some(query_gaps(
        &app.conn,
        &app.config,
        since_utc,
        until_utc,
        &app.config.sleep,
    )?);
    let streaks = Some(query_streaks(&events, &app.config));
    let input_metrics = Some(input_metrics(&events, total_keys));
    let flow = Some(query_flow_sessions(&events, &app.config));
    let fatigue = Some(fatigue_indicators(&events, &app.config));

    Ok(ReportData {
        since_str,
        now_str,
        total_ms,
        active_ms,
        passive_ms,
        idle_ms,
        agent_ms,
        unmeasured_agent_ms,
        agent_credited_ms: credited_before,
        total_keys,
        total_clicks,
        total_scroll,
        total_distance,
        total_events,
        jiggler_count,
        categories,
        top_apps,
        projects,
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

fn query_streaks(events: &[EventInterval], config: &Config) -> StreakSummary {
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
    for ev in events {
        let is_prod = ev.category == Category::Productive;
        let has_input = ev.keystrokes > 0 || ev.mouse_clicks > 0;
        let cur_dt = ev.local_start(config);
        let timestamp = DateTime::<Utc>::from_timestamp_millis(ev.start_ms)
            .map(|value| value.to_rfc3339())
            .unwrap_or_default();
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
                streak_start = Some(timestamp);
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
    StreakSummary {
        longest_productive_ms: longest_ms,
        longest_productive_app: longest_app,
        avg_productive_streak_ms: avg_streak_ms,
        total_productive_streaks: total_streaks,
        top_streaks: streaks,
    }
}

fn query_gaps(
    conn: &Connection,
    config: &Config,
    since_utc: &str,
    until_utc: &str,
    sleep: &crate::config::SleepConfig,
) -> Result<AwayData, Error> {
    let range_start = chrono::DateTime::parse_from_rfc3339(since_utc)
        .map_err(|error| Error::InvalidArgument(format!("invalid report start: {error}")))?;
    let range_end = if until_utc == UNTIL_SENTINEL {
        Utc::now().fixed_offset()
    } else {
        chrono::DateTime::parse_from_rfc3339(until_utc)
            .map_err(|error| Error::InvalidArgument(format!("invalid report end: {error}")))?
    };
    #[allow(clippy::cast_possible_truncation)]
    let halo_ms = (sleep.gap_max_hours.max(0.0) * MS_PER_HOUR as f64).round() as i64;
    let halo = chrono::Duration::milliseconds(halo_ms);
    let halo_start = range_start
        .checked_sub_signed(halo)
        .unwrap_or(range_start)
        .to_rfc3339();
    let halo_end = range_end
        .checked_add_signed(halo)
        .unwrap_or(range_end)
        .to_rfc3339();

    let mut stmt = conn.prepare(
        "SELECT timestamp,
                active_ms + COALESCE(passive_ms, 0) + idle_ms AS total_ms,
                input_offsets, keystrokes, mouse_clicks, scroll_events
           FROM events
          WHERE timestamp >= ?1 AND timestamp < ?2
          ORDER BY timestamp, id",
    )?;
    struct RawGap {
        gap_start: String,
        gap_end: String,
        gap_type: GapType,
        duration_ms: i64,
    }
    let mut raw_gaps: Vec<RawGap> = Vec::new();
    let mut input_points = Vec::new();
    let mut legacy_intervals = Vec::new();
    let mut legacy_events = Vec::new();
    let mut has_measured_rows = false;
    for row in stmt.query_map(params![halo_start, halo_end], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })? {
        let (timestamp, total_ms, offsets, keystrokes, mouse_clicks, scroll_events) = row?;
        let Ok(start) = chrono::DateTime::parse_from_rfc3339(&timestamp) else {
            continue;
        };
        let start_ms = start.timestamp_millis();
        let duration_ms = total_ms.max(0);
        let end_ms = start_ms.saturating_add(duration_ms);
        let has_counted_input = keystrokes > 0 || mouse_clicks > 0 || scroll_events > 0;
        match offsets {
            Some(blob) if blob.len() % 4 == 0 => {
                has_measured_rows = true;
                if let Some(decoded) = normalize_input_offsets(&blob, duration_ms) {
                    input_points.extend(decoded.iter().map(|offset| {
                        start_ms.saturating_add(i64::from(*offset).min(duration_ms))
                    }));
                    if decoded.is_empty() && has_counted_input {
                        input_points.push(end_ms);
                    }
                } else {
                    legacy_intervals.push((start_ms, end_ms));
                    legacy_events.push((start_ms, duration_ms));
                    if has_counted_input {
                        input_points.push(end_ms);
                    }
                }
            }
            _ => {
                legacy_intervals.push((start_ms, end_ms));
                legacy_events.push((start_ms, duration_ms));
                if has_counted_input {
                    input_points.push(end_ms);
                }
            }
        }
    }
    input_points.sort_unstable();
    input_points.dedup();

    let range_start_ms = range_start.timestamp_millis();
    let range_end_ms = range_end.timestamp_millis();
    {
        let mut record_gap = |gap_start_ms: i64, gap_end_ms: i64| {
            let gap_start_dt = DateTime::<Utc>::from_timestamp_millis(gap_start_ms);
            let gap_end_dt = DateTime::<Utc>::from_timestamp_millis(gap_end_ms);
            let (Some(gap_start_dt), Some(gap_end_dt)) = (gap_start_dt, gap_end_dt) else {
                return;
            };
            let gap_ms = gap_end_dt
                .signed_duration_since(gap_start_dt)
                .num_milliseconds();
            let gap_hours = gap_ms as f64 / MS_PER_HOUR as f64;
            if !(sleep.gap_min_hours..=sleep.gap_max_hours).contains(&gap_hours) {
                return;
            }
            let gap_start = gap_start_dt.to_rfc3339();
            let gap_end = gap_end_dt.to_rfc3339();
            let start_hour = config
                .parse_timestamp_to_local(&gap_start)
                .map_or(0, |dt| dt.hour());
            let end_hour = config
                .parse_timestamp_to_local(&gap_end)
                .map_or(0, |dt| dt.hour());
            let Some(gap_type) = classify_gap(start_hour, end_hour, gap_hours, sleep) else {
                return;
            };
            let clipped_start_ms = gap_start_ms.max(range_start_ms);
            let clipped_end_ms = gap_end_ms.min(range_end_ms);
            if clipped_start_ms >= clipped_end_ms {
                return;
            }
            let Some(clipped_start) = DateTime::<Utc>::from_timestamp_millis(clipped_start_ms)
            else {
                return;
            };
            let Some(clipped_end) = DateTime::<Utc>::from_timestamp_millis(clipped_end_ms) else {
                return;
            };
            raw_gaps.push(RawGap {
                gap_start: clipped_start.to_rfc3339(),
                gap_end: clipped_end.to_rfc3339(),
                gap_type,
                duration_ms: clipped_end_ms.saturating_sub(clipped_start_ms),
            });
        };

        if has_measured_rows {
            for points in input_points.windows(2) {
                let gap_start_ms = points[0];
                let gap_end_ms = points[1];
                if legacy_intervals
                    .iter()
                    .any(|(start, end)| *start < gap_end_ms && *end > gap_start_ms)
                {
                    continue;
                }
                record_gap(gap_start_ms, gap_end_ms);
            }
        } else {
            for events in legacy_events.windows(2) {
                let gap_start_ms = events[0].0.saturating_add(events[0].1);
                record_gap(gap_start_ms, events[1].0);
            }
        }
    }
    let merge_window_ms = i64::from(sleep.merge_window_min).saturating_mul(MS_PER_MIN);
    let mut merged: Vec<RawGap> = Vec::new();
    for gap in raw_gaps {
        let should_merge = merge_window_ms > 0
            && merged.last().is_some_and(|prev| {
                let Some(prev_start) = config.parse_timestamp_to_local(&prev.gap_start) else {
                    return false;
                };
                let Some(prev_end) = config.parse_timestamp_to_local(&prev.gap_end) else {
                    return false;
                };
                let Some(curr_start) = config.parse_timestamp_to_local(&gap.gap_start) else {
                    return false;
                };
                let Some(curr_end) = config.parse_timestamp_to_local(&gap.gap_end) else {
                    return false;
                };
                let between = curr_start
                    .signed_duration_since(prev_end)
                    .num_milliseconds();
                if between < 0 || between >= merge_window_ms {
                    return false;
                }
                let combined_ms = curr_end
                    .signed_duration_since(prev_start)
                    .num_milliseconds();
                let combined_hours = combined_ms as f64 / MS_PER_HOUR as f64;
                classify_gap(prev_start.hour(), curr_end.hour(), combined_hours, sleep)
                    == Some(GapType::Sleep)
            });
        if should_merge {
            if let Some(prev) = merged.last_mut() {
                // Extend the displayed span to cover the whole rest period so
                // the entry reads "sleep_start -> final wake", but accumulate
                // only the ACTUAL no-input gap durations. The brief awake
                // bridge between the two gaps is real awake time and must NOT
                // be counted as sleep/away.
                prev.gap_end.clone_from(&gap.gap_end);
                prev.gap_type = GapType::Sleep;
                prev.duration_ms = prev.duration_ms.saturating_add(gap.duration_ms);
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

fn input_metrics(events: &[EventInterval], total_keys: i64) -> InputMetrics {
    let mut metrics = InputMetrics {
        backspace_count: 0,
        modifier_count: 0,
        left_clicks: 0,
        right_clicks: 0,
        middle_clicks: 0,
        scroll_up: 0,
        scroll_down: 0,
        scroll_horizontal: 0,
        backspace_rate: 0.0,
        modifier_rate: 0.0,
    };
    for event in events {
        metrics.backspace_count = metrics
            .backspace_count
            .saturating_add(event.granular.backspace_count);
        metrics.modifier_count = metrics
            .modifier_count
            .saturating_add(event.granular.modifier_count);
        metrics.left_clicks = metrics
            .left_clicks
            .saturating_add(event.granular.left_clicks);
        metrics.right_clicks = metrics
            .right_clicks
            .saturating_add(event.granular.right_clicks);
        metrics.middle_clicks = metrics
            .middle_clicks
            .saturating_add(event.granular.middle_clicks);
        metrics.scroll_up = metrics.scroll_up.saturating_add(event.granular.scroll_up);
        metrics.scroll_down = metrics
            .scroll_down
            .saturating_add(event.granular.scroll_down);
        metrics.scroll_horizontal = metrics
            .scroll_horizontal
            .saturating_add(event.granular.scroll_horizontal);
    }
    let backspace_rate = if total_keys > 0 {
        (metrics.backspace_count as f64 / total_keys as f64) * 100.0
    } else {
        0.0
    };
    let modifier_rate = if total_keys > 0 {
        (metrics.modifier_count as f64 / total_keys as f64) * 100.0
    } else {
        0.0
    };
    metrics.backspace_rate = backspace_rate;
    metrics.modifier_rate = modifier_rate;
    metrics
}

fn query_flow_sessions(rows: &[EventInterval], config: &Config) -> FlowSummary {
    const GAP_TOLERANCE_MS: i64 = 2 * 60 * 1000;
    const MIN_SESSION_MS: i64 = 5 * 60 * 1000;
    const MIN_KEYS_THRESHOLD: f64 = 30.0;
    const OPTIMAL_KEYS: f64 = 80.0;
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
    for row in rows {
        let is_prod = row.category == Category::Productive;
        let cur_dt = row.local_start(config);
        let timestamp = DateTime::<Utc>::from_timestamp_millis(row.start_ms)
            .map(|value| value.to_rfc3339())
            .unwrap_or_default();
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
                c.backspaces += row.granular.backspace_count;
                c.event_rates.push(event_rate);
                c.last_event_ts = cur_dt;
            } else {
                let b = current.take().unwrap();
                finalize(b, config, &mut sessions);
                current = Some(SessionBuilder {
                    app_id: row.app_id.clone(),
                    start_ts: timestamp,
                    duration_ms: row.active_ms,
                    keystrokes: row.keystrokes,
                    backspaces: row.granular.backspace_count,
                    event_rates: vec![event_rate],
                    last_event_ts: cur_dt,
                });
            }
        } else {
            current = Some(SessionBuilder {
                app_id: row.app_id.clone(),
                start_ts: timestamp,
                duration_ms: row.active_ms,
                keystrokes: row.keystrokes,
                backspaces: row.granular.backspace_count,
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
    #[allow(clippy::cast_possible_wrap)]
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
    let dom = if deep_ms == 0 && mod_ms == 0 && light_ms == 0 {
        FlowQuality::Shallow
    } else if deep_ms >= mod_ms && deep_ms >= light_ms {
        FlowQuality::Deep
    } else if mod_ms >= light_ms {
        FlowQuality::Moderate
    } else {
        FlowQuality::Light
    };
    let top_sessions: Vec<FlowSession> = sessions.into_iter().take(5).collect();
    FlowSummary {
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
    }
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

fn fatigue_indicators(events: &[EventInterval], config: &Config) -> FatigueIndicators {
    let mut hourly_keys: HashMap<u32, i64> = HashMap::new();
    let mut hourly_backspaces: HashMap<u32, i64> = HashMap::new();
    for event in events {
        for slice in event.minute_slices() {
            let Some(timestamp) = slice.local_start(config) else {
                continue;
            };
            let hour = timestamp.hour();
            let keys = hourly_keys.entry(hour).or_default();
            *keys = keys.saturating_add(slice.keystrokes);
            let backspaces = hourly_backspaces.entry(hour).or_default();
            *backspaces = backspaces.saturating_add(slice.granular.backspace_count);
        }
    }
    let mut hourly_rates: Vec<HourlyErrorRate> = Vec::new();
    for hour in 0..24 {
        let keys = hourly_keys.get(&hour).copied().unwrap_or(0);
        let backspaces = hourly_backspaces.get(&hour).copied().unwrap_or(0);
        if keys > 100 {
            hourly_rates.push(HourlyErrorRate {
                hour,
                backspace_rate: (backspaces as f64 / keys as f64) * 100.0,
                keystrokes: keys,
            });
        }
    }
    if hourly_rates.len() < 2 {
        return FatigueIndicators {
            trend: FatigueTrend::Insufficient,
            early_error_rate: 0.0,
            late_error_rate: 0.0,
            hourly_rates,
            recommendation: None,
        };
    }
    hourly_rates.sort_by_key(|rate| rate.hour);
    let mid = hourly_rates.len() / 2;
    let early_avg: f64 = hourly_rates[..mid]
        .iter()
        .map(|rate| rate.backspace_rate)
        .sum::<f64>()
        / mid as f64;
    let late_avg: f64 = hourly_rates[mid..]
        .iter()
        .map(|rate| rate.backspace_rate)
        .sum::<f64>()
        / (hourly_rates.len() - mid) as f64;
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
        _ => None,
    };
    FatigueIndicators {
        trend,
        early_error_rate: early_avg,
        late_error_rate: late_avg,
        hourly_rates,
        recommendation,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Datelike;
    use rusqlite::Connection;

    use super::*;
    use crate::config::SleepConfig;
    use crate::db::init_db;

    fn config_with_credit(enabled: bool) -> Config {
        let mut config = Config::default();
        config.agent_activity.counts_as_productive = enabled;
        config
    }

    fn breakdown(category: Category, total_ms: i64, agent_ms: i64) -> CategoryBreakdown {
        CategoryBreakdown {
            category,
            total_ms,
            active_ms: total_ms,
            idle_ms: 0,
            agent_ms,
        }
    }

    fn total_of(categories: &[CategoryBreakdown], category: Category) -> i64 {
        categories
            .iter()
            .find(|c| c.category == category)
            .map_or(0, |c| c.total_ms)
    }

    #[test]
    fn agent_time_moves_from_unproductive_to_productive() {
        let input = vec![
            breakdown(Category::Productive, 3_600_000, 1_800_000),
            breakdown(Category::Unproductive, 1_200_000, 600_000),
        ];
        let out = reattribute_agent_time(&config_with_credit(true), input);

        assert_eq!(total_of(&out, Category::Productive), 4_200_000);
        assert_eq!(total_of(&out, Category::Unproductive), 600_000);
    }

    #[test]
    fn reattribution_conserves_category_agent_time() {
        let input = vec![
            breakdown(Category::Productive, 3_600_000, 1_000_000),
            breakdown(Category::Unproductive, 1_200_000, 600_000),
        ];

        let out = reattribute_agent_time(&config_with_credit(true), input);

        let productive = out
            .iter()
            .find(|category| category.category == Category::Productive)
            .expect("productive category");
        assert_eq!(productive.agent_ms, 1_600_000);
        assert_eq!(
            out.iter().map(|category| category.agent_ms).sum::<i64>(),
            1_600_000
        );
    }

    #[test]
    fn reattribution_conserves_total_time() {
        let input = vec![
            breakdown(Category::Productive, 3_600_000, 1_000_000),
            breakdown(Category::Unproductive, 1_200_000, 600_000),
            breakdown(Category::Neutral, 900_000, 300_000),
        ];
        let before: i64 = input.iter().map(|c| c.total_ms).sum();
        let after: i64 = reattribute_agent_time(&config_with_credit(true), input)
            .iter()
            .map(|c| c.total_ms)
            .sum();

        assert_eq!(before, after, "moving time must not create or destroy any");
    }

    #[test]
    fn reattribution_preserves_human_state_totals() {
        let input = vec![CategoryBreakdown {
            category: Category::Unproductive,
            total_ms: 600_000,
            active_ms: 60_000,
            idle_ms: 240_000,
            agent_ms: 300_000,
        }];

        let out = reattribute_agent_time(&config_with_credit(true), input);

        assert_eq!(
            out.iter().map(|category| category.active_ms).sum::<i64>(),
            60_000
        );
        assert_eq!(
            out.iter().map(|category| category.idle_ms).sum::<i64>(),
            240_000
        );
    }

    #[test]
    fn credit_rounding_never_creates_a_missing_state() {
        assert_eq!(split_agent_credit(1, (1, 1, 0)), (0, 1, 0));
    }

    #[test]
    fn disabling_the_setting_leaves_categories_untouched() {
        let input = vec![
            breakdown(Category::Productive, 3_600_000, 1_000_000),
            breakdown(Category::Unproductive, 1_200_000, 600_000),
        ];
        let out = reattribute_agent_time(&config_with_credit(false), input);

        assert_eq!(total_of(&out, Category::Productive), 3_600_000);
        assert_eq!(total_of(&out, Category::Unproductive), 1_200_000);
    }

    #[test]
    fn credit_cannot_exceed_the_time_it_comes_from() {
        // agent_ms spans a whole session while total_ms excludes idle, so the
        // raw figure can exceed the bucket it is drawn from.
        let input = vec![breakdown(Category::Unproductive, 500_000, 900_000)];
        let out = reattribute_agent_time(&config_with_credit(true), input);

        assert_eq!(total_of(&out, Category::Unproductive), 0);
        assert_eq!(
            total_of(&out, Category::Productive),
            500_000,
            "credit must clamp to the source category's total"
        );
    }

    #[test]
    fn a_categorys_agent_time_never_exceeds_its_own_total() {
        // Subtracting the credit from total_ms but not agent_ms once produced
        // "8m (agent: 31m 366%)" in a real report.
        let input = vec![
            breakdown(Category::Productive, 3_600_000, 1_000_000),
            breakdown(Category::Unproductive, 1_200_000, 900_000),
        ];
        for cat in reattribute_agent_time(&config_with_credit(true), input) {
            assert!(
                cat.agent_ms <= cat.total_ms,
                "{:?}: agent {} exceeds total {}",
                cat.category,
                cat.agent_ms,
                cat.total_ms
            );
        }
    }

    #[test]
    fn reattribution_preserves_proportional_idle_breakdowns() {
        let mut idle_heavy = breakdown(Category::Unproductive, 1_000, 900);
        idle_heavy.active_ms = 100;
        idle_heavy.idle_ms = 700;

        let out = reattribute_agent_time(&config_with_credit(true), vec![idle_heavy]);
        let unproductive = out
            .iter()
            .find(|category| category.category == Category::Unproductive)
            .expect("unproductive remainder");

        assert_eq!(unproductive.total_ms, 100);
        assert_eq!(unproductive.active_ms, 10);
        assert_eq!(unproductive.idle_ms, 70);
    }

    #[test]
    fn productive_time_is_never_double_counted() {
        let input = vec![breakdown(Category::Productive, 3_600_000, 3_600_000)];
        let out = reattribute_agent_time(&config_with_credit(true), input);
        assert_eq!(total_of(&out, Category::Productive), 3_600_000);
    }

    #[test]
    fn a_productive_bucket_appears_when_none_existed() {
        let input = vec![breakdown(Category::Unproductive, 1_200_000, 600_000)];
        let out = reattribute_agent_time(&config_with_credit(true), input);
        assert_eq!(total_of(&out, Category::Productive), 600_000);
    }

    /// Build a schema identical to a real database.
    ///
    /// Runs the production migrations rather than restating them, so a new
    /// column cannot pass its own tests while breaking every query here.
    fn setup_test_db() -> rusqlite::Result<Connection> {
        let mut conn = Connection::open_in_memory()?;
        let to_sqlite =
            |e: crate::error::Error| rusqlite::Error::ToSqlConversionFailure(Box::new(e));
        init_db(&conn).map_err(to_sqlite)?;
        crate::db::run_migrations(&mut conn, &Config::default()).map_err(to_sqlite)?;
        Ok(conn)
    }

    /// Inserts a test event with configurable fields.
    fn insert_test_event(
        conn: &Connection,
        timestamp: &str,
        app_id: &str,
        category: &str,
        active_ms: i64,
        passive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
        mouse_clicks: i64,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO events (timestamp, app_id, title, category, active_ms, passive_ms, idle_ms, keystrokes, mouse_clicks, scroll_events, mouse_distance, jiggler_detected, backspace_count, modifier_count, left_clicks, right_clicks, middle_clicks, scroll_up, scroll_down, scroll_horizontal)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)",
            params![timestamp, app_id, "", category, active_ms, passive_ms, idle_ms, keystrokes, mouse_clicks],
        )?;
        Ok(())
    }

    struct MeasuredEvent<'a> {
        timestamp: &'a str,
        total_ms: i64,
        agent_ms: i64,
        input_offsets: &'a [u32],
    }

    fn insert_measured_event(conn: &Connection, event: MeasuredEvent<'_>) -> rusqlite::Result<()> {
        let offsets: Vec<u8> = event
            .input_offsets
            .iter()
            .flat_map(|offset| offset.to_le_bytes())
            .collect();
        let has_input = !event.input_offsets.is_empty();
        conn.execute(
            "INSERT INTO events (
                 timestamp, app_id, title, category, active_ms, passive_ms, idle_ms,
                 agent_ms, keystrokes, input_offsets
             ) VALUES (?1, 'foot', '', 'productive', ?2, ?3, 0, ?4, ?5, ?6)",
            params![
                event.timestamp,
                if has_input { event.total_ms } else { 0 },
                if has_input { 0 } else { event.total_ms },
                event.agent_ms,
                i64::from(has_input),
                offsets,
            ],
        )?;
        Ok(())
    }

    /// Creates a test App with in-memory database and default config.
    fn create_test_app() -> App {
        let conn = setup_test_db().expect("failed to create test db");
        let config = Config::default();
        App { config, conn }
    }

    fn create_utc_test_app() -> App {
        let mut app = create_test_app();
        app.config.timezone = Some(chrono_tz::UTC);
        app
    }

    // ==================== classify_gap tests ====================

    #[test]
    fn classify_gap_short_break() {
        let sleep = SleepConfig::default();
        // 1 hour gap during daytime (10am to 11am) = short break
        let result = classify_gap(10, 11, 1.0, &sleep);
        assert_eq!(result, Some(GapType::ShortBreak));
    }

    #[test]
    fn classify_gap_long_break() {
        let sleep = SleepConfig::default();
        // 2.5 hour gap during daytime = long break
        let result = classify_gap(10, 12, 2.5, &sleep);
        assert_eq!(result, Some(GapType::LongBreak));
    }

    #[test]
    fn classify_gap_sleep_overnight() {
        let sleep = SleepConfig::default();
        // 7 hour gap spanning midnight (22:00 to 05:00) = sleep
        let result = classify_gap(22, 5, 7.0, &sleep);
        assert_eq!(result, Some(GapType::Sleep));
    }

    #[test]
    fn classify_gap_sleep_night_hours() {
        let sleep = SleepConfig::default();
        // 4 hour gap starting at night (23:00 to 03:00) = sleep
        let result = classify_gap(23, 3, 4.0, &sleep);
        assert_eq!(result, Some(GapType::Sleep));
    }

    #[test]
    fn classify_gap_too_short() {
        let sleep = SleepConfig::default();
        // 15 minute gap = below minimum, returns None
        let result = classify_gap(10, 10, 0.25, &sleep);
        assert_eq!(result, None);
    }

    #[test]
    fn classify_gap_too_long() {
        let sleep = SleepConfig::default();
        // 30 hour gap = above maximum, returns None
        let result = classify_gap(10, 16, 30.0, &sleep);
        assert_eq!(result, None);
    }

    #[test]
    fn query_gaps_starts_after_previous_event_duration() {
        let app = create_test_app();

        insert_test_event(
            &app.conn,
            "2026-05-25T10:00:00+00:00",
            "code",
            "productive",
            45 * MS_PER_MIN,
            0,
            0,
            100,
            5,
        )
        .expect("insert should succeed");
        insert_test_event(
            &app.conn,
            "2026-05-25T12:00:00+00:00",
            "code",
            "productive",
            MS_PER_MIN,
            0,
            0,
            10,
            1,
        )
        .expect("insert should succeed");

        let away = query_gaps(
            &app.conn,
            &app.config,
            "2026-05-25T00:00:00+00:00",
            "2026-05-26T00:00:00+00:00",
            &app.config.sleep,
        )
        .expect("query should succeed");

        assert_eq!(away.total_away_ms, 75 * MS_PER_MIN);
        assert_eq!(away.summaries.len(), 1);
        assert_eq!(away.summaries[0].gap_type, GapType::ShortBreak);
        assert_eq!(away.summaries[0].count, 1);
        assert_eq!(away.entries.len(), 1);
        assert_eq!(away.entries[0].duration_ms, 75 * MS_PER_MIN);
    }

    #[test]
    fn overnight_agent_work_does_not_hide_human_sleep() {
        let app = create_utc_test_app();
        insert_measured_event(
            &app.conn,
            MeasuredEvent {
                timestamp: "2026-05-25T22:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
        )
        .expect("sleep start input");
        insert_measured_event(
            &app.conn,
            MeasuredEvent {
                timestamp: "2026-05-26T00:00:00+00:00",
                total_ms: 5 * MS_PER_HOUR,
                agent_ms: 5 * MS_PER_HOUR,
                input_offsets: &[],
            },
        )
        .expect("autonomous agent work");
        insert_measured_event(
            &app.conn,
            MeasuredEvent {
                timestamp: "2026-05-26T06:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
        )
        .expect("wake input");

        let away = query_gaps(
            &app.conn,
            &app.config,
            "2026-05-25T00:00:00+00:00",
            "2026-05-27T00:00:00+00:00",
            &app.config.sleep,
        )
        .expect("human gaps");
        let report = query_report_range(
            &app,
            TimeRange::DateRange(
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
                chrono::NaiveDate::from_ymd_opt(2026, 5, 26).expect("valid date"),
            ),
        )
        .expect("report");

        assert_eq!(away.summaries.len(), 1);
        assert_eq!(away.summaries[0].gap_type, GapType::Sleep);
        assert_eq!(away.summaries[0].total_ms, 8 * MS_PER_HOUR);
        assert_eq!(report.agent_ms, 5 * MS_PER_HOUR);
    }

    #[test]
    fn daytime_agent_work_remains_a_long_break_not_sleep() {
        let app = create_utc_test_app();
        for event in [
            MeasuredEvent {
                timestamp: "2026-05-25T10:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
            MeasuredEvent {
                timestamp: "2026-05-25T11:00:00+00:00",
                total_ms: 2 * MS_PER_HOUR,
                agent_ms: 2 * MS_PER_HOUR,
                input_offsets: &[],
            },
            MeasuredEvent {
                timestamp: "2026-05-25T14:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
        ] {
            insert_measured_event(&app.conn, event).expect("measured event");
        }

        let away = query_gaps(
            &app.conn,
            &app.config,
            "2026-05-25T00:00:00+00:00",
            "2026-05-26T00:00:00+00:00",
            &app.config.sleep,
        )
        .expect("human gaps");

        assert_eq!(away.summaries.len(), 1);
        assert_eq!(away.summaries[0].gap_type, GapType::LongBreak);
        assert_eq!(away.summaries[0].total_ms, 4 * MS_PER_HOUR);
    }

    #[test]
    fn legacy_unknown_event_prevents_speculative_sleep() {
        let app = create_utc_test_app();
        for event in [
            MeasuredEvent {
                timestamp: "2026-05-25T22:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
            MeasuredEvent {
                timestamp: "2026-05-26T06:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
        ] {
            insert_measured_event(&app.conn, event).expect("measured boundary");
        }
        insert_test_event(
            &app.conn,
            "2026-05-26T02:00:00+00:00",
            "foot",
            "productive",
            MS_PER_MIN,
            0,
            0,
            0,
            0,
        )
        .expect("legacy unknown event");

        let away = query_gaps(
            &app.conn,
            &app.config,
            "2026-05-25T00:00:00+00:00",
            "2026-05-27T00:00:00+00:00",
            &app.config.sleep,
        )
        .expect("human gaps");

        assert!(away.summaries.is_empty());
    }

    #[test]
    fn corrupt_aligned_offsets_block_sleep_even_with_counted_input() {
        let app = create_utc_test_app();
        for event in [
            MeasuredEvent {
                timestamp: "2026-05-25T22:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
            MeasuredEvent {
                timestamp: "2026-05-26T05:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[9_000_000],
            },
            MeasuredEvent {
                timestamp: "2026-05-26T06:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
        ] {
            insert_measured_event(&app.conn, event).expect("measured event");
        }

        let away = query_gaps(
            &app.conn,
            &app.config,
            "2026-05-25T00:00:00+00:00",
            "2026-05-27T00:00:00+00:00",
            &app.config.sleep,
        )
        .expect("human gaps");

        assert!(
            away.summaries
                .iter()
                .all(|summary| summary.gap_type != GapType::Sleep)
        );
    }

    #[test]
    fn sleep_uses_boundary_evidence_and_clips_to_report_range() {
        let app = create_utc_test_app();
        for event in [
            MeasuredEvent {
                timestamp: "2026-05-25T23:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
            MeasuredEvent {
                timestamp: "2026-05-26T07:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[1_000],
            },
        ] {
            insert_measured_event(&app.conn, event).expect("measured boundary");
        }

        let away = query_gaps(
            &app.conn,
            &app.config,
            "2026-05-26T00:00:00+00:00",
            "2026-05-27T00:00:00+00:00",
            &app.config.sleep,
        )
        .expect("human gaps");

        assert_eq!(away.summaries.len(), 1);
        assert_eq!(away.summaries[0].gap_type, GapType::Sleep);
        assert_eq!(away.summaries[0].total_ms, 7 * MS_PER_HOUR + 1_000);
    }

    #[test]
    fn sparse_overnight_input_does_not_fragment_sleep_into_breaks() {
        let app = create_utc_test_app();
        for timestamp in [
            "2026-05-26T01:00:00+00:00",
            "2026-05-26T03:30:00+00:00",
            "2026-05-26T05:00:00+00:00",
            "2026-05-26T07:00:00+00:00",
        ] {
            insert_measured_event(
                &app.conn,
                MeasuredEvent {
                    timestamp,
                    total_ms: MS_PER_MIN,
                    agent_ms: 0,
                    input_offsets: &[1_000],
                },
            )
            .expect("sparse input point");
        }

        let away = query_gaps(
            &app.conn,
            &app.config,
            "2026-05-26T00:00:00+00:00",
            "2026-05-27T00:00:00+00:00",
            &app.config.sleep,
        )
        .expect("human gaps");

        assert_eq!(away.summaries.len(), 1);
        assert_eq!(away.summaries[0].gap_type, GapType::Sleep);
        assert_eq!(away.summaries[0].total_ms, 6 * MS_PER_HOUR);
    }

    #[test]
    fn report_active_starts_at_input_instead_of_filling_the_stored_bucket() {
        let app = create_utc_test_app();
        insert_measured_event(
            &app.conn,
            MeasuredEvent {
                timestamp: "2026-05-25T10:00:00+00:00",
                total_ms: 5 * MS_PER_MIN,
                agent_ms: 5 * MS_PER_MIN,
                input_offsets: &[4 * MS_PER_MIN as u32],
            },
        )
        .expect("sparse input event");

        let report = query_report_range(
            &app,
            TimeRange::DateRange(
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
            ),
        )
        .expect("report");

        assert_eq!(report.active_ms, MS_PER_MIN);
        assert_eq!(report.passive_ms, 0);
        assert_eq!(report.idle_ms, 4 * MS_PER_MIN);
        assert_eq!(report.agent_ms, 5 * MS_PER_MIN);
        assert_eq!(report.total_events, 1);

        let intervals = load_human_intervals(
            &app.conn,
            &app.config,
            "2026-05-25T10:00:00+00:00",
            "2026-05-25T10:05:00+00:00",
        )
        .expect("human intervals");
        assert_eq!(intervals.len(), 2);
        assert_eq!(intervals[0].idle_ms, 4 * MS_PER_MIN);
        assert_eq!(intervals[1].active_ms, MS_PER_MIN);
    }

    #[test]
    fn measured_input_counters_stay_in_active_segments() {
        let app = create_utc_test_app();
        insert_measured_event(
            &app.conn,
            MeasuredEvent {
                timestamp: "2026-05-25T10:00:00+00:00",
                total_ms: 5 * MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[MS_PER_MIN as u32],
            },
        )
        .expect("measured input");

        let intervals = load_human_intervals(
            &app.conn,
            &app.config,
            "2026-05-25T10:00:00+00:00",
            "2026-05-25T10:05:00+00:00",
        )
        .expect("human intervals");

        let active_keys = intervals
            .iter()
            .filter(|interval| interval.active_ms > 0)
            .map(|interval| interval.keystrokes)
            .sum::<i64>();
        let inactive_keys = intervals
            .iter()
            .filter(|interval| interval.active_ms == 0)
            .map(|interval| interval.keystrokes)
            .sum::<i64>();
        assert_eq!(active_keys, 1);
        assert_eq!(inactive_keys, 0);
    }

    #[test]
    fn measured_agent_only_event_is_human_idle() {
        let app = create_utc_test_app();
        insert_measured_event(
            &app.conn,
            MeasuredEvent {
                timestamp: "2026-05-25T10:00:00+00:00",
                total_ms: 5 * MS_PER_MIN,
                agent_ms: 5 * MS_PER_MIN,
                input_offsets: &[],
            },
        )
        .expect("agent-only event");

        let report = query_report_range(
            &app,
            TimeRange::DateRange(
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
            ),
        )
        .expect("report");

        assert_eq!(report.active_ms, 0);
        assert_eq!(report.passive_ms, 0);
        assert_eq!(report.idle_ms, 5 * MS_PER_MIN);
        assert_eq!(report.agent_ms, 5 * MS_PER_MIN);
    }

    #[test]
    fn category_credit_is_clamped_per_event_before_aggregation() {
        let app = create_utc_test_app();
        for event in [
            MeasuredEvent {
                timestamp: "2026-05-25T10:00:00+00:00",
                total_ms: 100_000,
                agent_ms: 200_000,
                input_offsets: &[],
            },
            MeasuredEvent {
                timestamp: "2026-05-25T10:01:40+00:00",
                total_ms: 100_000,
                agent_ms: 0,
                input_offsets: &[],
            },
        ] {
            insert_measured_event(&app.conn, event).expect("measured event");
        }
        app.conn
            .execute("UPDATE events SET category = 'neutral'", [])
            .expect("neutral fixtures");

        let report = query_report_range(
            &app,
            TimeRange::DateRange(
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
            ),
        )
        .expect("report");
        let metrics = metrics_between(
            &app.conn,
            &app.config,
            "2026-05-25T00:00:00+00:00",
            "2026-05-26T00:00:00+00:00",
        )
        .expect("metrics");

        assert_eq!(report.agent_credited_ms, 100_000);
        assert_eq!(total_of(&report.categories, Category::Productive), 100_000);
        assert_eq!(total_of(&report.categories, Category::Neutral), 100_000);
        assert_eq!(metrics.productive_ms, 100_000);
        assert_eq!(metrics.neutral_ms, 100_000);
    }

    #[test]
    fn human_presence_window_crosses_periodic_flush_boundaries() {
        let app = create_utc_test_app();
        for event in [
            MeasuredEvent {
                timestamp: "2026-05-25T10:00:00+00:00",
                total_ms: 5 * MS_PER_MIN,
                agent_ms: 5 * MS_PER_MIN,
                input_offsets: &[299_000],
            },
            MeasuredEvent {
                timestamp: "2026-05-25T10:05:00+00:00",
                total_ms: 5 * MS_PER_MIN,
                agent_ms: 5 * MS_PER_MIN,
                input_offsets: &[],
            },
        ] {
            insert_measured_event(&app.conn, event).expect("measured event");
        }

        let report = query_report_range(
            &app,
            TimeRange::DateRange(
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
            ),
        )
        .expect("report");

        assert_eq!(report.active_ms, 120_000);
        assert_eq!(report.passive_ms, 180_000);
        assert_eq!(report.idle_ms, 300_000);
        assert_eq!(report.agent_ms, 10 * MS_PER_MIN);

        let first = load_human_intervals(
            &app.conn,
            &app.config,
            "2026-05-25T10:00:00+00:00",
            "2026-05-25T10:05:00+00:00",
        )
        .expect("first half");
        let second = load_human_intervals(
            &app.conn,
            &app.config,
            "2026-05-25T10:05:00+00:00",
            "2026-05-25T10:10:00+00:00",
        )
        .expect("second half");
        let halves = first.into_iter().chain(second).collect::<Vec<_>>();
        assert_eq!(
            halves.iter().map(|event| event.active_ms).sum::<i64>(),
            120_000
        );
        assert_eq!(
            halves.iter().map(|event| event.passive_ms).sum::<i64>(),
            180_000
        );
        assert_eq!(
            halves.iter().map(|event| event.idle_ms).sum::<i64>(),
            300_000
        );
    }

    #[test]
    fn report_preserves_legacy_human_state_without_offsets() {
        let app = create_utc_test_app();
        insert_test_event(
            &app.conn,
            "2026-05-25T10:00:00+00:00",
            "foot",
            "productive",
            MS_PER_MIN,
            0,
            4 * MS_PER_MIN,
            0,
            0,
        )
        .expect("legacy event");

        let report = query_report_range(
            &app,
            TimeRange::DateRange(
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
            ),
        )
        .expect("report");

        assert_eq!(report.active_ms, MS_PER_MIN);
        assert_eq!(report.idle_ms, 4 * MS_PER_MIN);
    }

    #[test]
    fn ambiguous_input_row_does_not_disable_following_measured_state() {
        let app = create_utc_test_app();
        for event in [
            MeasuredEvent {
                timestamp: "2026-05-25T10:00:00+00:00",
                total_ms: MS_PER_MIN,
                agent_ms: 0,
                input_offsets: &[],
            },
            MeasuredEvent {
                timestamp: "2026-05-25T10:01:00+00:00",
                total_ms: 5 * MS_PER_MIN,
                agent_ms: 5 * MS_PER_MIN,
                input_offsets: &[],
            },
        ] {
            insert_measured_event(&app.conn, event).expect("measured event");
        }
        app.conn
            .execute(
                "UPDATE events SET active_ms = 60000, passive_ms = 0, keystrokes = 1 WHERE timestamp = '2026-05-25T10:00:00+00:00'",
                [],
            )
            .expect("ambiguous fixture");

        let report = query_report_range(
            &app,
            TimeRange::DateRange(
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).expect("valid date"),
            ),
        )
        .expect("report");

        assert_eq!(report.active_ms, MS_PER_MIN);
        assert_eq!(report.idle_ms, 5 * MS_PER_MIN);
    }

    #[test]
    fn sparse_input_does_not_create_a_five_minute_focus_streak() {
        let app = create_utc_test_app();
        insert_measured_event(
            &app.conn,
            MeasuredEvent {
                timestamp: "2026-05-25T03:53:00+00:00",
                total_ms: 5 * MS_PER_MIN,
                agent_ms: 5 * MS_PER_MIN,
                input_offsets: &[4 * MS_PER_MIN as u32],
            },
        )
        .expect("sparse input event");

        let events = load_human_intervals(
            &app.conn,
            &app.config,
            "2026-05-25T00:00:00+00:00",
            "2026-05-26T00:00:00+00:00",
        )
        .expect("human intervals");
        let streaks = query_streaks(&events, &app.config);

        assert_eq!(streaks.total_productive_streaks, 0);
    }

    // ==================== group_apps tests ====================

    #[test]
    fn group_apps_single_app() {
        let flat = vec![AppBreakdown {
            app_id: "firefox".to_string(),
            category: Category::Productive,
            total_ms: 3600000,
            active_ms: 3000000,
            keys: 1000,
            clicks: 50,
        }];
        let groups = group_apps(flat, 10);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].app_id, "firefox");
        assert_eq!(groups[0].total_ms, 3600000);
        assert_eq!(groups[0].children.len(), 1);
    }

    #[test]
    fn group_apps_multiple_categories() {
        // Same app with different categories should be grouped together
        let flat = vec![
            AppBreakdown {
                app_id: "firefox".to_string(),
                category: Category::Productive,
                total_ms: 2000000,
                active_ms: 1800000,
                keys: 500,
                clicks: 25,
            },
            AppBreakdown {
                app_id: "firefox".to_string(),
                category: Category::Unproductive,
                total_ms: 1000000,
                active_ms: 900000,
                keys: 100,
                clicks: 10,
            },
        ];
        let groups = group_apps(flat, 10);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].app_id, "firefox");
        assert_eq!(groups[0].total_ms, 3000000);
        assert_eq!(groups[0].keys, 600);
        assert_eq!(groups[0].children.len(), 2);
    }

    #[test]
    fn group_apps_sorted_by_total() {
        let flat = vec![
            AppBreakdown {
                app_id: "small".to_string(),
                category: Category::Neutral,
                total_ms: 100000,
                active_ms: 90000,
                keys: 10,
                clicks: 5,
            },
            AppBreakdown {
                app_id: "large".to_string(),
                category: Category::Productive,
                total_ms: 5000000,
                active_ms: 4500000,
                keys: 2000,
                clicks: 100,
            },
        ];
        let groups = group_apps(flat, 10);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].app_id, "large"); // Sorted by total_ms descending
        assert_eq!(groups[1].app_id, "small");
    }

    #[test]
    fn group_apps_respects_limit() {
        let flat: Vec<AppBreakdown> = (0..20)
            .map(|i| AppBreakdown {
                app_id: format!("app{}", i),
                category: Category::Neutral,
                total_ms: (20 - i) as i64 * 100000,
                active_ms: (20 - i) as i64 * 90000,
                keys: 10,
                clicks: 5,
            })
            .collect();
        let groups = group_apps(flat, 5);
        assert_eq!(groups.len(), 5);
    }

    // ==================== compute_consistency tests ====================

    #[test]
    fn compute_consistency_empty() {
        let rates: Vec<f64> = vec![];
        assert_eq!(compute_consistency(&rates), 50);
    }

    #[test]
    fn compute_consistency_single() {
        let rates = vec![60.0];
        assert_eq!(compute_consistency(&rates), 50);
    }

    #[test]
    fn compute_consistency_uniform() {
        // All same rate = perfect consistency
        let rates = vec![60.0, 60.0, 60.0, 60.0];
        assert_eq!(compute_consistency(&rates), 100);
    }

    #[test]
    fn compute_consistency_variable() {
        // High variance = lower consistency
        let rates = vec![10.0, 100.0, 20.0, 90.0];
        let score = compute_consistency(&rates);
        assert!(
            score < 80,
            "Expected low consistency for variable rates, got {}",
            score
        );
    }

    // ==================== query_today tests ====================

    #[test]
    fn query_today_empty_db() {
        let app = create_test_app();
        let result = query_today(&app).expect("query should succeed");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn query_today_single_event() {
        let app = create_test_app();
        let today = app.config.local_date_today();
        let timestamp = format!("{}T10:00:00+00:00", today);

        insert_test_event(
            &app.conn,
            &timestamp,
            "firefox",
            "productive",
            3600000,
            0,
            0,
            1000,
            50,
        )
        .expect("insert should succeed");

        let result = query_today(&app).expect("query should succeed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].app_id, "firefox");
        assert_eq!(result.rows[0].category, Category::Productive);
        assert_eq!(result.rows[0].total_ms, 3600000);
    }

    #[test]
    fn query_today_multiple_apps() {
        let app = create_test_app();
        let today = app.config.local_date_today();
        let timestamp = format!("{}T10:00:00+00:00", today);

        insert_test_event(
            &app.conn,
            &timestamp,
            "firefox",
            "productive",
            3600000,
            0,
            0,
            1000,
            50,
        )
        .expect("insert should succeed");
        insert_test_event(
            &app.conn, &timestamp, "slack", "neutral", 1800000, 0, 0, 200, 30,
        )
        .expect("insert should succeed");
        insert_test_event(
            &app.conn,
            &timestamp,
            "youtube",
            "unproductive",
            900000,
            0,
            0,
            10,
            5,
        )
        .expect("insert should succeed");

        let result = query_today(&app).expect("query should succeed");
        assert_eq!(result.rows.len(), 3);
        // Should be sorted by total_ms descending
        assert_eq!(result.rows[0].app_id, "firefox");
        assert_eq!(result.rows[1].app_id, "slack");
        assert_eq!(result.rows[2].app_id, "youtube");
    }

    #[test]
    fn query_today_excludes_other_days() {
        let app = create_test_app();
        let today = app.config.local_date_today();
        let yesterday = today - chrono::Duration::days(1);

        let today_ts = format!("{}T10:00:00+00:00", today);
        let yesterday_ts = format!("{}T10:00:00+00:00", yesterday);

        insert_test_event(
            &app.conn,
            &today_ts,
            "today_app",
            "productive",
            1000000,
            0,
            0,
            100,
            10,
        )
        .expect("insert should succeed");
        insert_test_event(
            &app.conn,
            &yesterday_ts,
            "yesterday_app",
            "productive",
            2000000,
            0,
            0,
            200,
            20,
        )
        .expect("insert should succeed");

        let result = query_today(&app).expect("query should succeed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].app_id, "today_app");
    }

    #[test]
    fn query_today_preserves_mixed_application_categories() {
        let app = create_utc_test_app();
        let today = app.config.local_date_today();
        let timestamp = format!("{}T10:00:00+00:00", today);
        insert_test_event(
            &app.conn,
            &timestamp,
            "zen",
            "productive",
            60_000,
            0,
            0,
            0,
            0,
        )
        .expect("productive event");
        insert_test_event(
            &app.conn,
            &timestamp,
            "zen",
            "unproductive",
            1_000,
            0,
            0,
            0,
            0,
        )
        .expect("unproductive event");

        let result = query_today(&app).expect("today");

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].category, Category::Productive);
        assert_eq!(result.rows[0].total_ms, 60_000);
        assert_eq!(result.rows[1].category, Category::Unproductive);
        assert_eq!(result.rows[1].total_ms, 1_000);
    }

    // ==================== query_metrics_range tests ====================

    #[test]
    fn query_metrics_range_empty_db() {
        let app = create_test_app();
        let result = query_metrics_range(&app, TimeRange::Days(7)).expect("query should succeed");
        assert_eq!(result.total_ms, 0);
        assert_eq!(result.productive_ms, 0);
        assert_eq!(result.unproductive_ms, 0);
        assert_eq!(result.neutral_ms, 0);
    }

    #[test]
    fn query_metrics_range_single_day() {
        let app = create_test_app();
        let today = app.config.local_date_today();
        let timestamp = format!("{}T10:00:00+00:00", today);

        insert_test_event(
            &app.conn,
            &timestamp,
            "code",
            "productive",
            3600000,
            600000,
            300000,
            2000,
            100,
        )
        .expect("insert should succeed");

        let result = query_metrics_range(&app, TimeRange::Days(1)).expect("query should succeed");
        assert_eq!(result.productive_ms, 4500000);
        assert_eq!(result.productive_active_ms, 3600000);
        assert_eq!(result.productive_passive_ms, 600000);
        assert_eq!(result.productive_idle_ms, 300000);
    }

    #[test]
    fn query_metrics_range_multi_category() {
        let app = create_test_app();
        let today = app.config.local_date_today();
        let timestamp = format!("{}T10:00:00+00:00", today);

        insert_test_event(
            &app.conn,
            &timestamp,
            "code",
            "productive",
            2000000,
            0,
            0,
            1000,
            50,
        )
        .expect("insert should succeed");
        insert_test_event(
            &app.conn,
            &timestamp,
            "youtube",
            "unproductive",
            1000000,
            0,
            0,
            10,
            5,
        )
        .expect("insert should succeed");
        insert_test_event(
            &app.conn, &timestamp, "slack", "neutral", 500000, 0, 0, 100, 20,
        )
        .expect("insert should succeed");

        let result = query_metrics_range(&app, TimeRange::Days(1)).expect("query should succeed");
        assert_eq!(result.productive_ms, 2000000);
        assert_eq!(result.unproductive_ms, 1000000);
        assert_eq!(result.neutral_ms, 500000);
        assert_eq!(result.total_ms, 3500000);
    }

    #[test]
    fn metrics_and_report_use_the_same_idle_inclusive_totals() {
        let app = create_utc_test_app();
        let today = app.config.local_date_today();
        let timestamp = format!("{}T10:00:00+00:00", today);
        insert_test_event(
            &app.conn,
            &timestamp,
            "code",
            "productive",
            60_000,
            0,
            60_000,
            0,
            0,
        )
        .expect("productive event");
        insert_test_event(
            &app.conn,
            &timestamp,
            "youtube",
            "unproductive",
            60_000,
            0,
            0,
            0,
            0,
        )
        .expect("unproductive event");

        let metrics = query_metrics_range(&app, TimeRange::Days(0)).expect("metrics");
        let report = query_report_range(&app, TimeRange::Days(0)).expect("report");
        let report_productive = report
            .categories
            .iter()
            .find(|category| category.category == Category::Productive)
            .expect("productive category");

        assert_eq!(metrics.total_ms, 180_000);
        assert_eq!(metrics.total_ms, report.total_ms);
        assert_eq!(metrics.productive_ms, 120_000);
        assert_eq!(metrics.productive_ms, report_productive.total_ms);
    }

    #[test]
    fn report_window_clips_an_event_that_started_before_midnight() {
        let app = create_utc_test_app();
        let today = app.config.local_date_today();
        let yesterday = today - chrono::Duration::days(1);
        insert_test_event(
            &app.conn,
            &format!("{}T23:59:00+00:00", yesterday),
            "code",
            "productive",
            120_000,
            0,
            0,
            100,
            0,
        )
        .expect("cross-midnight event");
        app.conn
            .execute(
                "UPDATE events SET backspace_count = 10 WHERE app_id = 'code'",
                [],
            )
            .expect("granular input");
        let range = TimeRange::DateRange(today, today);

        let metrics = query_metrics_range(&app, range.clone()).expect("metrics");
        let report = query_report_range(&app, range).expect("report");

        assert_eq!(metrics.total_ms, 60_000);
        assert_eq!(report.total_ms, 60_000);
        assert_eq!(report.daily.len(), 1);
        assert_eq!(report.daily[0].date, today.to_string());
        assert_eq!(report.daily[0].total_ms, 60_000);
        assert_eq!(report.total_keys, 50);
        assert_eq!(
            report.input_metrics.expect("input metrics").backspace_count,
            5
        );
    }

    #[test]
    fn report_clips_granular_input_at_the_window_end() {
        let app = create_utc_test_app();
        let today = app.config.local_date_today();
        insert_test_event(
            &app.conn,
            &format!("{}T23:59:00+00:00", today),
            "code",
            "productive",
            120_000,
            0,
            0,
            100,
            0,
        )
        .expect("cross-boundary event");
        app.conn
            .execute("UPDATE events SET backspace_count = 10", [])
            .expect("granular input");

        let report = query_report_range(&app, TimeRange::DateRange(today, today)).expect("report");

        assert_eq!(report.total_keys, 50);
        assert_eq!(
            report.input_metrics.expect("input metrics").backspace_count,
            5
        );
    }

    #[test]
    fn report_splits_an_event_at_the_schedule_boundary() {
        let mut app = create_utc_test_app();
        let today = app.config.local_date_today();
        app.config.schedule.enabled = true;
        app.config.schedule.start = "09:00".to_string();
        app.config.schedule.end = "17:00".to_string();
        app.config.schedule.days = vec![today.weekday().to_string()];
        insert_test_event(
            &app.conn,
            &format!("{}T08:59:30+00:00", today),
            "code",
            "productive",
            60_000,
            0,
            0,
            60,
            0,
        )
        .expect("cross-schedule event");

        let report = query_report_range(&app, TimeRange::Days(0)).expect("report");
        let schedule = report.schedule.expect("schedule");

        assert_eq!(schedule.work_total_ms, 30_000);
        assert_eq!(schedule.after_total_ms, 30_000);
        assert_eq!(schedule.work_keys + schedule.after_keys, 60);
    }

    // ==================== query_timeline tests ====================

    #[test]
    fn query_timeline_empty_db() {
        let app = create_test_app();
        let result = query_timeline(&app, 0, 15).expect("query should succeed");
        assert!(result.buckets.is_empty());
    }

    #[test]
    fn query_timeline_zero_bucket_size_error() {
        let app = create_test_app();
        let result = query_timeline(&app, 0, 0);
        assert!(result.is_err());
    }

    #[test]
    fn query_timeline_buckets_events() {
        let app = create_test_app();
        let today = app.config.local_date_today();

        // Insert events at different times
        insert_test_event(
            &app.conn,
            &format!("{}T10:00:00+00:00", today),
            "code",
            "productive",
            900000, // 15 min
            0,
            0,
            500,
            25,
        )
        .expect("insert should succeed");

        insert_test_event(
            &app.conn,
            &format!("{}T10:30:00+00:00", today),
            "slack",
            "neutral",
            600000, // 10 min
            0,
            0,
            100,
            10,
        )
        .expect("insert should succeed");

        let result = query_timeline(&app, 0, 30).expect("query should succeed");
        // Should have buckets for the events
        assert!(!result.buckets.is_empty());
        assert_eq!(result.bucket_min, 30);
    }

    #[test]
    fn query_timeline_splits_events_across_bucket_boundaries() {
        let app = create_utc_test_app();
        let today = app.config.local_date_today();
        insert_test_event(
            &app.conn,
            &format!("{}T10:55:00+00:00", today),
            "code",
            "productive",
            600_000,
            0,
            0,
            100,
            0,
        )
        .expect("cross-bucket event");

        let result = query_timeline(&app, 0, 15).expect("timeline");

        assert_eq!(result.buckets.len(), 2);
        assert_eq!(result.buckets[0].hour, 10);
        assert_eq!(result.buckets[0].minute, 45);
        assert_eq!(result.buckets[0].productive_ms, 300_000);
        assert_eq!(result.buckets[1].hour, 11);
        assert_eq!(result.buckets[1].minute, 0);
        assert_eq!(result.buckets[1].productive_ms, 300_000);
        assert_eq!(
            result
                .buckets
                .iter()
                .map(|bucket| bucket.keystrokes)
                .sum::<i64>(),
            100
        );
    }

    // ==================== query_report_range tests ====================

    #[test]
    fn query_report_range_empty_db() {
        let app = create_test_app();
        let result = query_report_range(&app, TimeRange::Days(7)).expect("query should succeed");
        assert_eq!(result.total_ms, 0);
        assert_eq!(result.total_events, 0);
        assert!(result.categories.is_empty());
        assert!(result.top_apps.is_empty());
    }

    #[test]
    fn query_report_range_with_data() {
        let app = create_test_app();
        let today = app.config.local_date_today();
        let timestamp = format!("{}T10:00:00+00:00", today);

        insert_test_event(
            &app.conn,
            &timestamp,
            "code",
            "productive",
            3600000,
            0,
            0,
            2000,
            100,
        )
        .expect("insert should succeed");
        insert_test_event(
            &app.conn,
            &timestamp,
            "firefox",
            "productive",
            1800000,
            0,
            0,
            500,
            50,
        )
        .expect("insert should succeed");

        let result = query_report_range(&app, TimeRange::Days(1)).expect("query should succeed");
        assert_eq!(result.total_events, 2);
        assert_eq!(result.total_ms, 5400000);
        assert_eq!(result.total_keys, 2500);
        assert_eq!(result.total_clicks, 150);
        assert!(!result.categories.is_empty());
        assert!(!result.top_apps.is_empty());
    }

    #[test]
    fn query_report_range_daily_breakdown() {
        let app = create_test_app();
        let today = app.config.local_date_today();
        let yesterday = today - chrono::Duration::days(1);

        insert_test_event(
            &app.conn,
            &format!("{}T10:00:00+00:00", today),
            "code",
            "productive",
            2000000,
            0,
            0,
            1000,
            50,
        )
        .expect("insert should succeed");
        insert_test_event(
            &app.conn,
            &format!("{}T10:00:00+00:00", yesterday),
            "code",
            "productive",
            3000000,
            0,
            0,
            1500,
            75,
        )
        .expect("insert should succeed");

        let result = query_report_range(&app, TimeRange::Days(2)).expect("query should succeed");
        assert_eq!(result.daily.len(), 2);
    }
    #[test]
    fn projects_preserve_pure_and_mixed_category_totals() {
        let app = create_utc_test_app();
        let today = app.config.local_date_today();
        let timestamp = format!("{}T10:00:00+00:00", today);
        for (app_id, category, duration) in [
            ("pure-app", "neutral", 60_000),
            ("mixed-productive", "productive", 120_000),
            ("mixed-unproductive", "unproductive", 30_000),
        ] {
            insert_test_event(
                &app.conn, &timestamp, app_id, category, duration, 0, 0, 0, 0,
            )
            .expect("project event");
        }
        app.conn
            .execute(
                "UPDATE events SET project = CASE WHEN app_id = 'pure-app' THEN 'pure-project' ELSE 'mixed-project' END",
                [],
            )
            .expect("projects");

        let report = query_report_range(&app, TimeRange::Days(0)).expect("report");
        let pure = report
            .projects
            .iter()
            .find(|project| project.project == "pure-project")
            .expect("pure project");
        assert_eq!(pure.total_ms, 60_000);
        assert_eq!(pure.productive_ms, 0);
        assert_eq!(pure.neutral_ms, 60_000);
        assert_eq!(pure.unproductive_ms, 0);

        let mixed = report
            .projects
            .iter()
            .find(|project| project.project == "mixed-project")
            .expect("mixed project");
        assert_eq!(mixed.total_ms, 150_000);
        assert_eq!(mixed.productive_ms, 120_000);
        assert_eq!(mixed.neutral_ms, 0);
        assert_eq!(mixed.unproductive_ms, 30_000);
    }

    #[test]
    fn fatigue_uses_input_clipped_at_the_report_boundary() {
        let app = create_utc_test_app();
        let today = app.config.local_date_today();
        let yesterday = today - chrono::Duration::days(1);
        insert_test_event(
            &app.conn,
            &format!("{}T23:59:00+00:00", yesterday),
            "code",
            "productive",
            120_000,
            0,
            0,
            400,
            0,
        )
        .expect("boundary event");
        app.conn
            .execute("UPDATE events SET backspace_count = 40", [])
            .expect("granular input");

        let report = query_report_range(&app, TimeRange::DateRange(today, today)).expect("report");
        let fatigue = report.fatigue.expect("fatigue");

        assert_eq!(fatigue.hourly_rates.len(), 1);
        assert_eq!(fatigue.hourly_rates[0].hour, 0);
        assert_eq!(fatigue.hourly_rates[0].keystrokes, 200);
        assert!((fatigue.hourly_rates[0].backspace_rate - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fatigue_splits_input_across_hour_boundaries() {
        let app = create_utc_test_app();
        let today = app.config.local_date_today();
        insert_test_event(
            &app.conn,
            &format!("{}T10:59:00+00:00", today),
            "code",
            "productive",
            120_000,
            0,
            0,
            400,
            0,
        )
        .expect("cross-hour event");
        app.conn
            .execute("UPDATE events SET backspace_count = 40", [])
            .expect("granular input");

        let report = query_report_range(&app, TimeRange::Days(0)).expect("report");
        let fatigue = report.fatigue.expect("fatigue");

        assert_eq!(fatigue.hourly_rates.len(), 2);
        assert_eq!(fatigue.hourly_rates[0].hour, 10);
        assert_eq!(fatigue.hourly_rates[0].keystrokes, 200);
        assert_eq!(fatigue.hourly_rates[1].hour, 11);
        assert_eq!(fatigue.hourly_rates[1].keystrokes, 200);
        assert!(
            fatigue
                .hourly_rates
                .iter()
                .all(|rate| (rate.backspace_rate - 10.0).abs() < f64::EPSILON)
        );
    }
}
