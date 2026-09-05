use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::{fmt, fs};

use chrono::{DateTime, Datelike, Local, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Categorise a browser page by the site it came from rather than its title.
///
/// A title is whatever the page calls itself, so title rules misfire both
/// ways: a GitHub repo about Discord reads as chat, a film from `cineby.at`
/// reads as nothing. The domain is unambiguous.
#[derive(Debug, Clone)]
pub struct DomainRule {
    /// Host to match. Also matches subdomains, so `youtube.com` covers
    /// `www.youtube.com` and `m.youtube.com`.
    pub domain: String,
    pub category: Category,
}

impl DomainRule {
    fn matches(&self, host: &str) -> bool {
        // A rule naming no port matches any port, so `localhost` covers the
        // dev servers on :3000 and :5173. A rule naming one is exact.
        let host = if self.domain.contains(':') {
            host
        } else {
            host.split(':').next().unwrap_or(host)
        };

        host == self.domain
            || host
                .strip_suffix(&self.domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    }
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

    /// Credit time to `productive` when an agent was working, whatever the
    /// focused window was.
    ///
    /// Waiting on an agent produces work even when the screen shows something
    /// unproductive. The underlying `agent_ms` measurement is unaffected, so
    /// this only changes how reports attribute it and can be turned off again
    /// without losing data.
    #[serde(default = "default_agent_counts_productive")]
    pub counts_as_productive: bool,

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
    vec!["~/.local/share/opencode/opencode*.db".to_string()]
}

fn default_process_whitelist() -> Vec<String> {
    Vec::new()
}

fn default_process_recency_secs() -> u64 {
    5 * 60
}

fn default_agent_counts_productive() -> bool {
    true
}

impl Default for AgentActivityConfig {
    fn default() -> Self {
        Self {
            enabled: default_agent_enabled(),
            poll_interval_secs: default_agent_poll_interval(),
            activity_window_secs: default_agent_activity_window(),
            databases: default_agent_databases(),
            process_whitelist: default_process_whitelist(),
            counts_as_productive: default_agent_counts_productive(),
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
        let masked_local = if local.is_empty() {
            "*".to_string()
        } else {
            format!("{}***", local.chars().next().unwrap_or('*'))
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

    // If there are trailing digits without a unit suffix, the input is
    // malformed
    if !current_num.is_empty() {
        return None;
    }

    if total_ms > 0 { Some(total_ms) } else { None }
}

/// Raw projects configuration (deserialized from TOML).
#[derive(Debug, Deserialize, Default)]
struct RawProjectsConfig {
    #[serde(default)]
    search_dirs: Vec<String>,
    #[serde(default)]
    aliases: HashMap<String, String>,
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
    projects: RawProjectsConfig,
    #[serde(default)]
    categories: HashMap<String, Category>,
    #[serde(default)]
    title_rules: Vec<RawTitleRule>,
    #[serde(default)]
    domain_rules: Vec<RawDomainRule>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawDomainRule {
    domain: String,
    category: Category,
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
    pub project_search_dirs: Vec<PathBuf>,
    pub project_aliases: HashMap<String, String>,
    pub categories: HashMap<String, Category>,
    pub title_rules: Vec<TitleRule>,
    pub domain_rules: Vec<DomainRule>,
    /// Page title to domain, built from browser history. Empty until
    /// [`Config::load_browser_history`] runs.
    pub title_domains: HashMap<String, String>,
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

/// Expand a leading `~` in a path string to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(base_dirs) = directories::BaseDirs::new() {
            return base_dirs.home_dir().join(rest);
        }
    } else if path == "~" {
        if let Some(base_dirs) = directories::BaseDirs::new() {
            return base_dirs.home_dir().to_path_buf();
        }
    }
    PathBuf::from(path)
}

impl TryFrom<RawConfig> for Config {
    type Error = Error;

    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        let title_rules = raw
            .title_rules
            .into_iter()
            .enumerate()
            .map(|(index, r)| {
                let compiled = if r.regex {
                    Some(
                        regex::RegexBuilder::new(&r.pattern)
                            .case_insensitive(true)
                            .build()
                            .map_err(|error| {
                                Error::InvalidArgument(format!(
                                    "invalid title_rules[{index}] regex {:?}: {error}",
                                    r.pattern
                                ))
                            })?,
                    )
                } else {
                    None
                };
                Ok(TitleRule {
                    pattern: r.pattern,
                    category: r.category,
                    app: r.app,
                    compiled,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let timezone = raw
            .timezone
            .map(|timezone| {
                timezone.parse::<Tz>().map_err(|error| {
                    Error::InvalidArgument(format!(
                        "invalid configured timezone {timezone:?}: {error}"
                    ))
                })
            })
            .transpose()?;

        let project_search_dirs = raw
            .projects
            .search_dirs
            .iter()
            .map(|s| expand_tilde(s))
            .collect();
        let project_aliases = raw.projects.aliases;

        Ok(Self {
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
            project_search_dirs,
            project_aliases,
            categories: raw.categories,
            title_rules,
            domain_rules: raw
                .domain_rules
                .into_iter()
                .map(|r| DomainRule {
                    domain: r.domain.to_lowercase(),
                    category: r.category,
                })
                .collect(),
            title_domains: HashMap::new(),
        })
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
            project_search_dirs: Vec::new(),
            project_aliases: HashMap::new(),
            categories: HashMap::new(),
            title_rules: Vec::new(),
            domain_rules: Vec::new(),
            title_domains: HashMap::new(),
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
    if pattern.is_empty() {
        return value.is_empty();
    }

    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let segments = pattern.split('*').filter(|segment| !segment.is_empty());
    let mut cursor = 0usize;
    let mut first = true;
    let mut last_end = 0usize;

    for segment in segments {
        let remainder = &value[cursor..];
        let relative = if first && anchored_start {
            remainder.starts_with(segment).then_some(0)
        } else {
            remainder.find(segment)
        };
        let Some(relative) = relative else {
            return false;
        };
        cursor = cursor
            .saturating_add(relative)
            .saturating_add(segment.len());
        last_end = cursor;
        first = false;
    }

    if first {
        // One or more `*` characters and no literal segments.
        return true;
    }
    !anchored_end || last_end == value.len()
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
        // Exact match first (case-insensitive) so specific keys beat globs.
        for (pattern, &cat) in &self.categories {
            if !pattern.contains('*') && pattern.eq_ignore_ascii_case(app_id) {
                return Some(cat);
            }
        }
        let app_id_lower = app_id.to_ascii_lowercase();
        for (pattern, &cat) in &self.categories {
            if pattern.contains('*') && glob_match(&pattern.to_ascii_lowercase(), &app_id_lower) {
                return Some(cat);
            }
        }
        None
    }

    /// Populate [`Config::title_domains`] from local browser history.
    ///
    /// Returns the number of titles resolved. A missing or unreadable history
    /// database is not an error — domain rules simply stay inert and
    /// classification falls back to title matching alone.
    pub fn load_browser_history(&mut self) -> usize {
        match browser_profiles::title_to_domain() {
            Ok(map) => {
                self.title_domains = map;
                self.title_domains.len()
            }
            Err(e) => {
                self.title_domains.clear();
                tracing::debug!("browser history unavailable, domain rules inert: {e}");
                0
            }
        }
    }

    pub fn can_reclassify_all(&self) -> bool {
        self.domain_rules.is_empty() || !self.title_domains.is_empty()
    }

    fn domain_category(&self, title: &str) -> Option<Category> {
        // A window title carries the browser's branding and a history title
        // does not, so skipping this drops match rates from ~91% to ~4%.
        let key = browser_profiles::strip_window_suffix(title);
        let host = self
            .title_domains
            .get(key)
            .or_else(|| self.title_domains.get(title))?;
        self.domain_rules
            .iter()
            .find(|rule| rule.matches(host))
            .map(|rule| rule.category)
    }

    pub fn classify(&self, app_id: &str, title: &str) -> Category {
        let title_lower = title.to_lowercase();
        let explicit_cat = self.app_category(app_id);

        // Domain beats title: the host a page was served from is a fact, while
        // its title is a claim. Only trust the domain map for actual browser
        // windows — otherwise a non-browser window sharing a page title would
        // be silently reclassified.
        if browser_profiles::is_browser_app_id(app_id)
            && let Some(cat) = self.domain_category(title)
        {
            return cat;
        }

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

                // Validate key and value before calling set_var
                if key.is_empty() || key.contains('=') || key.contains('\0') {
                    tracing::warn!("Skipping invalid environment variable key: {:?}", key);
                    continue;
                }
                if value.contains('\0') {
                    tracing::warn!(
                        "Skipping environment variable with NUL byte in value: {:?}",
                        key
                    );
                    continue;
                }

                // SAFETY: This is called from main() immediately after
                // Cli::parse(), before watch() spawns any
                // threads (signal_hook, input monitor, logind).
                // std::env::set_var is only unsafe in multi-threaded contexts.
                // Do NOT move this call to after watcher::watch() or any thread
                // spawn. SAFETY: Called before any threads are
                // spawned (single-threaded context).
                #[allow(unsafe_code)]
                unsafe {
                    std::env::set_var(key, value);
                }
            }
        }
    }
    Ok(())
}
fn validate_agent_activity_config(config: &AgentActivityConfig) -> Result<(), Error> {
    if config.enabled && config.poll_interval_secs == 0 {
        return Err(Error::InvalidArgument(
            "agent_activity.poll_interval_secs must be at least 1 when agent activity is enabled"
                .into(),
        ));
    }
    if config.enabled && config.activity_window_secs == 0 {
        return Err(Error::InvalidArgument(
            "agent_activity.activity_window_secs must be at least 1 when agent activity is enabled"
                .into(),
        ));
    }
    Ok(())
}

fn validate_jiggler_config(config: &JigglerConfig) -> Result<(), Error> {
    if !config.enabled {
        return Ok(());
    }
    if config.window_secs < 60 {
        return Err(Error::InvalidArgument(
            "jiggler.window_secs must be at least 60 when jiggler detection is enabled".into(),
        ));
    }
    if config.min_events < 2 {
        return Err(Error::InvalidArgument(
            "jiggler.min_events must be at least 2 when jiggler detection is enabled".into(),
        ));
    }
    Ok(())
}

/// Load configuration from file, or return defaults if file does not exist.
pub fn load_config() -> Result<Config, Error> {
    let config_path = get_config_path()?;
    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        let raw: RawConfig = toml::from_str(&content)?;
        validate_jiggler_config(&raw.jiggler)?;
        validate_agent_activity_config(&raw.agent_activity)?;
        Config::try_from(raw)
    } else {
        Ok(Config::default())
    }
}

const DEFAULT_CONFIG: &str = r#"# Seconds without input before state transitions to Passive (default: 60)
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
pattern = "GitHub|Stack Overflow|docs\\.rs|LinkedIn"
category = "productive"
regex = true

[agent_activity]
# Count time as productive whenever a coding agent was working, whatever was
# on screen. Waiting on an agent produces work even when the focused window is
# a video. Turning this off restores plain focus-based categories; the raw
# measurement is stored either way, so nothing is lost by changing it.
counts_as_productive = true

# Domain rules — categorise browser pages by the site they came from, resolved
# through local browser history. Checked BEFORE title rules, since a domain is
# a fact and a title is a claim. Subdomains match automatically.
[[domain_rules]]
domain = "youtube.com"
category = "unproductive"

[[domain_rules]]
domain = "github.com"
category = "productive"


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

# Project detection — map working directories to project names
[projects]
# Directories to search for git projects (~ is expanded to home dir)
search_dirs = ["~/RustProjects/active", "~/projects", "~/work"]

# Manual project name overrides (directory basename → display name)
[projects.aliases]
"niri-activity-rs" = "Activity Tracker"
"#;

/// Create a default configuration file if one does not already exist.
pub fn init_config() -> Result<(), Error> {
    let config_path = get_config_path()?;

    if config_path.exists() {
        println!("Config already exists: {}", config_path.display());
        return Ok(());
    }

    fs::write(&config_path, DEFAULT_CONFIG)?;
    println!("Created config: {}", config_path.display());
    println!("\nEdit this file to customize your categories.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_config_is_valid_toml() {
        let raw = toml::from_str::<RawConfig>(DEFAULT_CONFIG).expect("generated config must parse");
        Config::try_from(raw).expect("generated config must compile");
    }

    fn parse_config(toml: &str) -> Result<Config, Error> {
        let raw: RawConfig = toml::from_str(toml).expect("test config must parse");
        Config::try_from(raw)
    }

    #[test]
    fn invalid_title_rule_regex_rejects_entire_config() {
        let error = parse_config(
            r#"
[[title_rules]]
pattern = "valid"
category = "productive"

[[title_rules]]
pattern = "[invalid"
category = "unproductive"
regex = true
"#,
        )
        .expect_err("an invalid later rule must not leave a partial rule set");

        let message = error.to_string();
        assert!(
            message.contains("invalid title_rules[1] regex"),
            "{message}"
        );
        assert!(message.contains("[invalid"), "{message}");
    }

    #[test]
    fn invalid_configured_timezone_is_rejected() {
        let error = parse_config(r#"timezone = "Mars/Olympus_Mons""#)
            .expect_err("an explicitly configured invalid timezone must fail closed");

        let message = error.to_string();
        assert!(message.contains("invalid configured timezone"), "{message}");
        assert!(message.contains("Mars/Olympus_Mons"), "{message}");
    }

    #[test]
    fn omitted_and_valid_timezones_are_accepted() {
        assert_eq!(
            parse_config("").expect("timezone is optional").timezone,
            None
        );
        assert_eq!(
            parse_config(r#"timezone = "America/New_York""#)
                .expect("a valid timezone must compile")
                .timezone,
            Some(chrono_tz::America::New_York)
        );
    }

    fn parse_and_validate_agent_activity(toml: &str) -> Result<(), Error> {
        let raw: RawConfig = toml::from_str(toml).expect("test config must parse");
        validate_agent_activity_config(&raw.agent_activity)
    }

    #[test]
    fn enabled_agent_activity_rejects_zero_activity_window() {
        let error = parse_and_validate_agent_activity(
            "[agent_activity]\nenabled = true\nactivity_window_secs = 0\n",
        )
        .expect_err("a zero window makes every timestamp stale");

        assert!(
            error
                .to_string()
                .contains("agent_activity.activity_window_secs must be at least 1")
        );
    }

    #[test]
    fn disabled_agent_activity_allows_zero_activity_window() {
        parse_and_validate_agent_activity(
            "[agent_activity]\nenabled = false\nactivity_window_secs = 0\n",
        )
        .expect("disabled agent settings are inert");
    }

    fn parse_and_validate_jiggler(toml: &str) -> Result<(), Error> {
        let raw: RawConfig = toml::from_str(toml).expect("test config must parse");
        validate_jiggler_config(&raw.jiggler)
    }

    #[test]
    fn enabled_jiggler_rejects_window_shorter_than_minimum_span() {
        let error = parse_and_validate_jiggler(
            "[jiggler]\nenabled = true\nwindow_secs = 59\nmin_events = 2\n",
        )
        .expect_err("a short window makes the 60-second span invariant impossible");

        assert!(
            error
                .to_string()
                .contains("jiggler.window_secs must be at least 60")
        );
    }

    #[test]
    fn enabled_jiggler_accepts_minimum_window() {
        parse_and_validate_jiggler("[jiggler]\nenabled = true\nwindow_secs = 60\nmin_events = 2\n")
            .expect("the minimum viable window must be accepted");
    }

    #[test]
    fn enabled_jiggler_rejects_fewer_than_two_events() {
        let error = parse_and_validate_jiggler(
            "[jiggler]\nenabled = true\nwindow_secs = 60\nmin_events = 1\n",
        )
        .expect_err("interval detection requires at least two events");

        assert!(
            error
                .to_string()
                .contains("jiggler.min_events must be at least 2")
        );
    }

    #[test]
    fn disabled_jiggler_allows_inert_detection_values() {
        parse_and_validate_jiggler("[jiggler]\nenabled = false\nwindow_secs = 0\nmin_events = 0\n")
            .expect("disabled jiggler settings are inert");
    }

    #[test]
    fn domain_reclassification_requires_loaded_history() {
        let config = domain_config(vec![("youtube.com", Category::Unproductive)], vec![]);

        assert!(!config.can_reclassify_all());
    }

    #[test]
    fn default_process_whitelist_excludes_persistent_runtimes() {
        let defaults = default_process_whitelist();

        for process in ["node", "java", "php", "sccache", "rust-analyzer"] {
            assert!(
                !defaults.iter().any(|candidate| candidate == process),
                "{process} is commonly persistent and must not imply agent activity"
            );
        }
    }

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
    fn glob_supports_multiple_and_consecutive_wildcards() {
        assert!(glob_match("*foo*bar*", "prefix-foo-middle-bar-suffix"));
        assert!(glob_match("foo*bar*baz", "foo-1-bar-2-baz"));
        assert!(glob_match("foo**bar", "foobar"));
        assert!(!glob_match("foo*bar*baz", "foo-baz-bar"));
        assert!(!glob_match("foo*bar", "xfoo-bar"));
        assert!(!glob_match("foo*bar", "foo-bar-x"));
    }

    #[test]
    fn wildcard_category_matching_is_ascii_case_insensitive() {
        let config = Config {
            categories: HashMap::from([("JetBrains-*".to_string(), Category::Productive)]),
            ..Config::default()
        };
        assert_eq!(
            config.app_category("jetbrains-RustRover"),
            Some(Category::Productive)
        );
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

    fn domain_config(rules: Vec<(&str, Category)>, titles: Vec<(&str, &str)>) -> Config {
        let title_domains = titles
            .into_iter()
            .map(|(t, d)| (t.to_string(), d.to_string()))
            .collect::<HashMap<_, _>>();
        Config {
            domain_rules: rules
                .into_iter()
                .map(|(domain, category)| DomainRule {
                    domain: domain.to_string(),
                    category,
                })
                .collect(),
            title_domains,
            ..Default::default()
        }
    }

    #[test]
    fn domain_rule_matches_subdomains() {
        let config = domain_config(
            vec![("youtube.com", Category::Unproductive)],
            vec![
                ("Some Video", "www.youtube.com"),
                ("Mobile Video", "m.youtube.com"),
                ("Bare", "youtube.com"),
            ],
        );
        for title in ["Some Video", "Mobile Video", "Bare"] {
            assert_eq!(config.classify("zen", title), Category::Unproductive);
        }
    }

    #[test]
    fn domain_rule_matches_through_browser_branding() {
        let config = domain_config(
            vec![("youtube.com", Category::Unproductive)],
            vec![("Some Video", "www.youtube.com")],
        );
        assert_eq!(
            config.classify("zen", "Some Video — Zen Browser"),
            Category::Unproductive,
            "window titles carry branding that history titles lack"
        );
    }

    #[test]
    fn portless_domain_rule_matches_any_port() {
        let config = domain_config(
            vec![("localhost", Category::Productive)],
            vec![
                ("Vite App", "localhost:5173"),
                ("Next App", "localhost:3000"),
                ("Bare", "localhost"),
            ],
        );
        for title in ["Vite App", "Next App", "Bare"] {
            assert_eq!(config.classify("zen", title), Category::Productive);
        }
    }

    #[test]
    fn domain_rule_with_a_port_is_exact() {
        let config = domain_config(
            vec![("localhost:3000", Category::Productive)],
            vec![("App", "localhost:3000"), ("Other", "localhost:9999")],
        );
        assert_eq!(config.classify("zen", "App"), Category::Productive);
        assert_eq!(config.classify("zen", "Other"), Category::Neutral);
    }

    #[test]
    fn domain_rule_does_not_match_lookalike_suffix() {
        let config = domain_config(
            vec![("youtube.com", Category::Unproductive)],
            vec![("Phish", "notyoutube.com")],
        );
        assert_eq!(config.classify("zen", "Phish"), Category::Neutral);
    }

    #[test]
    fn supported_browser_ids_receive_domain_classification() {
        let config = domain_config(
            vec![("youtube.com", Category::Unproductive)],
            vec![("Some Video", "www.youtube.com")],
        );
        let supported_ids = [
            // Firefox family.
            "firefox",
            "firefox-bin",
            "firefox-esr",
            "firefox-nightly",
            "FirefoxDeveloperEdition",
            "org.mozilla.firefox",
            "org.mozilla.FirefoxDeveloperEdition",
            "org.mozilla.FirefoxNightly",
            "zen",
            "zen-browser",
            "zen-alpha",
            "zen-twilight",
            "zen-unofficial",
            "app.zen_browser.zen",
            "app.zen_browser.zen-twilight",
            "LibreWolf",
            "io.gitlab.librewolf-community",
            "waterfox",
            "net.waterfox.waterfox",
            "Tor Browser",
            "org.torproject.torbrowser-launcher",
            "camoufox",
            // Chromium family.
            "google-chrome",
            "google-chrome-stable",
            "google-chrome-beta",
            "google-chrome-unstable",
            "com.google.Chrome",
            "com.google.ChromeBeta",
            "com.google.ChromeDev",
            "chromium",
            "chromium-browser",
            "org.chromium.Chromium",
            "brave-browser",
            "brave-browser-beta",
            "brave-browser-dev",
            "brave-browser-nightly",
            "com.brave.Browser",
            "com.brave.Browser.Beta",
            "com.brave.Browser.Dev",
            "com.brave.Browser.Nightly",
            "microsoft-edge",
            "microsoft-edge-stable",
            "microsoft-edge-beta",
            "microsoft-edge-dev",
            "com.microsoft.Edge",
            "com.microsoft.Edge.Beta",
            "com.microsoft.Edge.Dev",
            "vivaldi-stable",
            "vivaldi-snapshot",
            "com.vivaldi.Vivaldi",
            "opera",
            "opera-beta",
            "opera-developer",
            "com.opera.Opera",
        ];

        for app_id in supported_ids {
            assert_eq!(
                config.classify(app_id, "Some Video"),
                Category::Unproductive,
                "supported browser ID {app_id:?} must receive domain classification"
            );
        }
    }

    #[test]
    fn non_browser_ids_do_not_receive_domain_classification() {
        let config = domain_config(
            vec![("youtube.com", Category::Unproductive)],
            vec![("Some Video", "www.youtube.com")],
        );

        for app_id in [
            "foot",
            "code",
            "spotify",
            "firefox-helper",
            "my-google-chrome-wrapper",
            "org.mozilla.not-firefox",
            "",
        ] {
            assert_eq!(
                config.classify(app_id, "Some Video"),
                Category::Neutral,
                "non-browser ID {app_id:?} must fail closed"
            );
        }
    }

    #[test]
    fn domain_rule_beats_conflicting_title_rule() {
        let mut config = domain_config(
            vec![("github.com", Category::Productive)],
            vec![(
                "serenity-rs/serenity: A Rust library for the Discord API",
                "github.com",
            )],
        );
        config.title_rules = vec![TitleRule {
            pattern: "Discord".to_string(),
            category: Category::Unproductive,
            app: vec!["zen".to_string()],
            compiled: None,
        }];

        assert_eq!(
            config.classify(
                "zen",
                "serenity-rs/serenity: A Rust library for the Discord API"
            ),
            Category::Productive,
            "the page is served from github.com regardless of what it discusses"
        );
    }

    #[test]
    fn title_rules_still_apply_when_domain_is_unknown() {
        let mut config = domain_config(vec![("github.com", Category::Productive)], vec![]);
        config.title_rules = vec![TitleRule {
            pattern: "YouTube".to_string(),
            category: Category::Unproductive,
            app: vec![],
            compiled: None,
        }];

        assert_eq!(
            config.classify("zen", "Some Video - YouTube"),
            Category::Unproductive
        );
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

    #[test]
    fn projects_config_parses_correctly() {
        let toml_str = r#"
[projects]
search_dirs = ["~/RustProjects/active", "~/projects", "/absolute/path"]

[projects.aliases]
"niri-activity-rs" = "Activity Tracker"
"ers-rs" = "Entity Resolution"
"#;
        let raw: RawConfig = toml::from_str(toml_str).expect("should parse projects config");
        let config = Config::try_from(raw).expect("config must compile");

        // Should have 3 search dirs
        assert_eq!(config.project_search_dirs.len(), 3);

        // Tilde should be expanded (not start with ~)
        assert!(
            !config.project_search_dirs[0]
                .to_str()
                .unwrap()
                .starts_with('~')
        );
        assert!(
            !config.project_search_dirs[1]
                .to_str()
                .unwrap()
                .starts_with('~')
        );
        // Absolute path stays as-is
        assert_eq!(
            config.project_search_dirs[2],
            PathBuf::from("/absolute/path")
        );

        // Tilde-expanded paths should end with the correct suffix
        assert!(
            config.project_search_dirs[0]
                .to_str()
                .unwrap()
                .ends_with("RustProjects/active")
        );
        assert!(
            config.project_search_dirs[1]
                .to_str()
                .unwrap()
                .ends_with("projects")
        );

        // Aliases should be populated
        assert_eq!(config.project_aliases.len(), 2);
        assert_eq!(
            config.project_aliases.get("niri-activity-rs").unwrap(),
            "Activity Tracker"
        );
        assert_eq!(
            config.project_aliases.get("ers-rs").unwrap(),
            "Entity Resolution"
        );
    }

    #[test]
    fn projects_config_defaults_to_empty() {
        let toml_str = r"
idle_threshold_secs = 120
";
        let raw: RawConfig = toml::from_str(toml_str).expect("should parse without projects");
        let config = Config::try_from(raw).expect("config must compile");

        assert_eq!(config.project_search_dirs, [] as [std::path::PathBuf; 0]);
        assert!(config.project_aliases.is_empty());
    }

    #[test]
    fn expand_tilde_absolute_path_unchanged() {
        let path = expand_tilde("/home/user/projects");
        assert_eq!(path, PathBuf::from("/home/user/projects"));
    }

    #[test]
    fn expand_tilde_expands_home() {
        let path = expand_tilde("~/some/path");
        // Should not start with ~ after expansion
        assert!(!path.to_str().unwrap().starts_with('~'));
        assert!(path.to_str().unwrap().ends_with("some/path"));
    }
    #[test]
    fn enabled_agent_activity_rejects_zero_poll_interval() {
        let config = AgentActivityConfig {
            poll_interval_secs: 0,
            ..AgentActivityConfig::default()
        };
        let error = validate_agent_activity_config(&config).expect_err("zero poll interval");
        assert!(error.to_string().contains("poll_interval_secs"));
    }

    #[test]
    fn disabled_agent_activity_allows_inert_zero_poll_interval() {
        let config = AgentActivityConfig {
            enabled: false,
            poll_interval_secs: 0,
            ..AgentActivityConfig::default()
        };
        validate_agent_activity_config(&config).expect("disabled config is inert");
    }
}
