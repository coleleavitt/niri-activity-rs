//! Browser-derived reports and browser focus accounting.
//!
//! This tracker has bounded window-focus intervals. Firefox independently
//! stores lifetime aggregate page-engagement counters. Those two measurements
//! cannot be compared for a requested reporting interval without persisted
//! engagement snapshots and deltas.

use std::collections::HashMap;

use owo_colors::OwoColorize;

use super::interval::load_human_intervals;
use super::{App, TimeRange, UNTIL_SENTINEL};
use crate::error::Error;
use crate::fmt::truncate;

/// Focus time this tracker recorded, per domain, for a range.
///
/// This uses the canonical interval loader so events overlapping either range
/// boundary are clipped and focused idle time remains part of window focus.
#[expect(
    dead_code,
    reason = "reserved for a future interval-safe browser report"
)]
fn tracked_by_domain(app: &App, range: TimeRange) -> Result<HashMap<String, i64>, Error> {
    let bounds = range.resolve(&app.config)?;
    let until = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);
    tracked_by_domain_between(app, &bounds.since_utc, until)
}

fn tracked_by_domain_between(
    app: &App,
    since: &str,
    until: &str,
) -> Result<HashMap<String, i64>, Error> {
    let events = load_human_intervals(&app.conn, &app.config, since, until)?;
    let mut totals: HashMap<String, i64> = HashMap::new();
    for event in events {
        let title = &event.title;
        let key = browser_profiles::strip_window_suffix(title);
        if let Some(domain) = app
            .config
            .title_domains
            .get(key)
            .or_else(|| app.config.title_domains.get(title))
        {
            *totals.entry(domain.clone()).or_default() += event.total_ms();
        }
    }
    Ok(totals)
}

/// Browser engagement cannot be compared to a bounded tracker range.
///
/// Firefox exposes lifetime cumulative counters. Its created/updated
/// timestamps do not say when the accumulated engagement occurred, so using
/// them as interval bounds would silently overcount arbitrary history.
pub fn show_engagement(_app: &App, _range: TimeRange, _limit: usize) -> Result<(), Error> {
    Err(Error::InvalidArgument(
        "bounded browser engagement is unsupported: Firefox exposes lifetime aggregate counters, not interval engagement"
            .to_string(),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::{init_db, run_migrations};

    #[test]
    fn tracked_focus_includes_idle_and_clips_overlapping_events() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("database");
        init_db(&conn).expect("schema");
        run_migrations(&mut conn, &Config::default()).expect("migrations");
        conn.execute(
            "INSERT INTO events (
                 timestamp, app_id, title, category, active_ms, passive_ms, idle_ms
             ) VALUES (
                 '2026-01-01T23:59:00+00:00', 'zen', 'Example — Zen Browser',
                 'neutral', 0, 0, 120000
             )",
            [],
        )
        .expect("event");
        let mut config = Config::default();
        config
            .title_domains
            .insert("Example".to_string(), "example.com".to_string());
        let app = App { config, conn };

        let totals = tracked_by_domain_between(
            &app,
            "2026-01-02T00:00:00+00:00",
            "2026-01-03T00:00:00+00:00",
        )
        .expect("tracked totals");

        assert_eq!(totals.get("example.com"), Some(&60_000));
    }

    #[test]
    fn bounded_engagement_fails_closed() {
        let conn = rusqlite::Connection::open_in_memory().expect("database");
        let app = App {
            config: Config::default(),
            conn,
        };
        let error = show_engagement(&app, TimeRange::Days(7), 25).expect_err("unsupported");

        assert!(error.to_string().contains("lifetime aggregate counters"));
    }
}
