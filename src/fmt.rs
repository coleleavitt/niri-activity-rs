use owo_colors::OwoColorize;

use crate::config::Category;

pub fn fmt_duration_compact(ms: i64) -> String {
    if ms < 0 {
        return "0s".to_string();
    }
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
    if ms < 0 {
        return "0s".to_string();
    }
    let hours = ms / 3_600_000;
    let mins = (ms % 3_600_000) / 60_000;
    if hours == 0 && mins == 0 {
        let secs = (ms % 60_000) / 1000;
        format!("{}s", secs)
    } else if hours == 0 {
        format!("{}m", mins)
    } else {
        format!("{}h {}m", hours, mins)
    }
}

/// Format milliseconds as `h:mm:ss` for ActivTrak-compatible CSV export.
pub fn fmt_hms(ms: i64) -> String {
    if ms < 0 {
        return "0:00:00".to_string();
    }
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
    } else if max < 3 {
        s.chars().take(max).collect()
    } else {
        let end: String = s.chars().take(max - 3).collect();
        format!("{}...", end)
    }
}

/// Convert raw mouse sensor counts to physical distance.
/// `counts / mouse_dpi` = inches (evdev REL_X/REL_Y are mickeys).
pub fn fmt_distance(counts: i64, mouse_dpi: f64) -> String {
    if !mouse_dpi.is_finite() || mouse_dpi <= 0.0 {
        return "0ft".to_string();
    }
    let feet = counts as f64 / mouse_dpi / 12.0;
    if feet >= 5280.0 {
        format!("{:.1}mi", feet / 5280.0)
    } else {
        format!("{:.0}ft", feet)
    }
}

fn fractional_block(remainder: f64) -> &'static str {
    if !remainder.is_finite() || remainder <= 0.0 {
        return "";
    }
    let clamped = remainder.min(1.0);
    let idx = (clamped * 8.0).round() as usize;
    match idx {
        0 => "",
        1 => "▏",
        2 => "▎",
        3 => "▍",
        4 => "▌",
        5 => "▋",
        6 => "▊",
        7 => "▉",
        _ => "█",
    }
}

pub fn cat_bar_fractional(category: Category, frac_blocks: f64, width: usize) -> String {
    if !frac_blocks.is_finite() || frac_blocks <= 0.0 {
        let pad = " ".repeat(width);
        return match category {
            Category::Productive => pad.green().to_string(),
            Category::Unproductive => pad.red().to_string(),
            Category::Neutral => pad.yellow().to_string(),
        };
    }
    let clamped = frac_blocks.min(width as f64);
    let full = clamped.floor() as usize;
    let remainder = clamped - full as f64;
    let partial = fractional_block(remainder);
    let used = full + usize::from(!partial.is_empty());
    let pad = width.saturating_sub(used);
    let bar = format!("{}{}{}", "█".repeat(full), partial, " ".repeat(pad));
    match category {
        Category::Productive => bar.green().to_string(),
        Category::Unproductive => bar.red().to_string(),
        Category::Neutral => bar.yellow().to_string(),
    }
}

pub fn colored_bar(prod_frac: f64, neutral_frac: f64, _unprod_frac: f64, width: usize) -> String {
    let prod_chars = if prod_frac.is_finite() && prod_frac > 0.0 {
        (prod_frac.min(1.0) * width as f64).round() as usize
    } else {
        0
    };
    let neutral_chars = if neutral_frac.is_finite() && neutral_frac > 0.0 {
        (neutral_frac.min(1.0) * width as f64).round() as usize
    } else {
        0
    };
    let total_used = prod_chars.saturating_add(neutral_chars);
    let (prod_chars, neutral_chars) = if total_used > width {
        let excess = total_used.saturating_sub(width);
        let new_neutral = neutral_chars.saturating_sub(excess);
        let remaining = excess.saturating_sub(neutral_chars);
        (prod_chars.saturating_sub(remaining), new_neutral)
    } else {
        (prod_chars, neutral_chars)
    };
    let unprod_chars = width.saturating_sub(prod_chars.saturating_add(neutral_chars));
    format!(
        "{}{}{}",
        "█".repeat(prod_chars).green(),
        "█".repeat(neutral_chars).yellow(),
        "█".repeat(unprod_chars).red(),
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
