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

pub use browser::{Browser, Family, strip_window_suffix};
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
/// the domain behind it. Later profiles win on collision, which is arbitrary
/// but stable — a title mapping to two domains is genuinely ambiguous.
pub fn title_to_domain() -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for (_, visits) in read_all_history()? {
        for visit in visits {
            if let (Some(title), Some(domain)) = (visit.title.as_ref(), visit.domain()) {
                map.insert(title.clone(), domain);
            }
        }
    }
    Ok(map)
}

/// Total engagement per domain across every profile that measures it.
///
/// Returns view time paired with keystroke count, which distinguishes reading
/// from working: an hour on a docs site with no keystrokes is not an hour in
/// an editor.
pub fn engagement_by_domain() -> Result<HashMap<String, (std::time::Duration, i64)>> {
    engagement_by_domain_between(None, None)
}

fn in_window(
    row: &Engagement,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    if since.is_none() && until.is_none() {
        return true;
    }
    let Some(at) = row.updated_at.or(row.created_at) else {
        return false;
    };
    !(since.is_some_and(|s| at < s) || until.is_some_and(|u| at >= u))
}

/// Same as [`engagement_by_domain`], restricted to a time window.
///
/// Rows with no timestamp are excluded whenever either bound is set, since
/// they cannot be placed inside or outside the window.
pub fn engagement_by_domain_between(
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<HashMap<String, (std::time::Duration, i64)>> {
    let mut map: HashMap<String, (std::time::Duration, i64)> = HashMap::new();
    for profile in discover()? {
        let Ok(rows) = read_engagement(&profile) else {
            continue;
        };
        for row in rows {
            if !in_window(&row, since, until) {
                continue;
            }
            if let Some(domain) = row.domain() {
                let entry = map.entry(domain).or_default();
                entry.0 = entry.0.saturating_add(row.view_time);
                entry.1 = entry.1.saturating_add(row.key_presses);
            }
        }
    }
    Ok(map)
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
        Visit {
            url: url.to_string(),
            title: None,
            visit_count: 0,
            last_visit: None,
            description: None,
            site_name: None,
            frecency: None,
            typed: false,
        }
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

    fn engagement_at(url: &str, at: Option<&str>, secs: u64) -> Engagement {
        let stamp = at.map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .expect("valid rfc3339")
                .with_timezone(&chrono::Utc)
        });
        Engagement {
            url: url.to_string(),
            title: None,
            view_time: std::time::Duration::from_secs(secs),
            typing_time: std::time::Duration::ZERO,
            key_presses: 0,
            scrolling_time: std::time::Duration::ZERO,
            scrolling_distance: 0,
            referrer: None,
            is_media: false,
            created_at: stamp,
            updated_at: stamp,
        }
    }

    fn utc(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .expect("valid rfc3339")
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn window_excludes_rows_outside_bounds() {
        let inside = engagement_at("https://a.com/", Some("2026-08-12T12:00:00Z"), 60);
        let before = engagement_at("https://a.com/", Some("2026-08-01T12:00:00Z"), 60);
        let after = engagement_at("https://a.com/", Some("2026-08-20T12:00:00Z"), 60);

        let since = Some(utc("2026-08-10T00:00:00Z"));
        let until = Some(utc("2026-08-17T00:00:00Z"));

        assert!(in_window(&inside, since, until));
        assert!(!in_window(&before, since, until));
        assert!(!in_window(&after, since, until));
    }

    #[test]
    fn window_end_is_exclusive() {
        let row = engagement_at("https://a.com/", Some("2026-08-17T00:00:00Z"), 60);
        assert!(!in_window(&row, None, Some(utc("2026-08-17T00:00:00Z"))));
    }

    #[test]
    fn undated_rows_count_only_when_unbounded() {
        let row = engagement_at("https://a.com/", None, 60);
        assert!(in_window(&row, None, None));
        assert!(!in_window(&row, Some(utc("2026-08-10T00:00:00Z")), None));
    }
}
