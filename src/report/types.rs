use chrono::NaiveDate;
use serde::Serialize;

use crate::config::Category;

pub struct TodayRow {
    pub app_id: String,
    pub category: Category,
    pub total_ms: i64,
}

pub struct TodayData {
    pub date: NaiveDate,
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
    pub date: NaiveDate,
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
    pub fn label(self) -> &'static str {
        match self {
            Self::Sleep => "Sleep",
            Self::LongBreak => "Long Break",
            Self::ShortBreak => "Short Break",
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

#[derive(Default)]
#[allow(clippy::struct_field_names)] // Fields share `_ms` suffix for unit clarity
pub(crate) struct Metrics {
    pub(crate) total_ms: i64,
    pub(crate) productive_ms: i64,
    pub(crate) unproductive_ms: i64,
    pub(crate) neutral_ms: i64,
    pub(crate) productive_active_ms: i64,
    pub(crate) productive_passive_ms: i64,
    pub(crate) productive_idle_ms: i64,
}
