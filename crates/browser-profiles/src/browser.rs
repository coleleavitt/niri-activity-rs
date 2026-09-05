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

    /// Linux desktop and Flatpak application IDs emitted for this browser.
    ///
    /// Keep this list exact and fail-closed: an unknown application must not
    /// gain access to browser-history-derived identity merely because its name
    /// resembles a browser.
    fn app_ids(self) -> &'static [&'static str] {
        match self {
            Browser::Firefox => &[
                "firefox",
                "firefox-bin",
                "firefox-esr",
                "firefox-nightly",
                "firefoxdeveloperedition",
                "org.mozilla.firefox",
                "org.mozilla.firefoxdeveloperedition",
                "org.mozilla.firefoxnightly",
            ],
            Browser::Zen => &[
                "zen",
                "zen-browser",
                "zen-alpha",
                "zen-twilight",
                "zen-unofficial",
                "app.zen_browser.zen",
                "app.zen_browser.zen-twilight",
            ],
            Browser::LibreWolf => &["librewolf", "io.gitlab.librewolf-community"],
            Browser::Waterfox => &["waterfox", "net.waterfox.waterfox"],
            Browser::TorBrowser => &["tor browser", "org.torproject.torbrowser-launcher"],
            Browser::Camoufox => &["camoufox"],
            Browser::Chrome => &[
                "google-chrome",
                "google-chrome-stable",
                "google-chrome-beta",
                "google-chrome-unstable",
                "com.google.chrome",
                "com.google.chromebeta",
                "com.google.chromedev",
            ],
            Browser::Chromium => &["chromium", "chromium-browser", "org.chromium.chromium"],
            Browser::Brave => &[
                "brave-browser",
                "brave-browser-beta",
                "brave-browser-dev",
                "brave-browser-nightly",
                "com.brave.browser",
                "com.brave.browser.beta",
                "com.brave.browser.dev",
                "com.brave.browser.nightly",
            ],
            Browser::Edge => &[
                "microsoft-edge",
                "microsoft-edge-stable",
                "microsoft-edge-beta",
                "microsoft-edge-dev",
                "com.microsoft.edge",
                "com.microsoft.edge.beta",
                "com.microsoft.edge.dev",
            ],
            Browser::Vivaldi => &["vivaldi-stable", "vivaldi-snapshot", "com.vivaldi.vivaldi"],
            Browser::Opera => &["opera", "opera-beta", "opera-developer", "com.opera.opera"],
        }
    }

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
                ".config/google-chrome-beta",
                ".config/google-chrome-unstable",
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

/// Return whether `app_id` is an exact known Linux application ID for a
/// supported browser.
///
/// Matching is ASCII-case-insensitive because compositors do not consistently
/// preserve desktop-file casing. No substring or prefix matching is used, so
/// unknown and non-browser applications fail closed.
pub fn is_browser_app_id(app_id: &str) -> bool {
    Browser::ALL.iter().any(|browser| {
        browser
            .app_ids()
            .iter()
            .any(|known| known.eq_ignore_ascii_case(app_id))
    })
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
    fn chrome_discovers_supported_linux_channel_roots() {
        let roots = Browser::Chrome.profile_roots();
        assert!(roots.contains(&".config/google-chrome"));
        assert!(roots.contains(&".config/google-chrome-beta"));
        assert!(roots.contains(&".config/google-chrome-unstable"));
    }

    #[test]
    fn recognizes_app_ids_for_every_supported_browser() {
        let representatives = [
            (Browser::Firefox, "org.mozilla.firefox"),
            (Browser::Zen, "app.zen_browser.zen-twilight"),
            (Browser::LibreWolf, "io.gitlab.librewolf-community"),
            (Browser::Waterfox, "net.waterfox.waterfox"),
            (Browser::TorBrowser, "org.torproject.torbrowser-launcher"),
            (Browser::Camoufox, "camoufox"),
            (Browser::Chrome, "com.google.Chrome"),
            (Browser::Chromium, "org.chromium.Chromium"),
            (Browser::Brave, "com.brave.Browser"),
            (Browser::Edge, "com.microsoft.Edge"),
            (Browser::Vivaldi, "com.vivaldi.Vivaldi"),
            (Browser::Opera, "com.opera.Opera"),
        ];

        assert_eq!(representatives.len(), Browser::ALL.len());
        for (browser, app_id) in representatives {
            assert!(
                browser
                    .app_ids()
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(app_id)),
                "{browser} must own its representative app ID"
            );
            assert!(is_browser_app_id(app_id), "{app_id:?} must be recognized");
        }
    }

    #[test]
    fn browser_app_id_matching_fails_closed() {
        for app_id in [
            "foot",
            "code",
            "firefox-helper",
            "my-google-chrome-wrapper",
            "org.mozilla.not-firefox",
            "",
        ] {
            assert!(
                !is_browser_app_id(app_id),
                "unexpected match for {app_id:?}"
            );
        }
    }

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
