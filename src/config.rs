use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::{fmt, fs};

use chrono::{DateTime, Datelike, Local, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Productive,
    Unproductive,
    Neutral,
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Category::Productive => write!(f, "productive"),
            Category::Unproductive => write!(f, "unproductive"),
            Category::Neutral => write!(f, "neutral"),
        }
    }
}

impl FromStr for Category {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "productive" => Ok(Category::Productive),
            "unproductive" => Ok(Category::Unproductive),
            "neutral" => Ok(Category::Neutral),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawTitleRule {
    pattern: String,
    category: Category,
    #[serde(default)]
    app: Vec<String>,
    #[serde(default)]
    regex: bool,
}

#[derive(Debug)]
pub struct TitleRule {
    pub pattern: String,
    pub category: Category,
    pub app: Vec<String>,
    pub compiled: Option<regex::Regex>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JigglerConfig {
    #[serde(default = "default_jiggler_enabled")]
    pub enabled: bool,
    /// Observation window in seconds (default: 600 = 10 min).
    #[serde(default = "default_jiggler_window")]
    pub window_secs: u64,
    /// Minimum events in the window before detection triggers (default: 5).
    #[serde(default = "default_jiggler_min_events")]
    pub min_events: usize,
    /// If max_interval - min_interval < this (ms), flag as artificial (default:
    /// 100).
    #[serde(default = "default_jiggler_variance")]
    pub variance_threshold_ms: u64,
    /// Process names to flag as jiggler software.
    #[serde(default = "default_jiggler_blacklist")]
    pub process_blacklist: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Schedule {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_schedule_start")]
    pub start: String,
    #[serde(default = "default_schedule_end")]
    pub end: String,
    #[serde(default = "default_schedule_days")]
    pub days: Vec<String>,
    #[serde(default = "default_holidays")]
    pub holidays: Vec<String>,
}

/// Agent activity detection configuration.
/// Treats AI agent activity (streaming, tool use) as "active" time even without
/// keyboard input. Also detects build processes (cargo, npm, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentActivityConfig {
    #[serde(default = "default_agent_enabled")]
    pub enabled: bool,

    #[serde(default = "default_agent_poll_interval")]
    pub poll_interval_secs: u64,

    #[serde(default = "default_agent_activity_window")]
    pub activity_window_secs: u64,

    #[serde(default = "default_agent_databases")]
    pub databases: Vec<String>,

    /// Process names to treat as "active" when running (builds, compiles).
    #[serde(default = "default_process_whitelist")]
    pub process_whitelist: Vec<String>,

    /// Only count process activity if user had input within this many seconds.
    /// Prevents counting background processes when user is away. (default: 300
    /// = 5 min)
    #[serde(default = "default_process_recency_secs")]
    pub process_recency_secs: u64,
}

fn default_agent_enabled() -> bool {
    true
}

fn default_agent_poll_interval() -> u64 {
    5
}

fn default_agent_activity_window() -> u64 {
    30
}

fn default_agent_databases() -> Vec<String> {
    vec!["~/.local/share/opencode/opencode.db".to_string()]
}

fn default_process_whitelist() -> Vec<String> {
    [
        // Rust ecosystem
        "cargo",
        "rustc",
        "rustup",
        "clippy-driver",
        "rust-analyzer",
        "rustfmt",
        "rls",
        "rust-gdb",
        "rust-lldb",
        "miri",
        "sccache",
        // JavaScript/TypeScript
        "npm",
        "node",
        "bun",
        "deno",
        "pnpm",
        "yarn",
        "tsc",
        "esbuild",
        "turbo",
        "swc",
        // C/C++ compilers and build systems
        "gcc",
        "g++",
        "clang",
        "clang++",
        "make",
        "cmake",
        "ninja",
        "meson",
        "ccache",
        // Linkers
        "ld",
        "lld",
        "mold",
        "gold",
        // Go
        "go",
        // Python
        "python",
        "python3",
        "pip",
        "uv",
        "poetry",
        "ruff",
        "hatch",
        "wheel",
        "pytest",
        "mypy",
        "pyright",
        // Java/JVM
        "java",
        "javac",
        "gradle",
        "mvn",
        // .NET
        "dotnet",
        // Ruby
        "ruby",
        "gem",
        "bundle",
        // Other languages
        "perl",
        "php",
        "composer",
        "elixir",
        "mix",
        "zig",
        // Containers
        "docker",
        "podman",
        // Git
        "git",
        // Debuggers and profilers
        "gdb",
        "lldb",
        "valgrind",
        "strace",
        "ltrace",
        // Binary utilities
        "as",
        "ar",
        "nm",
        "objdump",
        "objcopy",
        "strip",
        // Gentoo package management
        "emerge",
        "portage",
        "eix",
        "ebuild",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

fn default_process_recency_secs() -> u64 {
    5 * 60
}

impl Default for AgentActivityConfig {
    fn default() -> Self {
        Self {
            enabled: default_agent_enabled(),
            poll_interval_secs: default_agent_poll_interval(),
            activity_window_secs: default_agent_activity_window(),
            databases: default_agent_databases(),
            process_whitelist: default_process_whitelist(),
            process_recency_secs: default_process_recency_secs(),
        }
    }
}

/// Sleep / gap classification configuration.
/// Controls how activity gaps are classified as Sleep vs LongBreak vs
/// ShortBreak.
#[derive(Debug, Clone, Deserialize)]
pub struct SleepConfig {
    /// Earliest hour (24h) that can start a sleep period (default: 18 = 6pm).
    #[serde(default = "default_sleep_earliest_hour")]
    pub earliest_hour: u32,

    /// Latest hour (24h) that can start a sleep period (default: 8 = 8am).
    #[serde(default = "default_sleep_latest_hour")]
    pub latest_hour: u32,

    /// Minimum gap hours to classify as sleep (default: 3.0 - industry
    /// standard).
    #[serde(default = "default_sleep_min_hours")]
    pub min_hours: f64,

    /// Minimum gap hours to classify as long break (default: 2.0).
    #[serde(default = "default_long_break_min_hours")]
    pub long_break_min_hours: f64,

    /// Minimum gap hours to report at all (default: 0.5).
    #[serde(default = "default_gap_min_hours")]
    pub gap_min_hours: f64,

    /// Maximum gap hours to report (default: 24.0).
    #[serde(default = "default_gap_max_hours")]
    pub gap_max_hours: f64,

    /// Merge sleep gaps separated by activity shorter than this (minutes).
    /// 0 = disabled. Default: 30.
    #[serde(default = "default_sleep_merge_window_min")]
    pub merge_window_min: u32,

    /// Any gap longer than this that spans midnight is always sleep (hours).
    /// Default: 6.0. Set to 0 to disable.
    #[serde(default = "default_overnight_auto_hours")]
    pub overnight_auto_hours: f64,
}

fn default_holidays() -> Vec<String> {
    vec![
        "2025-01-01".into(), // New Year's Day
        "2025-01-20".into(), // MLK Jr. Day
        "2025-02-17".into(), // Presidents Day
        "2025-04-18".into(), // Good Friday
        "2025-05-26".into(), // Memorial Day
        "2025-06-19".into(), // Juneteenth
        "2025-07-04".into(), // Independence Day
        "2025-09-01".into(), // Labor Day
        "2025-11-27".into(), // Thanksgiving
        "2025-12-25".into(), // Christmas
        "2026-01-01".into(), // New Year's Day
        "2026-01-19".into(), // MLK Jr. Day
        "2026-02-16".into(), // Presidents Day
        "2026-04-03".into(), // Good Friday
        "2026-05-25".into(), // Memorial Day
        "2026-06-19".into(), // Juneteenth
        "2026-07-03".into(), // Independence Day (observed)
        "2026-09-07".into(), // Labor Day
        "2026-11-26".into(), // Thanksgiving
        "2026-12-25".into(), // Christmas
        "2027-01-01".into(), // New Year's Day
        "2027-01-18".into(), // MLK Jr. Day
        "2027-02-15".into(), // Presidents Day
        "2027-03-26".into(), // Good Friday
        "2027-05-31".into(), // Memorial Day
        "2027-06-18".into(), // Juneteenth (observed)
        "2027-07-05".into(), // Independence Day (observed)
        "2027-09-06".into(), // Labor Day
        "2027-11-25".into(), // Thanksgiving
        "2027-12-24".into(), // Christmas (observed)
    ]
}

#[derive(Debug, Deserialize, Default)]
pub struct Goals {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_daily_goal")]
    pub daily: String,
    #[serde(default = "default_weekly_goal")]
    pub weekly: String,
}

#[derive(Deserialize, Default)]
pub struct Email {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    smtp_user: String,
    #[serde(default)]
    smtp_password: String,
    #[serde(default)]
    pub from_address: String,
    #[serde(default)]
    pub to_addresses: Vec<String>,
    #[serde(default)]
    pub cc_addresses: Vec<String>,
    #[serde(default)]
    pub subject_prefix: String,
    #[serde(default)]
    pub report_name: String,
}

impl Email {
    pub fn smtp_user(&self) -> String {
        std::env::var("NIRI_SMTP_USER").unwrap_or_else(|_| self.smtp_user.clone())
    }

    pub fn smtp_password(&self) -> String {
        std::env::var("NIRI_SMTP_PASSWORD").unwrap_or_else(|_| self.smtp_password.clone())
    }
}

fn mask_email(addr: &str) -> String {
    if let Some(at_pos) = addr.find('@') {
        let local = &addr[..at_pos];
        let domain = &addr[at_pos + 1..];
        let masked_local = if local.len() <= 1 {
            "*".to_string()
        } else {
            format!("{}***", &local[..1])
        };
        format!("{}@{}", masked_local, domain)
    } else {
        "[REDACTED]".to_string()
    }
}

impl fmt::Debug for Email {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let masked_from = mask_email(&self.from_address);
        let masked_to: Vec<String> = self.to_addresses.iter().map(|a| mask_email(a)).collect();
        let masked_cc: Vec<String> = self.cc_addresses.iter().map(|a| mask_email(a)).collect();
        f.debug_struct("Email")
            .field("enabled", &self.enabled)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("smtp_user", &"[REDACTED]")
            .field("smtp_password", &"[REDACTED]")
            .field("from_address", &masked_from)
            .field("to_addresses", &masked_to)
            .field("cc_addresses", &masked_cc)
            .field("subject_prefix", &self.subject_prefix)
            .field("report_name", &self.report_name)
            .finish()
    }
}

fn default_smtp_port() -> u16 {
    587
}

fn default_daily_goal() -> String {
    "8h".to_string()
}

fn default_weekly_goal() -> String {
    "40h".to_string()
}

impl Goals {
    pub fn daily_ms(&self) -> Option<i64> {
        parse_duration_ms(&self.daily)
    }

    pub fn weekly_ms(&self) -> Option<i64> {
        parse_duration_ms(&self.weekly)
    }
}

fn parse_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    // Reject negative durations
    if s.contains('-') {
        return None;
    }
    let mut total_ms: i64 = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            current_num.push(c);
        } else if !current_num.is_empty() {
            let num: f64 = current_num.parse().ok()?;
            current_num.clear();
            match c {
                'h' => {
                    let ms = num * 3600.0 * 1000.0;
                    if ms.is_finite() && ms >= 0.0 && ms <= i64::MAX as f64 {
                        total_ms = total_ms.checked_add(ms as i64)?;
                    } else {
                        return None;
                    }
                }
                'm' => {
                    let ms = num * 60.0 * 1000.0;
                    if ms.is_finite() && ms >= 0.0 && ms <= i64::MAX as f64 {
                        total_ms = total_ms.checked_add(ms as i64)?;
                    } else {
                        return None;
                    }
                }
                's' => {
                    let ms = num * 1000.0;
                    if ms.is_finite() && ms >= 0.0 && ms <= i64::MAX as f64 {
                        total_ms = total_ms.checked_add(ms as i64)?;
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        }
    }

    // If there are trailing digits without a unit suffix, the input is malformed
    if !current_num.is_empty() {
        return None;
    }

    if total_ms > 0 { Some(total_ms) } else { None }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default = "default_idle_threshold")]
    idle_threshold_secs: u64,
    #[serde(default = "default_deep_idle")]
    deep_idle_secs: u64,
    #[serde(default = "default_away_threshold")]
    away_threshold_secs: u64,
    #[serde(default = "default_mouse_dpi")]
    mouse_dpi: f64,
    #[serde(default = "default_mouse_idle_threshold")]
    mouse_idle_threshold: u64,
    /// Seconds of zero meaningful input (keystrokes + clicks) before active
    /// time is reclassified as passive at flush boundaries. Prevents
    /// false-active from compositor events (e.g. Firefox video inhibiting
    /// idle). Default: 60.
    #[serde(default = "default_input_active_secs")]
    input_active_secs: u64,
    #[serde(default = "default_streak_break_tolerance")]
    streak_break_tolerance_secs: u64,
    #[serde(default = "default_streak_idle_timeout")]
    streak_idle_timeout_secs: u64,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    schedule: Schedule,
    #[serde(default)]
    goals: Goals,
    #[serde(default)]
    email: Email,
    #[serde(default)]
    jiggler: JigglerConfig,
    #[serde(default)]
    sleep: SleepConfig,
    #[serde(default)]
    agent_activity: AgentActivityConfig,
    #[serde(default)]
    categories: HashMap<String, Category>,
    #[serde(default)]
    title_rules: Vec<RawTitleRule>,
}

#[derive(Debug)]
pub struct Config {
    pub idle_threshold_secs: u64,
    pub deep_idle_secs: u64,
    pub away_threshold_secs: u64,
    pub mouse_dpi: f64,
    pub mouse_idle_threshold: u64,
    pub input_active_secs: u64,
    pub streak_break_tolerance_secs: u64,
    pub streak_idle_timeout_secs: u64,
    pub timezone: Option<Tz>,
    pub schedule: Schedule,
    pub goals: Goals,
    pub email: Email,
    pub jiggler: JigglerConfig,
    pub sleep: SleepConfig,
    pub agent_activity: AgentActivityConfig,
    pub categories: HashMap<String, Category>,
    pub title_rules: Vec<TitleRule>,
}

fn default_idle_threshold() -> u64 {
    120
}

fn default_deep_idle() -> u64 {
    300
}

fn default_away_threshold() -> u64 {
    1800
}

fn default_mouse_dpi() -> f64 {
    800.0
}

fn default_mouse_idle_threshold() -> u64 {
    50
}

fn default_input_active_secs() -> u64 {
    60
}

fn default_streak_break_tolerance() -> u64 {
    120
}

fn default_streak_idle_timeout() -> u64 {
    300
}

// Sleep classification defaults
fn default_sleep_earliest_hour() -> u32 {
    18 // 6pm
}

fn default_sleep_latest_hour() -> u32 {
    8 // 8am
}

fn default_sleep_min_hours() -> f64 {
    3.0 // Industry standard (COROS, Oura)
}

fn default_long_break_min_hours() -> f64 {
    2.0
}

fn default_gap_min_hours() -> f64 {
    0.5 // 30 minutes
}

fn default_gap_max_hours() -> f64 {
    24.0
}

fn default_sleep_merge_window_min() -> u32 {
    30 // Merge sleep gaps separated by <30 min activity
}

fn default_overnight_auto_hours() -> f64 {
    6.0 // Any 6h+ gap spanning midnight = sleep
}

fn default_jiggler_enabled() -> bool {
    true
}

fn default_jiggler_window() -> u64 {
    600
}

fn default_jiggler_min_events() -> usize {
    10
}

fn default_jiggler_variance() -> u64 {
    100
}

fn default_jiggler_blacklist() -> Vec<String> {
    [
        "xdotool",
        "ydotool",
        "xdg-screensaver",
        "caffeine",
        "keep-presence",
        "mouse-jiggler",
        "movemouse",
        "jiggler",
        "wiggle",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

fn default_schedule_start() -> String {
    "09:00".to_string()
}

fn default_schedule_end() -> String {
    "17:00".to_string()
}

fn default_schedule_days() -> Vec<String> {
    vec![
        "Mon".to_string(),
        "Tue".to_string(),
        "Wed".to_string(),
        "Thu".to_string(),
        "Fri".to_string(),
    ]
}

impl From<RawConfig> for Config {
    fn from(raw: RawConfig) -> Self {
        let title_rules = raw
            .title_rules
            .into_iter()
            .filter_map(|r| {
                let compiled = if r.regex {
                    match regex::RegexBuilder::new(&r.pattern)
                        .case_insensitive(true)
                        .build()
                    {
                        Ok(re) => Some(re),
                        Err(e) => {
                            tracing::warn!("Skipping title rule with invalid regex: {}", e);
                            return None;
                        }
                    }
                } else {
                    None
                };
                Some(TitleRule {
                    pattern: r.pattern,
                    category: r.category,
                    app: r.app,
                    compiled,
                })
            })
            .collect();
        let timezone = raw.timezone.and_then(|tz_str| {
            if let Ok(tz) = tz_str.parse::<Tz>() {
                Some(tz)
            } else {
                tracing::warn!(
                    "Warning: invalid timezone '{}', using system timezone",
                    tz_str
                );
                None
            }
        });

        Self {
            idle_threshold_secs: raw.idle_threshold_secs,
            deep_idle_secs: raw.deep_idle_secs,
            away_threshold_secs: raw.away_threshold_secs,
            mouse_dpi: raw.mouse_dpi,
            mouse_idle_threshold: raw.mouse_idle_threshold,
            input_active_secs: raw.input_active_secs,
            streak_break_tolerance_secs: raw.streak_break_tolerance_secs,
            streak_idle_timeout_secs: raw.streak_idle_timeout_secs,
            timezone,
            schedule: raw.schedule,
            goals: raw.goals,
            email: raw.email,
            jiggler: raw.jiggler,
            sleep: raw.sleep,
            agent_activity: raw.agent_activity,
            categories: raw.categories,
            title_rules,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_threshold_secs: default_idle_threshold(),
            deep_idle_secs: default_deep_idle(),
            away_threshold_secs: default_away_threshold(),
            mouse_dpi: default_mouse_dpi(),
            mouse_idle_threshold: default_mouse_idle_threshold(),
            input_active_secs: default_input_active_secs(),
            streak_break_tolerance_secs: default_streak_break_tolerance(),
            streak_idle_timeout_secs: default_streak_idle_timeout(),
            timezone: None,
            schedule: Schedule::default(),
            goals: Goals::default(),
            email: Email::default(),
            jiggler: JigglerConfig::default(),
            sleep: SleepConfig::default(),
            agent_activity: AgentActivityConfig::default(),
            categories: HashMap::new(),
            title_rules: Vec::new(),
        }
    }
}

impl Default for JigglerConfig {
    fn default() -> Self {
        Self {
            enabled: default_jiggler_enabled(),
            window_secs: default_jiggler_window(),
            min_events: default_jiggler_min_events(),
            variance_threshold_ms: default_jiggler_variance(),
            process_blacklist: default_jiggler_blacklist(),
        }
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            enabled: false,
            start: default_schedule_start(),
            end: default_schedule_end(),
            days: default_schedule_days(),
            holidays: default_holidays(),
        }
    }
}

impl Default for SleepConfig {
    fn default() -> Self {
        Self {
            earliest_hour: default_sleep_earliest_hour(),
            latest_hour: default_sleep_latest_hour(),
            min_hours: default_sleep_min_hours(),
            long_break_min_hours: default_long_break_min_hours(),
            gap_min_hours: default_gap_min_hours(),
            gap_max_hours: default_gap_max_hours(),
            merge_window_min: default_sleep_merge_window_min(),
            overnight_auto_hours: default_overnight_auto_hours(),
        }
    }
}

fn glob_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    if pattern == "*" {
        return true;
    }
    match (pattern.starts_with('*'), pattern.ends_with('*')) {
        (true, true) => {
            let inner = pattern.trim_start_matches('*').trim_end_matches('*');
            value.contains(inner)
        }
        (true, false) => {
            let suffix = pattern.trim_start_matches('*');
            value.ends_with(suffix)
        }
        (false, true) => {
            let prefix = pattern.trim_end_matches('*');
            value.starts_with(prefix)
        }
        (false, false) => {
            if let Some((prefix, suffix)) = pattern.split_once('*') {
                prefix.len() + suffix.len() <= value.len()
                    && value.starts_with(prefix)
                    && value.ends_with(suffix)
            } else {
                pattern == value
            }
        }
    }
}

impl Config {
    pub fn local_now(&self) -> DateTime<chrono::FixedOffset> {
        let utc_now = chrono::Utc::now();
        if let Some(tz) = self.timezone {
            utc_now.with_timezone(&tz).fixed_offset()
        } else {
            utc_now.with_timezone(&Local).fixed_offset()
        }
    }

    pub fn local_date_today(&self) -> chrono::NaiveDate {
        self.local_now().date_naive()
    }

    pub fn day_start_utc(&self, date: chrono::NaiveDate) -> Option<chrono::DateTime<chrono::Utc>> {
        let midnight = date.and_hms_opt(0, 0, 0)?;
        let local_dt = if let Some(tz) = self.timezone {
            match tz.from_local_datetime(&midnight) {
                chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
                    dt.with_timezone(&chrono::Utc)
                }
                chrono::LocalResult::None => return None,
            }
        } else {
            match midnight.and_local_timezone(Local) {
                chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
                    dt.with_timezone(&chrono::Utc)
                }
                chrono::LocalResult::None => return None,
            }
        };
        Some(local_dt)
    }

    pub fn day_end_utc(&self, date: chrono::NaiveDate) -> Option<chrono::DateTime<chrono::Utc>> {
        let next_day = date.succ_opt()?;
        self.day_start_utc(next_day)
    }

    pub fn parse_timestamp_to_local(
        &self,
        value: &str,
    ) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        let utc_dt = chrono::DateTime::parse_from_rfc3339(value).ok()?;
        if let Some(tz) = self.timezone {
            Some(utc_dt.with_timezone(&tz).fixed_offset())
        } else {
            Some(utc_dt.with_timezone(&Local).fixed_offset())
        }
    }

    fn app_category(&self, app_id: &str) -> Option<Category> {
        if let Some(&cat) = self.categories.get(app_id) {
            return Some(cat);
        }
        for (pattern, &cat) in &self.categories {
            if pattern.contains('*') && glob_match(pattern, app_id) {
                return Some(cat);
            }
        }
        None
    }

    pub fn classify(&self, app_id: &str, title: &str) -> Category {
        let title_lower = title.to_lowercase();
        let explicit_cat = self.app_category(app_id);

        for rule in &self.title_rules {
            let scoped = !rule.app.is_empty();

            if scoped && !rule.app.iter().any(|a| a.eq_ignore_ascii_case(app_id)) {
                continue;
            }

            if !scoped && explicit_cat.is_some() {
                continue;
            }

            let matched = if let Some(re) = &rule.compiled {
                re.is_match(title)
            } else {
                title_lower.contains(&rule.pattern.to_lowercase())
            };
            if matched {
                return rule.category;
            }
        }

        explicit_cat.unwrap_or(Category::Neutral)
    }
}

impl Schedule {
    pub fn is_in_schedule<Tz: chrono::TimeZone>(&self, dt: &DateTime<Tz>) -> bool {
        if !self.enabled {
            return true;
        }

        let weekday = dt.weekday().to_string();
        let matches_day = self
            .days
            .iter()
            .any(|day| day.eq_ignore_ascii_case(&weekday));
        if !matches_day {
            return false;
        }

        let start = match NaiveTime::parse_from_str(&self.start, "%H:%M") {
            Ok(time) => time,
            Err(_) => return false,
        };
        let end = match NaiveTime::parse_from_str(&self.end, "%H:%M") {
            Ok(time) => time,
            Err(_) => return false,
        };
        let current = dt.time();

        if start <= end {
            current >= start && current <= end
        } else {
            current >= start || current <= end
        }
    }

    pub fn count_workdays(
        &self,
        start_date: chrono::NaiveDate,
        end_date: chrono::NaiveDate,
    ) -> i64 {
        const MAX_ITERATIONS: i64 = 36_600;
        let holiday_set = self.holiday_set();
        let mut count = 0i64;
        let mut iterations = 0i64;
        let mut date = start_date;
        while date <= end_date {
            if self.is_workday_with_holidays(date, &holiday_set) {
                count = count.saturating_add(1);
            }
            date += chrono::Duration::days(1);
            iterations += 1;
            if iterations >= MAX_ITERATIONS {
                break;
            }
        }
        count
    }

    pub(crate) fn holiday_set(&self) -> std::collections::HashSet<chrono::NaiveDate> {
        self.holidays
            .iter()
            .filter_map(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .collect()
    }

    pub(crate) fn is_workday_with_holidays(
        &self,
        date: chrono::NaiveDate,
        holiday_set: &std::collections::HashSet<chrono::NaiveDate>,
    ) -> bool {
        if holiday_set.contains(&date) {
            return false;
        }
        let weekday = date.weekday().to_string().to_lowercase();
        let weekday_short = match weekday.get(..3) {
            Some(s) => s,
            None => return false, // weekday name unexpectedly short
        };
        self.days.iter().any(|d| {
            let d_lower = d.to_lowercase();
            d_lower == weekday || d_lower == weekday_short
        })
    }
}

/// Get the data directory path, creating it if necessary.
pub fn get_data_dir() -> Result<PathBuf, Error> {
    let dirs = directories::ProjectDirs::from("", "", "niri-activity-rs")
        .ok_or_else(|| Error::NiriError("Could not determine data directory".into()))?;
    let data_dir = dirs.data_dir().to_path_buf();
    fs::create_dir_all(&data_dir)?;
    Ok(data_dir)
}

/// Get the configuration file path, creating the config directory if necessary.
pub fn get_config_path() -> Result<PathBuf, Error> {
    let dirs = directories::ProjectDirs::from("", "", "niri-activity-rs")
        .ok_or_else(|| Error::NiriError("Could not determine config directory".into()))?;
    let config_dir = dirs.config_dir();
    fs::create_dir_all(config_dir)?;
    Ok(config_dir.join("config.toml"))
}

/// Load env file (KEY=VALUE lines) from config dir into process environment.
/// Must be called before any threads are spawned (set_var is unsafe in
/// multi-threaded contexts). Call this once from main() at startup.
pub(crate) fn load_env_file() -> Result<(), Error> {
    let env_path = get_config_path()?.with_file_name("env");
    if env_path.exists()
        && let Ok(contents) = fs::read_to_string(&env_path)
    {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();

                // Validate key before calling set_var
                if key.is_empty() || key.contains('=') || key.contains('\0') {
                    tracing::warn!("Skipping invalid environment variable key: {:?}", key);
                    continue;
                }

                // SAFETY: This is called from main() immediately after Cli::parse(),
                // before watch() spawns any threads (signal_hook, input monitor, logind).
                // std::env::set_var is only unsafe in multi-threaded contexts.
                // Do NOT move this call to after watcher::watch() or any thread spawn.
                // SAFETY: Called before any threads are spawned (single-threaded context).
                #[allow(unsafe_code)]
                unsafe {
                    std::env::set_var(key, value);
                }
            }
        }
    }
    Ok(())
}
/// Load configuration from file, or return defaults if file does not exist.
pub fn load_config() -> Result<Config, Error> {
    let config_path = get_config_path()?;
    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        let raw: RawConfig = toml::from_str(&content)?;
        Ok(Config::from(raw))
    } else {
        Ok(Config::default())
    }
}

/// Create a default configuration file if one does not already exist.
pub fn init_config() -> Result<(), Error> {
    let config_path = get_config_path()?;

    if config_path.exists() {
        println!("Config already exists: {}", config_path.display());
        return Ok(());
    }

    let example = r#"# Seconds without input before state transitions to Passive (default: 60)
idle_threshold_secs = 120

# Seconds without input before Passive transitions to Idle (default: 300)
deep_idle_secs = 300

# Seconds of continuous idle before entering Away state and pausing tracking (default: 1800 = 30 min)
# When Away, the daemon stops recording events until user input resumes.
# This prevents overnight/sleep idle from inflating your tracked time.
away_threshold_secs = 1800

# Seconds of zero meaningful input (keystrokes + clicks) before active time
# is reclassified as passive. Prevents false-active from media playback
# (e.g. Firefox video playing keeps compositor alive but no real input).
input_active_secs = 60

# Jiggler / artificial input detection
[jiggler]
enabled = true
window_secs = 600          # 10-min observation window
min_events = 10            # need this many events to evaluate
variance_threshold_ms = 100 # max-min interval < this = artificial
process_blacklist = [
    "xdotool", "ydotool", "caffeine", "keep-presence",
    "mouse-jiggler", "movemouse", "jiggler", "wiggle"
]

# Map app_id to category: productive, unproductive, neutral
[categories]
# IDEs / Development
jetbrains-rustrover = "productive"
jetbrains-idea = "productive"
jetbrains-pycharm = "productive"
code = "productive"
zed = "productive"
neovim = "productive"
vim = "productive"

# Terminal
Alacritty = "productive"
kitty = "productive"
foot = "productive"
wezterm = "productive"

# Browser (productive by default — override specific sites with [[title_rules]] below)
zen = "productive"
firefox = "productive"
chromium = "productive"

# Notes / Docs
obsidian = "productive"
logseq = "productive"
notion = "productive"

# Email
thunderbird = "productive"

# Communication
slack = "neutral"
discord = "unproductive"
vesktop = "unproductive"
teams = "productive"
zoom = "neutral"

# Entertainment
spotify = "unproductive"
steam = "unproductive"
vlc = "unproductive"
mpv = "unproductive"

# Title rules — override category based on window title (case-insensitive).
# Default: substring match. Add regex = true for regex patterns.
# Optional: app = [\"zen\", \"firefox\"] limits rule to those apps.
# Checked BEFORE app-level categories.

# Browser-only: one regex rule replaces many substring rules
[[title_rules]]
pattern = "YouTube|Instagram|Spotify|Discord|Reddit|TikTok|Twitter|Twitch|Netflix"
category = "unproductive"
app = ["zen", "firefox", "chromium", "chromium-browser", "spotify"]
regex = true

# Global productive patterns
[[title_rules]]
pattern = "GitHub|Stack Overflow|docs\.rs|LinkedIn"
category = "productive"
regex = true


# Work schedule — only affects report breakdown (tracking is always on)
[schedule]
enabled = false
start = "09:00"
end = "17:00"
days = ["Mon", "Tue", "Wed", "Thu", "Fri"]
# NYSE market holidays (used for calculating daily averages in exports)
holidays = [
    "2025-01-01", "2025-01-20", "2025-02-17", "2025-04-18", "2025-05-26",
    "2025-06-19", "2025-07-04", "2025-09-01", "2025-11-27", "2025-12-25",
    "2026-01-01", "2026-01-19", "2026-02-16", "2026-04-03", "2026-05-25",
    "2026-06-19", "2026-07-03", "2026-09-07", "2026-11-26", "2026-12-25",
]

# Goals / targets — show progress towards daily/weekly goals in reports
[goals]
enabled = false
daily = "8h"      # Target productive time per day (e.g., "8h", "6h30m")
weekly = "40h"    # Target productive time per week
"#;

    fs::write(&config_path, example)?;
    println!("Created config: {}", config_path.display());
    println!("\nEdit this file to customize your categories.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("spotify", "spotify"));
        assert!(!glob_match("spotify", "spotify-web"));
    }

    #[test]
    fn glob_prefix() {
        assert!(glob_match("jetbrains-*", "jetbrains-rustrover"));
        assert!(glob_match("jetbrains-*", "jetbrains-clion"));
        assert!(!glob_match("jetbrains-*", "code"));
    }

    #[test]
    fn glob_suffix() {
        assert!(glob_match("*.desktop", "com.ayugram.desktop"));
        assert!(!glob_match("*.desktop", "Alacritty"));
    }

    #[test]
    fn glob_contains() {
        assert!(glob_match("*hex_rays*", "com.hex_rays.IDA.pro._9_3"));
        assert!(!glob_match("*hex_rays*", "jetbrains-idea"));
    }

    #[test]
    fn glob_middle() {
        assert!(glob_match("com.*desktop", "com.ayugram.desktop"));
        assert!(!glob_match("com.*desktop", "org.pulseaudio.pavucontrol"));
        // * matches zero characters (off-by-one regression test)
        assert!(glob_match("com.*desktop", "com.desktop"));
    }

    #[test]
    fn classify_exact_before_glob() {
        let mut categories = HashMap::new();
        categories.insert("jetbrains-*".to_string(), Category::Productive);
        categories.insert("jetbrains-idea".to_string(), Category::Neutral);
        let config = Config {
            categories,
            ..Default::default()
        };
        assert_eq!(config.classify("jetbrains-idea", ""), Category::Neutral);
        assert_eq!(
            config.classify("jetbrains-rustrover", ""),
            Category::Productive
        );
    }

    #[test]
    fn classify_glob_fallback() {
        let mut categories = HashMap::new();
        categories.insert("jetbrains-*".to_string(), Category::Productive);
        let config = Config {
            categories,
            ..Default::default()
        };
        assert_eq!(config.classify("jetbrains-clion", ""), Category::Productive);
        assert_eq!(config.classify("code", ""), Category::Neutral);
    }

    #[test]
    fn title_rule_substring_default() {
        let config = Config {
            title_rules: vec![TitleRule {
                pattern: "YouTube".to_string(),
                category: Category::Unproductive,
                app: vec![],
                compiled: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            config.classify("zen", "Watching YouTube - Firefox"),
            Category::Unproductive
        );
        assert_eq!(config.classify("zen", "GitHub PR #42"), Category::Neutral);
    }

    #[test]
    fn title_rule_regex() {
        let re = regex::RegexBuilder::new("YouTube|Reddit|TikTok")
            .case_insensitive(true)
            .build()
            .expect("hardcoded regex pattern is valid");
        let config = Config {
            title_rules: vec![TitleRule {
                pattern: "YouTube|Reddit|TikTok".to_string(),
                category: Category::Unproductive,
                app: vec![],
                compiled: Some(re),
            }],
            ..Default::default()
        };
        assert_eq!(
            config.classify("zen", "Funny reddit thread"),
            Category::Unproductive
        );
        assert_eq!(
            config.classify("zen", "TikTok - For You"),
            Category::Unproductive
        );
        assert_eq!(
            config.classify("zen", "Rust documentation"),
            Category::Neutral
        );
    }

    #[test]
    fn title_rule_regex_case_insensitive() {
        #[allow(clippy::trivial_regex)]
        let re = regex::RegexBuilder::new("GitHub")
            .case_insensitive(true)
            .build()
            .expect("hardcoded regex pattern is valid");
        let config = Config {
            title_rules: vec![TitleRule {
                pattern: "GitHub".to_string(),
                category: Category::Productive,
                app: vec![],
                compiled: Some(re),
            }],
            ..Default::default()
        };
        assert_eq!(
            config.classify("zen", "github.com/coleleavitt"),
            Category::Productive
        );
    }

    #[test]
    fn title_rule_app_scoped_regex() {
        let re = regex::RegexBuilder::new("YouTube|Spotify")
            .case_insensitive(true)
            .build()
            .expect("hardcoded regex pattern is valid");
        let config = Config {
            title_rules: vec![TitleRule {
                pattern: "YouTube|Spotify".to_string(),
                category: Category::Unproductive,
                app: vec!["zen".to_string(), "firefox".to_string()],
                compiled: Some(re),
            }],
            ..Default::default()
        };
        assert_eq!(
            config.classify("zen", "YouTube - Music"),
            Category::Unproductive
        );
        assert_eq!(
            config.classify("Alacritty", "vim spotify-rs/src/main.rs"),
            Category::Neutral
        );
    }
}
