use owo_colors::OwoColorize;

use crate::config::Category;

pub fn fmt_duration_compact(ms: i64) -> String {
    let total_secs = ms / 1000;
    if total_secs < 60 {
        format!("{}s", total_secs)
    } else if total_secs < 3600 {
        format!("{}m {:02}s", total_secs / 60, total_secs % 60)
    } else {
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        format!("{}h {:02}m", h, m)
    }
}

pub fn fmt_duration(ms: i64) -> String {
    let hours = ms / 3_600_000;
    let mins = (ms % 3_600_000) / 60_000;
    format!("{}h {}m", hours, mins)
}

/// Format milliseconds as `h:mm:ss` for ActivTrak-compatible CSV export.
pub fn fmt_hms(ms: i64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{}:{:02}:{:02}", h, m, s)
}

pub fn pct(part: i64, total: i64) -> String {
    if total == 0 {
        "0%".to_string()
    } else {
        format!("{:.1}%", part as f64 / total as f64 * 100.0)
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{}...", end)
    }
}

/// Convert raw mouse sensor counts to physical distance.
/// `counts / mouse_dpi` = inches (evdev REL_X/REL_Y are mickeys).
pub fn fmt_distance(counts: i64, mouse_dpi: f64) -> String {
    let feet = counts as f64 / mouse_dpi / 12.0;
    if feet >= 5280.0 {
        format!("{:.1}mi", feet / 5280.0)
    } else {
        format!("{:.0}ft", feet)
    }
}

pub fn colored_bar(prod_frac: f64, neutral_frac: f64, unprod_frac: f64, width: usize) -> String {
    let prod_chars = (prod_frac * width as f64).round() as usize;
    let neutral_chars = (neutral_frac * width as f64).round() as usize;
    let unprod_chars = (unprod_frac * width as f64).round() as usize;
    let remaining = width.saturating_sub(prod_chars + neutral_chars + unprod_chars);
    format!(
        "{}{}{}{}",
        "█".repeat(prod_chars).green(),
        "█".repeat(neutral_chars).yellow(),
        "█".repeat(unprod_chars).red(),
        " ".repeat(remaining),
    )
}

pub fn cat_colored(category: Category, text: &str) -> String {
    match category {
        Category::Productive => text.green().to_string(),
        Category::Unproductive => text.red().to_string(),
        Category::Neutral => text.yellow().to_string(),
    }
}

pub fn cat_label(category: Category) -> String {
    match category {
        Category::Productive => "productive".green().bold().to_string(),
        Category::Unproductive => "unproductive".red().bold().to_string(),
        Category::Neutral => "neutral".yellow().bold().to_string(),
    }
}

pub fn cat_bar(category: Category, filled: usize) -> String {
    let segment = "█".repeat(filled);
    match category {
        Category::Productive => segment.green().to_string(),
        Category::Unproductive => segment.red().to_string(),
        Category::Neutral => segment.yellow().to_string(),
    }
}

pub fn section_header(text: &str) -> String {
    text.cyan().bold().to_string()
}
