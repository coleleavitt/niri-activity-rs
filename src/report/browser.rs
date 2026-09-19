//! Browser-derived reports and browser focus accounting.
//!
//! This tracker has bounded window-focus intervals. Firefox independently
//! stores lifetime aggregate page-engagement counters. Those two measurements
//! cannot be compared for a requested reporting interval without persisted
//! engagement snapshots and deltas.

use std::collections::HashMap;

use chrono::{DateTime, Local, Utc};
use owo_colors::OwoColorize;

use super::interval::load_human_intervals;
use super::{App, TimeRange, UNTIL_SENTINEL};
use crate::config::Config;
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
        // Only a browser window's title names a visited page. A non-browser
        // window that happens to share a page title is not browsing that
        // domain, and `Config::classify` already refuses the same inference.
        if !browser_profiles::is_browser_app_id(&event.app_id) {
            continue;
        }
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

/// Show Firefox's lifetime cumulative engagement counters by domain.
///
/// These values deliberately have no reporting-range argument. Firefox's
/// created/updated timestamps do not bound the time accumulated by a counter,
/// so presenting the counters as interval engagement would be misleading.
pub fn show_engagement(limit: usize) -> Result<(), Error> {
    let totals = browser_profiles::lifetime_engagement_by_domain()
        .map_err(|error| Error::NiriError(format!("Failed to read browser engagement: {error}")))?;
    let mut rows: Vec<_> = totals.into_iter().collect();
    rows.sort_by_key(|(_, (view_time, _))| std::cmp::Reverse(*view_time));

    println!(
        "{}
",
        "Lifetime browser engagement".cyan().bold()
    );
    println!(
        "{}",
        "Firefox-family cumulative counters; not a date-range report.".dimmed()
    );
    if rows.is_empty() {
        println!("{}", "No Firefox engagement data found.".yellow());
        return Ok(());
    }

    println!(
        "
{:>10}  {:>10}  {}",
        "View time".bold(),
        "Keys".bold(),
        "Domain".bold()
    );
    println!("{}", "─".repeat(66).dimmed());
    for (domain, (view_time, key_presses)) in rows.into_iter().take(limit) {
        let millis = i64::try_from(view_time.as_millis()).unwrap_or(i64::MAX);
        println!(
            "{:>10}  {:>10}  {}",
            crate::fmt::fmt_duration(millis),
            key_presses,
            truncate(&domain, 42)
        );
    }
    Ok(())
}

/// Split per-profile reads into the rows that succeeded and the ones that did
/// not.
///
/// A profile that cannot be read is not a profile without records. Collapsing
/// both into an empty list would present a failed read as a measurement.
fn partition_reads<T, E: std::fmt::Display>(
    reads: impl IntoIterator<Item = (String, Result<Vec<T>, E>)>,
) -> (Vec<T>, Vec<String>) {
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for (profile, result) in reads {
        match result {
            Ok(items) => rows.extend(items),
            Err(error) => failures.push(format!("{profile}: {error}")),
        }
    }
    (rows, failures)
}

/// Report unreadable profiles on stderr so an empty table is never mistaken
/// for a browser that recorded nothing.
fn warn_failures(kind: &str, failures: &[String]) {
    for failure in failures {
        eprintln!("{}", format!("Failed to read {kind} from {failure}").red());
    }
}

/// Show which sites lead to which, revealing how a destination is reached.
pub fn show_referrers(limit: usize) {
    let edges = match browser_profiles::referrer_edges() {
        Ok(edges) => edges,
        Err(error) => {
            eprintln!(
                "{}",
                format!("Failed to read browser history: {error}").red()
            );
            return;
        }
    };
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

/// Label a browser timestamp with the calendar date the report would use.
///
/// The browser records UTC instants. Formatting them as UTC puts late-evening
/// activity on the next day for every zone east of UTC, and formatting them
/// in the machine zone disagrees with every other surface whenever `timezone`
/// is configured. `EventInterval::local_start` resolves stored instants with
/// exactly this rule, so a download and the focus event beside it cannot land
/// on different days.
fn calendar_date(instant: Option<DateTime<Utc>>, config: &Config) -> String {
    instant.map_or_else(
        || "—".to_string(),
        |instant| {
            let local = if let Some(timezone) = config.timezone {
                instant.with_timezone(&timezone).fixed_offset()
            } else {
                instant.with_timezone(&Local).fixed_offset()
            };
            local.format("%Y-%m-%d").to_string()
        },
    )
}

/// Show downloads and address-bar searches recorded by the browser.
pub fn show_activity(config: &Config, limit: usize) {
    let profiles = match browser_profiles::discover() {
        Ok(profiles) => profiles,
        Err(error) => {
            eprintln!(
                "{}",
                format!("Failed to discover browser profiles: {error}").red()
            );
            return;
        }
    };

    let (mut downloads, failures) = partition_reads(
        profiles
            .iter()
            .map(|p| (p.name.clone(), browser_profiles::read_downloads(p))),
    );
    warn_failures("downloads", &failures);
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
        let when = calendar_date(d.started_at, config);
        println!(
            "  {} {:<44} {}",
            when.dimmed(),
            truncate(&name, 42),
            browser_profiles::domain_of(&d.url)
                .unwrap_or_else(|| "—".into())
                .dimmed()
        );
    }

    let (mut searches, failures) = partition_reads(
        profiles
            .iter()
            .map(|p| (p.name.clone(), browser_profiles::read_search_terms(p))),
    );
    warn_failures("address-bar searches", &failures);
    searches.sort_by_key(|s| std::cmp::Reverse(s.last_searched));

    println!("\n{}\n", "Recent address-bar searches".cyan().bold());
    if searches.is_empty() {
        println!(
            "{}",
            "  none recorded (Firefox does not expose these)".dimmed()
        );
    }
    for s in searches.iter().take(limit) {
        let when = calendar_date(s.last_searched, config);
        println!("  {} {}", when.dimmed(), truncate(&s.term, 60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn non_browser_window_sharing_a_page_title_is_not_browser_focus() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("database");
        init_db(&conn).expect("schema");
        run_migrations(&mut conn, &Config::default()).expect("migrations");
        conn.execute(
            "INSERT INTO events (
                 timestamp, app_id, title, category, active_ms, passive_ms, idle_ms
             ) VALUES (
                 '2026-01-02T10:00:00+00:00', 'foot', 'Example',
                 'neutral', 60000, 0, 0
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

        assert_eq!(totals.get("example.com"), None);
    }

    #[test]
    fn unreadable_profiles_are_reported_instead_of_counted_as_empty() {
        let (rows, failures) = partition_reads([
            ("good".to_string(), Ok::<Vec<u8>, String>(vec![1, 2])),
            ("locked".to_string(), Err("database is locked".to_string())),
        ]);

        assert_eq!(rows, vec![1, 2]);
        assert_eq!(failures, vec!["locked: database is locked".to_string()]);
    }

    #[test]
    fn browser_timestamps_are_labelled_with_the_configured_report_timezone() {
        let instant = DateTime::parse_from_rfc3339("2026-01-01T23:30:00+00:00")
            .expect("valid instant")
            .with_timezone(&Utc);
        let utc = Config {
            timezone: Some(chrono_tz::UTC),
            ..Config::default()
        };
        // Moscow is +03:00 all year, so the same instant is already the 2nd
        // there whatever the machine zone says.
        let moscow = Config {
            timezone: Some(chrono_tz::Europe::Moscow),
            ..Config::default()
        };

        assert_eq!(calendar_date(Some(instant), &utc), "2026-01-01");
        assert_eq!(calendar_date(Some(instant), &moscow), "2026-01-02");
        assert_eq!(calendar_date(None, &utc), "—");
    }
}
