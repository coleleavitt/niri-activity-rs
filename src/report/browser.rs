//! Cross-check window-focus tracking against the browser's own measurements.
//!
//! This tracker times a window while it holds focus. Firefox independently
//! times each *page* while it is focused and visible, and counts keystrokes
//! against it. The two disagree in informative ways: a video playing in an
//! unfocused window is view time to Firefox but not focus time here, and a
//! browser window left open on a docs page is focus time here but idle there.

use std::collections::HashMap;
use std::time::Duration;

use owo_colors::OwoColorize;
use rusqlite::params;

use super::{App, MS_PER_HOUR, TimeRange, UNTIL_SENTINEL};
use crate::error::Error;
use crate::fmt::truncate;

/// Focus time this tracker recorded, per domain, for a range.
fn tracked_by_domain(app: &App, range: TimeRange) -> Result<HashMap<String, i64>, Error> {
    let bounds = range.resolve(&app.config)?;
    let until = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);
    let mut stmt = app.conn.prepare(
        "SELECT title, SUM(active_ms + COALESCE(passive_ms, 0)) AS ms
         FROM events
         WHERE timestamp >= ?1 AND timestamp < ?2
           AND title IS NOT NULL AND title != ''
         GROUP BY title",
    )?;

    let mut totals: HashMap<String, i64> = HashMap::new();
    let rows = stmt.query_map(params![&bounds.since_utc, until], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (title, ms) = row?;
        // A title that resolves to a domain came from a browser, so there is
        // no need to guess at which app ids are browsers.
        let key = browser_profiles::strip_window_suffix(&title);
        if let Some(domain) = app
            .config
            .title_domains
            .get(key)
            .or_else(|| app.config.title_domains.get(&title))
        {
            *totals.entry(domain.clone()).or_default() += ms;
        }
    }
    Ok(totals)
}

fn hours(ms: i64) -> f64 {
    ms as f64 / MS_PER_HOUR as f64
}

fn fmt_hm(ms: i64) -> String {
    format!("{}h {:02}m", ms / MS_PER_HOUR, (ms % MS_PER_HOUR) / 60_000)
}

/// Compare tracked focus time against browser-measured view time by domain.
pub fn show_engagement(app: &App, range: TimeRange, limit: usize) -> Result<(), Error> {
    let bounds = range.resolve(&app.config)?;
    let since = app.config.parse_timestamp_to_local(&bounds.since_utc);
    let until = bounds
        .until_utc
        .as_deref()
        .and_then(|u| app.config.parse_timestamp_to_local(u));
    let measured = browser_profiles::engagement_by_domain_between(
        since.map(|d| d.with_timezone(&chrono::Utc)),
        until.map(|d| d.with_timezone(&chrono::Utc)),
    )
    .unwrap_or_default();
    if measured.is_empty() {
        println!(
            "{}",
            "No browser engagement data. Firefox-family browsers record this; \
             Chromium does not."
                .yellow()
        );
        return Ok(());
    }
    let tracked = tracked_by_domain(app, range)?;

    println!(
        "{}\n",
        "Focus time (this tracker) vs view time (browser's own)"
            .cyan()
            .bold()
    );
    println!(
        "{:<34} {:>10} {:>10} {:>9} {:>10}",
        "Domain".bold(),
        "Tracked".bold(),
        "Browser".bold(),
        "Ratio".bold(),
        "Keys".bold()
    );
    println!("{}", "─".repeat(77).dimmed());

    let mut rows: Vec<(&String, i64, Duration, i64)> = measured
        .iter()
        .map(|(domain, (view, keys))| {
            let t = tracked.get(domain).copied().unwrap_or(0);
            (domain, t, *view, *keys)
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.2));

    for (domain, tracked_ms, view, keys) in rows.into_iter().take(limit) {
        let view_ms = i64::try_from(view.as_millis()).unwrap_or(i64::MAX);
        let r = hours(tracked_ms) / hours(view_ms.max(1));
        let ratio = if view_ms > 0 {
            format!("{r:.2}x")
        } else {
            "—".to_string()
        };
        let ratio = if r < 0.5 {
            format!("{}", ratio.red())
        } else if r > 1.5 {
            format!("{}", ratio.yellow())
        } else {
            format!("{}", ratio.green())
        };
        println!(
            "{:<34} {:>10} {:>10} {:>9} {:>10}",
            truncate(domain, 32),
            fmt_hm(tracked_ms),
            fmt_hm(view_ms).dimmed(),
            ratio,
            keys
        );
    }

    println!(
        "\n{}",
        "Ratio < 1 means the browser saw more time than this tracker did, which is what\n\
         media playing in an unfocused window looks like. Ratio > 1 means a window held\n\
         focus while the page itself was idle."
            .dimmed()
    );
    Ok(())
}

/// Show which sites lead to which, revealing how a destination is reached.
pub fn show_referrers(limit: usize) {
    let edges = browser_profiles::referrer_edges().unwrap_or_default();
    if edges.is_empty() {
        println!("{}", "No referrer data in browser history.".yellow());
        return;
    }

    // Self-referring hops are in-site navigation rather than a path between
    // sites, and must go before the limit or they consume the whole list.
    let mut rows: Vec<_> = edges.into_iter().filter(|((f, t), _)| f != t).collect();
    rows.sort_by_key(|&(_, hops)| std::cmp::Reverse(hops));

    println!("{}\n", "How you arrive at sites".cyan().bold());
    println!(
        "{:>7}  {:<30} {:<30}",
        "Hops".bold(),
        "From".bold(),
        "To".bold()
    );
    println!("{}", "─".repeat(71).dimmed());
    for ((from, to), hops) in rows.into_iter().take(limit) {
        println!(
            "{:>7}  {:<30} {:<30}",
            hops,
            truncate(&from, 28),
            truncate(&to, 28).bold()
        );
    }
}

/// Show downloads and address-bar searches recorded by the browser.
pub fn show_activity(limit: usize) {
    let profiles = browser_profiles::discover().unwrap_or_default();

    let mut downloads: Vec<browser_profiles::Download> = profiles
        .iter()
        .filter_map(|p| browser_profiles::read_downloads(p).ok())
        .flatten()
        .collect();
    downloads.sort_by_key(|d| std::cmp::Reverse(d.started_at));

    println!("{}\n", "Recent downloads".cyan().bold());
    if downloads.is_empty() {
        println!(
            "{}",
            "  none recorded (Firefox does not expose these)".dimmed()
        );
    }
    for d in downloads.iter().take(limit) {
        let name = d.target_path.file_name().map_or_else(
            || d.target_path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let when = d
            .started_at
            .map_or_else(|| "—".to_string(), |t| t.format("%Y-%m-%d").to_string());
        println!(
            "  {} {:<44} {}",
            when.dimmed(),
            truncate(&name, 42),
            browser_profiles::domain_of(&d.url)
                .unwrap_or_else(|| "—".into())
                .dimmed()
        );
    }

    let mut searches: Vec<browser_profiles::SearchTerm> = profiles
        .iter()
        .filter_map(|p| browser_profiles::read_search_terms(p).ok())
        .flatten()
        .collect();
    searches.sort_by_key(|s| std::cmp::Reverse(s.last_searched));

    println!("\n{}\n", "Recent address-bar searches".cyan().bold());
    if searches.is_empty() {
        println!(
            "{}",
            "  none recorded (Firefox does not expose these)".dimmed()
        );
    }
    for s in searches.iter().take(limit) {
        let when = s
            .last_searched
            .map_or_else(|| "—".to_string(), |t| t.format("%Y-%m-%d").to_string());
        println!("  {} {}", when.dimmed(), truncate(&s.term, 60));
    }
}
