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
    #[serde(default = "default_mouse_dpi")]
    pub mouse_dpi: f64,
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default)]
    pub categories: HashMap<String, Category>,
    #[serde(default)]
    pub title_rules: Vec<TitleRule>,
}

fn default_idle_threshold() -> u64 {
    60
}

fn default_mouse_dpi() -> f64 {
    800.0
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
            mouse_dpi: default_mouse_dpi(),
            schedule: Schedule::default(),
            categories: HashMap::new(),
            title_rules: Vec::new(),
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

impl Config {
    pub fn classify(&self, app_id: &str, title: &str) -> Category {
        let title_lower = title.to_lowercase();
        for rule in &self.title_rules {
            if title_lower.contains(&rule.pattern.to_lowercase()) {
                return rule.category;
            }
        }
        self.categories
            .get(app_id)
            .copied()
            .unwrap_or(Category::Neutral)
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

    let example = r#"# Idle threshold in seconds (default: 120)
idle_threshold_secs = 120

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
# Useful for browsers where the same app_id can be productive or not.

[[title_rules]]
pattern = \"YouTube\"
category = \"unproductive\"

[[title_rules]]
pattern = \"Instagram\"
category = \"unproductive\"

[[title_rules]]
pattern = \"Spotify\"
category = \"unproductive\"

[[title_rules]]
pattern = \"Discord\"
category = \"unproductive\"

[[title_rules]]
pattern = \"Reddit\"
category = \"unproductive\"

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
