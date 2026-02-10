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
pub struct TitleRule {
    pub pattern: String,
    pub category: Category,
    #[serde(default)]
    pub app: Vec<String>,
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
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_idle_threshold")]
    pub idle_threshold_secs: u64,
    #[serde(default = "default_deep_idle")]
    pub deep_idle_secs: u64,
    #[serde(default = "default_mouse_dpi")]
    pub mouse_dpi: f64,
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default)]
    pub jiggler: JigglerConfig,
    #[serde(default)]
    pub categories: HashMap<String, Category>,
    #[serde(default)]
    pub title_rules: Vec<TitleRule>,
}

fn default_idle_threshold() -> u64 {
    60
}

fn default_deep_idle() -> u64 {
    300
}

fn default_mouse_dpi() -> f64 {
    800.0
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

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_threshold_secs: default_idle_threshold(),
            deep_idle_secs: default_deep_idle(),
            mouse_dpi: default_mouse_dpi(),
            schedule: Schedule::default(),
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
                value.starts_with(prefix) && value.ends_with(suffix)
            } else {
                pattern == value
            }
        }
    }
}

impl Config {
    pub fn classify(&self, app_id: &str, title: &str) -> Category {
        let title_lower = title.to_lowercase();
        for rule in &self.title_rules {
            if !rule.app.is_empty() && !rule.app.iter().any(|a| a.eq_ignore_ascii_case(app_id)) {
                continue;
            }
            if title_lower.contains(&rule.pattern.to_lowercase()) {
                return rule.category;
            }
        }

        if let Some(&cat) = self.categories.get(app_id) {
            return cat;
        }

        for (pattern, &cat) in &self.categories {
            if pattern.contains('*') && glob_match(pattern, app_id) {
                return cat;
            }
        }

        Category::Neutral
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
        Ok(toml::from_str(&content)?)
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

# Title rules — override category based on window title (case-insensitive substring match)
# Checked BEFORE app-level categories, so these take priority.
#
# Optional: add app = [\"zen\", \"firefox\"] to limit a rule to specific apps.
# Without app, the rule applies to ALL windows (including terminals, IDEs, etc).

# Browser-only rules — won't misclassify terminal windows working on \"spotify-rs\"
[[title_rules]]
pattern = \"YouTube\"
category = \"unproductive\"
app = [\"zen\", \"firefox\", \"chromium\"]

[[title_rules]]
pattern = \"Instagram\"
category = \"unproductive\"
app = [\"zen\", \"firefox\", \"chromium\"]

[[title_rules]]
pattern = \"Spotify\"
category = \"unproductive\"
app = [\"zen\", \"firefox\", \"chromium\", \"spotify\"]

[[title_rules]]
pattern = \"Discord\"
category = \"unproductive\"
app = [\"zen\", \"firefox\", \"chromium\"]

[[title_rules]]
pattern = \"Reddit\"
category = \"unproductive\"
app = [\"zen\", \"firefox\", \"chromium\"]

# These are safe as global rules — unlikely false positives
[[title_rules]]
pattern = \"GitHub\"
category = \"productive\"

[[title_rules]]
pattern = \"LinkedIn\"
category = \"productive\"

[[title_rules]]
pattern = \"Stack Overflow\"
category = \"productive\"

[[title_rules]]
pattern = \"docs.rs\"
category = \"productive\"

# Work schedule — only affects report breakdown (tracking is always on)
[schedule]
enabled = false
start = \"09:00\"
end = \"17:00\"
days = [\"Mon\", \"Tue\", \"Wed\", \"Thu\", \"Fri\"]
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
}
