use std::fmt;

/// The storage schema a browser uses for its history database.
///
/// Every browser in the wild descends from one of two lineages, and the
/// lineage — not the brand — determines how history is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// `places.sqlite`, `moz_places`, timestamps in microseconds since the
    /// Unix epoch.
    Firefox,
    /// `History`, `urls`, timestamps in microseconds since 1601-01-01 (WebKit
    /// epoch).
    Chromium,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Browser {
    Firefox,
    Zen,
    LibreWolf,
    Waterfox,
    TorBrowser,
    Camoufox,
    Chrome,
    Chromium,
    Brave,
    Edge,
    Vivaldi,
    Opera,
}

impl Browser {
    pub const ALL: &'static [Browser] = &[
        Browser::Firefox,
        Browser::Zen,
        Browser::LibreWolf,
        Browser::Waterfox,
        Browser::TorBrowser,
        Browser::Camoufox,
        Browser::Chrome,
        Browser::Chromium,
        Browser::Brave,
        Browser::Edge,
        Browser::Vivaldi,
        Browser::Opera,
    ];

    pub fn family(self) -> Family {
        match self {
            Browser::Firefox
            | Browser::Zen
            | Browser::LibreWolf
            | Browser::Waterfox
            | Browser::TorBrowser
            | Browser::Camoufox => Family::Firefox,
            Browser::Chrome
            | Browser::Chromium
            | Browser::Brave
            | Browser::Edge
            | Browser::Vivaldi
            | Browser::Opera => Family::Chromium,
        }
    }

    /// Directories, relative to `$HOME`, that may contain this browser's
    /// profiles. Several are listed per browser because packaging (native,
    /// Flatpak, Snap) moves the root.
    pub fn profile_roots(self) -> &'static [&'static str] {
        match self {
            Browser::Firefox => &[
                ".mozilla/firefox",
                ".var/app/org.mozilla.firefox/.mozilla/firefox",
                "snap/firefox/common/.mozilla/firefox",
            ],
            Browser::Zen => &[".zen", ".var/app/app.zen_browser.zen/.zen"],
            Browser::LibreWolf => &[
                ".librewolf",
                ".var/app/io.gitlab.librewolf-community/.librewolf",
            ],
            Browser::Waterfox => &[".waterfox"],
            Browser::TorBrowser => &[
                ".local/share/torbrowser/tbb/x86_64/tor-browser/Browser/TorBrowser/Data/Browser",
                ".tor-browser/app/Browser/TorBrowser/Data/Browser",
            ],
            Browser::Camoufox => &[".camoufox", ".config/camoufox"],
            Browser::Chrome => &[
                ".config/google-chrome",
                ".var/app/com.google.Chrome/config/google-chrome",
            ],
            Browser::Chromium => &[
                ".config/chromium",
                ".var/app/org.chromium.Chromium/config/chromium",
                "snap/chromium/common/chromium",
            ],
            Browser::Brave => &[
                ".config/BraveSoftware/Brave-Browser",
                ".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser",
            ],
            Browser::Edge => &[
                ".config/microsoft-edge",
                ".var/app/com.microsoft.Edge/config/microsoft-edge",
            ],
            Browser::Vivaldi => &[
                ".config/vivaldi",
                ".var/app/com.vivaldi.Vivaldi/config/vivaldi",
            ],
            Browser::Opera => &[".config/opera", ".var/app/com.opera.Opera/config/opera"],
        }
    }

    /// Branding a browser appends to the window title, one entry per release
    /// channel: Zen Twilight says "Zen Twilight" where stable says "Zen
    /// Browser". Channels share a profile root, so they are variants here
    /// rather than separate browsers.
    pub fn window_suffixes(self) -> &'static [&'static str] {
        match self {
            Browser::Firefox => &[
                "Mozilla Firefox",
                "Firefox Nightly",
                "Firefox Developer Edition",
                "Firefox",
            ],
            Browser::Zen => &["Zen Browser", "Zen Twilight", "Twilight", "Zen"],
            Browser::Chrome => &["Google Chrome", "Google Chrome Canary"],
            Browser::Chromium => &["Chromium"],
            Browser::TorBrowser => &["Tor Browser"],
            Browser::LibreWolf => &["LibreWolf"],
            Browser::Waterfox => &["Waterfox"],
            Browser::Camoufox => &["Camoufox"],
            Browser::Brave => &["Brave"],
            Browser::Edge => &["Microsoft Edge", "Edge"],
            Browser::Vivaldi => &["Vivaldi"],
            Browser::Opera => &["Opera"],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Browser::Firefox => "Firefox",
            Browser::Zen => "Zen",
            Browser::LibreWolf => "LibreWolf",
            Browser::Waterfox => "Waterfox",
            Browser::TorBrowser => "Tor Browser",
            Browser::Camoufox => "Camoufox",
            Browser::Chrome => "Chrome",
            Browser::Chromium => "Chromium",
            Browser::Brave => "Brave",
            Browser::Edge => "Edge",
            Browser::Vivaldi => "Vivaldi",
            Browser::Opera => "Opera",
        }
    }
}

impl fmt::Display for Browser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Remove the browser's branding from a window title.
///
/// A window manager sees `"Rust Docs — Zen Browser"` where history stores
/// `"Rust Docs"`, so the two only join after the suffix is gone. Titles
/// without a recognised suffix are returned unchanged.
pub fn strip_window_suffix(title: &str) -> &str {
    // Firefox-family brands use an em dash, Chromium-family a hyphen, but
    // either can appear depending on version and locale.
    for sep in [" — ", " - "] {
        for browser in Browser::ALL {
            for brand in browser.window_suffixes() {
                if let Some(head) = title.strip_suffix(&format!("{sep}{brand}")) {
                    return head;
                }
            }
        }
    }
    title
}

impl Family {
    /// Filename of the history database within a profile directory.
    pub fn history_filename(self) -> &'static str {
        match self {
            Family::Firefox => "places.sqlite",
            Family::Chromium => "History",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_release_channel_brands() {
        // Zen Twilight is the nightly channel and brands itself differently
        // from stable, which silently defeats a single-suffix match.
        assert_eq!(strip_window_suffix("Rust Docs — Zen Twilight"), "Rust Docs");
        assert_eq!(
            strip_window_suffix("Rust Docs — Firefox Nightly"),
            "Rust Docs"
        );
        assert_eq!(
            strip_window_suffix("Rust Docs — Firefox Developer Edition"),
            "Rust Docs"
        );
    }

    #[test]
    fn channel_brands_do_not_shadow_each_other() {
        for title in [
            "Rust Docs — Zen Browser",
            "Rust Docs — Zen Twilight",
            "Rust Docs — Zen",
        ] {
            assert_eq!(strip_window_suffix(title), "Rust Docs", "for {title:?}");
        }
    }

    #[test]
    fn strips_em_dash_and_hyphen_brands() {
        assert_eq!(strip_window_suffix("Rust Docs — Zen Browser"), "Rust Docs");
        assert_eq!(
            strip_window_suffix("Rust Docs - Google Chrome"),
            "Rust Docs"
        );
        assert_eq!(
            strip_window_suffix("Rust Docs — Mozilla Firefox"),
            "Rust Docs"
        );
    }

    #[test]
    fn leaves_unbranded_titles_alone() {
        assert_eq!(strip_window_suffix("Rust Docs"), "Rust Docs");
        // A title that merely mentions a browser is not a suffix.
        assert_eq!(
            strip_window_suffix("Zen Browser is fast"),
            "Zen Browser is fast"
        );
    }

    #[test]
    fn strips_only_the_trailing_brand() {
        assert_eq!(
            strip_window_suffix("Chromium vs Firefox — Zen Browser"),
            "Chromium vs Firefox"
        );
    }
}
