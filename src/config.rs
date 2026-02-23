use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Datelike, Local, NaiveTime};

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
    /// If max_interval - min_interval < this (ms), flag as artificial (default: 100).
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

#[derive(Debug, Deserialize, Default)]
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
    let mut total_ms: i64 = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            current_num.push(c);
        } else if !current_num.is_empty() {
            let num: f64 = current_num.parse().ok()?;
            current_num.clear();
            match c {
                'h' => total_ms += (num * 3600.0 * 1000.0) as i64,
                'm' => total_ms += (num * 60.0 * 1000.0) as i64,
                's' => total_ms += (num * 1000.0) as i64,
                _ => {}
            }
        }
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
    /// Seconds of zero meaningful input (keystrokes + clicks) before active time
    /// is reclassified as passive at flush boundaries. Prevents false-active from
    /// compositor events (e.g. Firefox video inhibiting idle). Default: 60.
    #[serde(default = "default_input_active_secs")]
    input_active_secs: u64,
    #[serde(default = "default_streak_break_tolerance")]
    streak_break_tolerance_secs: u64,
    #[serde(default = "default_streak_idle_timeout")]
    streak_idle_timeout_secs: u64,
    #[serde(default)]
    schedule: Schedule,
    #[serde(default)]
    goals: Goals,
    #[serde(default)]
    email: Email,
    #[serde(default)]
    jiggler: JigglerConfig,
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
    pub schedule: Schedule,
    pub goals: Goals,
    pub email: Email,
    pub jiggler: JigglerConfig,
    pub categories: HashMap<String, Category>,
    pub title_rules: Vec<TitleRule>,
}

fn default_idle_threshold() -> u64 {
    60
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
            .map(|r| {
                let compiled = if r.regex {
                    match regex::RegexBuilder::new(&r.pattern)
                        .case_insensitive(true)
                        .build()
                    {
                        Ok(re) => Some(re),
                        Err(e) => {
                            eprintln!("Warning: invalid regex pattern '{}': {}", r.pattern, e);
                            None
                        }
                    }
                } else {
                    None
                };
                TitleRule {
                    pattern: r.pattern,
                    category: r.category,
                    app: r.app,
                    compiled,
                }
            })
            .collect();
        Self {
            idle_threshold_secs: raw.idle_threshold_secs,
            deep_idle_secs: raw.deep_idle_secs,
            away_threshold_secs: raw.away_threshold_secs,
            mouse_dpi: raw.mouse_dpi,
            mouse_idle_threshold: raw.mouse_idle_threshold,
            input_active_secs: raw.input_active_secs,
            streak_break_tolerance_secs: raw.streak_break_tolerance_secs,
            streak_idle_timeout_secs: raw.streak_idle_timeout_secs,
            schedule: raw.schedule,
            goals: raw.goals,
            email: raw.email,
            jiggler: raw.jiggler,
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
            schedule: Schedule::default(),
            goals: Goals::default(),
            email: Email::default(),
            jiggler: JigglerConfig::default(),
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

fn glob_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    match (pattern.starts_with('*'), pattern.ends_with('*')) {
        (true, true) => {
            let inner = &pattern[1..pattern.len() - 1];
            value.contains(inner)
        }
        (true, false) => {
            let suffix = &pattern[1..];
            value.ends_with(suffix)
        }
        (false, true) => {
            let prefix = &pattern[..pattern.len() - 1];
            value.starts_with(prefix)
        }
        (false, false) => {
            if let Some(pos) = pattern.find('*') {
                let prefix = &pattern[..pos];
                let suffix = &pattern[pos + 1..];
                prefix.len() + suffix.len() <= value.len() && value.starts_with(prefix) && value.ends_with(suffix)
            } else {
                pattern == value
            }
        }
    }
}

impl Config {
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
    pub fn is_in_schedule(&self, dt: &DateTime<Local>) -> bool {
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
            Err(_) => return true,
        };
        let end = match NaiveTime::parse_from_str(&self.end, "%H:%M") {
            Ok(time) => time,
            Err(_) => return true,
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
        let holiday_set = self.holiday_set();
        let mut count = 0i64;
        let mut date = start_date;
        while date <= end_date {
            if self.is_workday_with_holidays(date, &holiday_set) {
                count += 1;
            }
            date += chrono::Duration::days(1);
        }
        count
    }

    fn holiday_set(&self) -> std::collections::HashSet<chrono::NaiveDate> {
        self.holidays
            .iter()
            .filter_map(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .collect()
    }

    fn is_workday_with_holidays(&self, date: chrono::NaiveDate, holiday_set: &std::collections::HashSet<chrono::NaiveDate>) -> bool {
        if holiday_set.contains(&date) {
            return false;
        }
        let weekday = date.weekday().to_string().to_lowercase();
        let weekday_short = &weekday[..3];
        self.days.iter().any(|d| {
            let d_lower = d.to_lowercase();
            d_lower == weekday || d_lower == weekday_short
        })
    }
    pub fn is_workday(&self, date: chrono::NaiveDate) -> bool {
        let holiday_set = self.holiday_set();
        self.is_workday_with_holidays(date, &holiday_set)
    }
}

pub fn get_data_dir() -> Result<PathBuf, Error> {
    let dirs = directories::ProjectDirs::from("", "", "niri-activity-rs")
        .ok_or_else(|| Error::NiriError("Could not determine data directory".into()))?;
    let data_dir = dirs.data_dir().to_path_buf();
    fs::create_dir_all(&data_dir)?;
    Ok(data_dir)
}

pub fn get_config_path() -> Result<PathBuf, Error> {
    let dirs = directories::ProjectDirs::from("", "", "niri-activity-rs")
        .ok_or_else(|| Error::NiriError("Could not determine config directory".into()))?;
    let config_dir = dirs.config_dir();
    fs::create_dir_all(config_dir)?;
    Ok(config_dir.join("config.toml"))
}

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

pub fn init_config() -> Result<(), Error> {
    let config_path = get_config_path()?;

    if config_path.exists() {
        println!("Config already exists: {}", config_path.display());
        return Ok(());
    }

    let example = r#"# Seconds without input before state transitions to Passive (default: 60)
idle_threshold_secs = 60

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
min_events = 5             # need this many events to evaluate
variance_threshold_ms = 100 # max-min interval < this = artificial
process_blacklist = [
    "xdotool", "ydotool", "caffeine", "keep-presence",
    "mouse-jiggler", "movemouse", "jiggler", "wiggle"
]

# Map app_id to category: productive, unproductive, neutral
[categories]
# IDEs / Development
\"jetbrains-rustrover\" = \"productive\"
\"jetbrains-idea\" = \"productive\"
\"jetbrains-pycharm\" = \"productive\"
\"code\" = \"productive\"
\"zed\" = \"productive\"
\"neovim\" = \"productive\"
\"vim\" = \"productive\"

# Terminal
\"Alacritty\" = \"productive\"
\"kitty\" = \"productive\"
\"foot\" = \"productive\"
\"wezterm\" = \"productive\"

# Browser (productive by default — override specific sites with [[title_rules]] below)
\"zen\" = \"productive\"
\"firefox\" = \"productive\"
\"chromium\" = \"productive\"

# Notes / Docs
\"obsidian\" = \"productive\"
\"logseq\" = \"productive\"
\"notion\" = \"productive\"

# Email
\"thunderbird\" = \"productive\"

# Communication
\"slack\" = \"neutral\"
\"discord\" = \"unproductive\"
\"vesktop\" = \"unproductive\"
\"teams\" = \"productive\"
\"zoom\" = \"neutral\"

# Entertainment
\"spotify\" = \"unproductive\"
\"steam\" = \"unproductive\"
\"vlc\" = \"unproductive\"
\"mpv\" = \"unproductive\"

# Title rules — override category based on window title (case-insensitive).
# Default: substring match. Add regex = true for regex patterns.
# Optional: app = [\"zen\", \"firefox\"] limits rule to those apps.
# Checked BEFORE app-level categories.

# Browser-only: one regex rule replaces many substring rules
[[title_rules]]
pattern = \"YouTube|Instagram|Spotify|Discord|Reddit|TikTok|Twitter|Twitch|Netflix\"
category = \"unproductive\"
app = [\"zen\", \"firefox\", \"chromium\", \"chromium-browser\", \"spotify\"]
regex = true

# Global productive patterns
[[title_rules]]
pattern = \"GitHub|Stack Overflow|docs\\.rs|LinkedIn\"
category = \"productive\"
regex = true
category = \"productive\"

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
            .unwrap();
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
        let re = regex::RegexBuilder::new("GitHub")
            .case_insensitive(true)
            .build()
            .unwrap();
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
            .unwrap();
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
