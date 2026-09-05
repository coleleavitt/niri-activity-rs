//! Discover browser profiles and read their history.
//!
//! Two database schemas cover every mainstream browser: Firefox's
//! `places.sqlite` and Chromium's `History`. This crate hides that split
//! behind one API, so callers ask for profiles and get back the same types
//! regardless of which browser produced them.
//!
//! ```no_run
//! # fn main() -> Result<(), browser_profiles::Error> {
//! for profile in browser_profiles::discover()? {
//!     let visits = browser_profiles::read_history(&profile)?;
//!     println!(
//!         "{} {}: {} urls",
//!         profile.browser,
//!         profile.name,
//!         visits.len()
//!     );
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Coverage
//!
//! Not every browser records every signal. Reads for an unsupported source
//! return an empty vector rather than an error:
//!
//! | Data                            | Firefox | Chromium |
//! |---------------------------------|---------|----------|
//! | [`Visit`] aggregates            | yes     | yes      |
//! | [`VisitRecord`] per-visit       | yes     | yes      |
//! | [`VisitRecord::duration`]       | no      | yes      |
//! | [`Engagement`] view/typing time | yes     | no       |
//! | [`Bookmark`]                    | yes     | yes      |
//! | [`Download`]                    | no      | yes      |
//! | [`SearchTerm`]                  | no      | yes      |

mod backend;
mod browser;
mod discover;
mod error;
mod model;
mod sql;
mod time;

use std::collections::HashMap;

pub use browser::{Browser, Family, is_browser_app_id, strip_window_suffix};
pub use discover::{discover, discover_browser, discover_in, history_db_for};
pub use error::{Error, Result};
pub use model::{
    Bookmark, Download, Engagement, Profile, SearchTerm, Visit, VisitRecord, VisitType, domain_of,
};

/// Read a profile's per-URL history aggregates.
pub fn read_history(profile: &Profile) -> Result<Vec<Visit>> {
    backend::read_visits(&profile.history_db, profile.family())
}

/// Read individual navigation events, ordered oldest first.
///
/// Where [`read_history`] gives one row per URL, this gives one row per visit,
/// which is what makes session reconstruction and referrer analysis possible.
pub fn read_visits(profile: &Profile) -> Result<Vec<VisitRecord>> {
    backend::read_visit_records(&profile.history_db, profile.family())
}

/// Read the browser's own per-page engagement measurements.
///
/// Firefox tracks focused-and-visible time, keystrokes, and scrolling per
/// page. This is an independent measurement of attention that can be compared
/// against an external window-focus tracker. Empty on Chromium.
pub fn read_engagement(profile: &Profile) -> Result<Vec<Engagement>> {
    backend::read_engagement(&profile.history_db, profile.family())
}

pub fn read_bookmarks(profile: &Profile) -> Result<Vec<Bookmark>> {
    backend::read_bookmarks(profile)
}

/// Read download history. Empty on Firefox.
pub fn read_downloads(profile: &Profile) -> Result<Vec<Download>> {
    backend::read_downloads(&profile.history_db, profile.family())
}

/// Read address-bar search queries. Empty on Firefox.
pub fn read_search_terms(profile: &Profile) -> Result<Vec<SearchTerm>> {
    backend::read_search_terms(&profile.history_db, profile.family())
}

/// Read history from every discovered profile, skipping unreadable ones.
///
/// A single locked or corrupt profile should not sink a whole-system scan, so
/// failures are dropped rather than propagated.
pub fn read_all_history() -> Result<Vec<(Profile, Vec<Visit>)>> {
    Ok(discover()?
        .into_iter()
        .filter_map(|p| read_history(&p).ok().map(|v| (p, v)))
        .collect())
}

/// Build a page-title to domain lookup across every profile.
///
/// Window-title-based activity trackers only see a page's title; this recovers
/// the domain behind it. A title is retained only when every URL observed for
/// it resolves to the same domain. Ambiguous or unresolvable titles are omitted
/// so profile discovery order cannot change classification.
pub fn title_to_domain() -> Result<HashMap<String, String>> {
    let histories = read_all_history()?;
    Ok(title_domains_from_visits(
        histories.into_iter().flat_map(|(_, visits)| visits),
    ))
}

fn title_domains_from_visits(visits: impl IntoIterator<Item = Visit>) -> HashMap<String, String> {
    let mut candidates: HashMap<String, Option<String>> = HashMap::new();

    for visit in visits {
        let domain = visit.domain();
        let Some(title) = visit.title else {
            continue;
        };
        candidates
            .entry(title)
            .and_modify(|candidate| {
                if *candidate != domain {
                    *candidate = None;
                }
            })
            .or_insert(domain);
    }

    candidates
        .into_iter()
        .filter_map(|(title, domain)| domain.map(|domain| (title, domain)))
        .collect()
}

/// Lifetime engagement aggregates per domain across every profile that measures
/// it.
///
/// Firefox stores cumulative per-page counters, not interval samples. The
/// timestamps on [`Engagement`] rows say when a counter was created or last
/// updated; they do not bound the time accumulated by that counter. Therefore
/// these totals must never be filtered by timestamp and presented as engagement
/// for a reporting interval.
///
/// Returns view time paired with keystroke count, which distinguishes reading
/// from working: an hour on a docs site with no keystrokes is not an hour in
/// an editor.
pub fn lifetime_engagement_by_domain() -> Result<HashMap<String, (std::time::Duration, i64)>> {
    let mut map = HashMap::new();
    for profile in discover()? {
        let Ok(rows) = read_engagement(&profile) else {
            continue;
        };
        add_lifetime_engagement(&mut map, rows);
    }
    Ok(map)
}

fn add_lifetime_engagement(
    map: &mut HashMap<String, (std::time::Duration, i64)>,
    rows: impl IntoIterator<Item = Engagement>,
) {
    for row in rows {
        if let Some(domain) = row.domain() {
            let entry = map.entry(domain).or_default();
            entry.0 = entry.0.saturating_add(row.view_time);
            entry.1 = entry.1.saturating_add(row.key_presses);
        }
    }
}

/// Lifetime engagement aggregates per domain.
///
/// This compatibility alias retains the original API name. Prefer
/// [`lifetime_engagement_by_domain`] where the lifetime semantics should be
/// explicit.
pub fn engagement_by_domain() -> Result<HashMap<String, (std::time::Duration, i64)>> {
    lifetime_engagement_by_domain()
}

/// Count navigations between domains, as `(from, to) -> hops`.
///
/// Surfaces how a user actually reaches a site — a search engine that
/// repeatedly leads to the same destination is a different behaviour from
/// typing that destination directly.
pub fn referrer_edges() -> Result<HashMap<(String, String), usize>> {
    let mut edges: HashMap<(String, String), usize> = HashMap::new();
    for profile in discover()? {
        let Ok(records) = read_visits(&profile) else {
            continue;
        };
        for record in records {
            if let (Some(from), Some(to)) = (record.referrer_domain(), record.domain()) {
                *edges.entry((from, to)).or_default() += 1;
            }
        }
    }
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(url: &str) -> Visit {
        titled_visit(url, None)
    }

    fn titled_visit(url: &str, title: Option<&str>) -> Visit {
        Visit {
            url: url.to_string(),
            title: title.map(str::to_owned),
            visit_count: 0,
            last_visit: None,
            description: None,
            site_name: None,
            frecency: None,
            typed: false,
        }
    }

    #[test]
    fn title_domains_keep_same_domain_duplicates() {
        let visits = [
            titled_visit("https://docs.example.com/one", Some("Reference")),
            titled_visit("https://docs.example.com/two", Some("Reference")),
        ];

        assert_eq!(
            title_domains_from_visits(visits).get("Reference"),
            Some(&"docs.example.com".to_string())
        );
    }

    #[test]
    fn title_domains_omit_ambiguous_titles_regardless_of_profile_order() {
        let first = titled_visit("https://one.example/page", Some("Shared title"));
        let second = titled_visit("https://two.example/page", Some("Shared title"));

        let forward = title_domains_from_visits([first.clone(), second.clone()]);
        let reverse = title_domains_from_visits([second, first]);

        assert!(!forward.contains_key("Shared title"));
        assert_eq!(forward, reverse);
    }

    #[test]
    fn title_domains_omit_titles_with_unresolvable_urls_regardless_of_order() {
        let resolved = titled_visit("https://example.com/page", Some("Shared title"));
        let unresolved = titled_visit("about:blank", Some("Shared title"));

        let forward = title_domains_from_visits([resolved.clone(), unresolved.clone()]);
        let reverse = title_domains_from_visits([unresolved, resolved]);

        assert!(!forward.contains_key("Shared title"));
        assert_eq!(forward, reverse);
    }

    #[test]
    fn extracts_domain_from_url() {
        assert_eq!(
            visit("https://www.example.com/a/b?c=1").domain().as_deref(),
            Some("www.example.com")
        );
    }

    #[test]
    fn keeps_port_to_distinguish_local_services() {
        assert_eq!(
            visit("http://localhost:5173/app").domain().as_deref(),
            Some("localhost:5173")
        );
    }

    #[test]
    fn strips_userinfo() {
        assert_eq!(
            visit("https://user:pw@example.com/x").domain().as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn lowercases_host() {
        assert_eq!(
            visit("https://EXAMPLE.com/X").domain().as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn handles_url_with_no_path() {
        assert_eq!(
            visit("https://example.com").domain().as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn rejects_urls_without_host() {
        assert_eq!(visit("about:blank").domain(), None);
        assert_eq!(visit("file:///tmp/x").domain(), None);
    }

    #[test]
    fn lifetime_engagement_does_not_filter_counters_by_update_timestamp() {
        let timestamp = chrono::DateTime::parse_from_rfc3339("2026-08-12T12:00:00Z")
            .expect("timestamp")
            .with_timezone(&chrono::Utc);
        let row = Engagement {
            url: "https://docs.example.com/page".to_string(),
            title: None,
            view_time: std::time::Duration::from_secs(3_600),
            typing_time: std::time::Duration::ZERO,
            key_presses: 42,
            scrolling_time: std::time::Duration::ZERO,
            scrolling_distance: 0,
            referrer: None,
            is_media: false,
            created_at: Some(timestamp - chrono::Duration::days(30)),
            updated_at: Some(timestamp),
        };
        let mut totals = HashMap::new();

        add_lifetime_engagement(&mut totals, [row]);

        assert_eq!(
            totals.get("docs.example.com"),
            Some(&(std::time::Duration::from_secs(3_600), 42))
        );
    }
}
