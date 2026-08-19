use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::browser::{Browser, Family};

/// A discovered browser profile on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub browser: Browser,
    /// Profile directory name, e.g. `Default` or `hz8tcfb3.Default (release)`.
    pub name: String,
    pub path: PathBuf,
    /// Path to the history database inside `path`.
    pub history_db: PathBuf,
}

impl Profile {
    pub fn family(&self) -> Family {
        self.browser.family()
    }

    /// Path to a sibling file in the profile directory, if it exists.
    pub fn sibling(&self, filename: &str) -> Option<PathBuf> {
        let path = self.path.join(filename);
        path.exists().then_some(path)
    }
}

/// One URL and its aggregate statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub url: String,
    pub title: Option<String>,
    pub visit_count: i64,
    pub last_visit: Option<DateTime<Utc>>,
    /// Page description meta tag. Firefox only.
    pub description: Option<String>,
    /// Site name meta tag, e.g. "GitHub". Firefox only.
    pub site_name: Option<String>,
    /// Ranking score combining recency and frequency. Firefox only.
    pub frecency: Option<i64>,
    /// Whether the user reached this URL by typing it, at least once.
    pub typed: bool,
}

impl Visit {
    /// Host portion of the URL, lowercased, without scheme, credentials, or
    /// path. Returns `None` for URLs with no host (`about:`, `file:`, …).
    pub fn domain(&self) -> Option<String> {
        domain_of(&self.url)
    }
}

/// Host portion of a URL, lowercased, port retained.
pub fn domain_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Strip userinfo; keep the port, since localhost:3000 and localhost:5173
    // are distinct services worth telling apart.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.is_empty() {
        return None;
    }
    Some(host.to_lowercase())
}

/// How the user arrived at a page.
///
/// Firefox and Chromium enumerate navigation causes differently; this is the
/// intersection that carries intent signal. [`VisitType::Typed`] and
/// [`VisitType::Bookmark`] are deliberate, [`VisitType::Redirect`] and
/// [`VisitType::Subframe`] are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisitType {
    /// Followed a link.
    Link,
    /// Typed into the address bar.
    Typed,
    /// Opened from a bookmark.
    Bookmark,
    /// Loaded inside a frame or embed.
    Subframe,
    /// Server- or client-side redirect.
    Redirect,
    /// Page reload.
    Reload,
    /// Started a download.
    Download,
    /// Search-engine or omnibox-generated navigation.
    Generated,
    Other(i64),
}

impl VisitType {
    /// Whether the user deliberately chose this destination.
    ///
    /// Redirects, subframes, and reloads happen *to* the user; the rest are
    /// chosen. Useful for separating intentional browsing from drift.
    pub fn is_deliberate(self) -> bool {
        matches!(
            self,
            VisitType::Typed | VisitType::Bookmark | VisitType::Generated | VisitType::Link
        )
    }

    /// Firefox `moz_historyvisits.visit_type`.
    pub fn from_firefox(raw: i64) -> Self {
        match raw {
            1 => VisitType::Link,
            2 => VisitType::Typed,
            3 => VisitType::Bookmark,
            4 | 8 => VisitType::Subframe,
            5 | 6 => VisitType::Redirect,
            7 => VisitType::Download,
            9 => VisitType::Reload,
            other => VisitType::Other(other),
        }
    }

    /// Chromium `visits.transition`. The core type is the low byte; the upper
    /// bits are qualifier flags such as redirect chain position.
    pub fn from_chromium(raw: i64) -> Self {
        match raw & 0xFF {
            0 => VisitType::Link,
            1 => VisitType::Typed,
            2 => VisitType::Bookmark,
            3 | 4 => VisitType::Subframe,
            5 | 9 | 10 => VisitType::Generated,
            8 => VisitType::Reload,
            other => VisitType::Other(other),
        }
    }
}

/// A single navigation event, as opposed to [`Visit`]'s per-URL aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisitRecord {
    pub url: String,
    pub title: Option<String>,
    pub visited_at: DateTime<Utc>,
    pub visit_type: VisitType,
    /// URL of the page that linked here, when the browser recorded one.
    pub referrer: Option<String>,
    /// How long the page stayed open. Chromium records this directly; Firefox
    /// does not, so it is `None` there — use [`Engagement::view_time`] instead.
    pub duration: Option<Duration>,
}

impl VisitRecord {
    pub fn domain(&self) -> Option<String> {
        domain_of(&self.url)
    }

    pub fn referrer_domain(&self) -> Option<String> {
        self.referrer.as_deref().and_then(domain_of)
    }
}

/// Per-page engagement as measured by the browser itself.
///
/// Firefox records this in `moz_places_metadata`. It counts only time the page
/// was focused and visible, which makes it a useful cross-check against an
/// external window-focus tracker. Chromium has no equivalent table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engagement {
    pub url: String,
    pub title: Option<String>,
    /// Time the page was focused and visible.
    pub view_time: Duration,
    /// Time spent actively typing on the page.
    pub typing_time: Duration,
    pub key_presses: i64,
    pub scrolling_time: Duration,
    /// Scroll distance in CSS pixels.
    pub scrolling_distance: i64,
    /// URL of the page that led here.
    pub referrer: Option<String>,
    /// True when the page was primarily video or audio.
    pub is_media: bool,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Engagement {
    pub fn domain(&self) -> Option<String> {
        domain_of(&self.url)
    }

    pub fn referrer_domain(&self) -> Option<String> {
        self.referrer.as_deref().and_then(domain_of)
    }
}

/// A bookmarked page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub url: String,
    pub title: Option<String>,
    /// Containing folder title, when known.
    pub folder: Option<String>,
    pub added_at: Option<DateTime<Utc>>,
}

impl Bookmark {
    pub fn domain(&self) -> Option<String> {
        domain_of(&self.url)
    }
}

/// A downloaded file and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub url: String,
    pub target_path: PathBuf,
    pub mime_type: Option<String>,
    pub total_bytes: i64,
    pub received_bytes: i64,
    /// Page the download was started from.
    pub referrer: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// A search query typed into the address bar or a search engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTerm {
    pub term: String,
    /// Search results URL the term produced.
    pub url: String,
    pub last_searched: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firefox_visit_types_map() {
        assert_eq!(VisitType::from_firefox(1), VisitType::Link);
        assert_eq!(VisitType::from_firefox(2), VisitType::Typed);
        assert_eq!(VisitType::from_firefox(5), VisitType::Redirect);
        assert_eq!(VisitType::from_firefox(6), VisitType::Redirect);
        assert_eq!(VisitType::from_firefox(9), VisitType::Reload);
        assert_eq!(VisitType::from_firefox(99), VisitType::Other(99));
    }

    #[test]
    fn chromium_transition_masks_qualifier_bits() {
        // 0x18000001 = TYPED with CHAIN_START|CHAIN_END qualifiers set.
        assert_eq!(VisitType::from_chromium(0x1800_0001), VisitType::Typed);
        assert_eq!(VisitType::from_chromium(0), VisitType::Link);
        assert_eq!(VisitType::from_chromium(8), VisitType::Reload);
    }

    #[test]
    fn redirects_and_reloads_are_not_deliberate() {
        assert!(VisitType::Typed.is_deliberate());
        assert!(VisitType::Bookmark.is_deliberate());
        assert!(!VisitType::Redirect.is_deliberate());
        assert!(!VisitType::Subframe.is_deliberate());
        assert!(!VisitType::Reload.is_deliberate());
    }

    #[test]
    fn domain_of_handles_common_shapes() {
        assert_eq!(domain_of("https://a.com/x").as_deref(), Some("a.com"));
        assert_eq!(
            domain_of("http://localhost:3000").as_deref(),
            Some("localhost:3000")
        );
        assert_eq!(domain_of("about:blank"), None);
    }
}
