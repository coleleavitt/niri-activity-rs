use std::path::{Path, PathBuf};

use crate::browser::{Browser, Family};
use crate::error::{Error, Result};
use crate::model::Profile;

/// Find every profile of every known browser under `$HOME`.
///
/// Profiles are located by walking each browser's candidate roots and keeping
/// directories that actually contain a history database, so a browser that is
/// installed but never launched yields nothing.
pub fn discover() -> Result<Vec<Profile>> {
    let home = dirs::home_dir().ok_or(Error::NoHomeDir)?;
    Ok(discover_in(&home))
}

/// Same as [`discover`], rooted at an arbitrary directory instead of `$HOME`.
pub fn discover_in(home: &Path) -> Vec<Profile> {
    let mut found: Vec<Profile> = Browser::ALL
        .iter()
        .flat_map(|&browser| {
            browser
                .profile_roots()
                .iter()
                .map(move |rel| (browser, home.join(rel)))
        })
        .filter(|(_, root)| root.is_dir())
        .flat_map(|(browser, root)| profiles_under(browser, &root))
        .collect();

    // The same profile can surface through more than one candidate root when
    // roots nest or symlink into each other.
    found.sort_by(|a, b| a.history_db.cmp(&b.history_db));
    found.dedup_by(|a, b| a.history_db == b.history_db);
    found
}

/// Discover profiles for a single browser.
pub fn discover_browser(browser: Browser) -> Result<Vec<Profile>> {
    let home = dirs::home_dir().ok_or(Error::NoHomeDir)?;
    Ok(discover_in(&home)
        .into_iter()
        .filter(|p| p.browser == browser)
        .collect())
}

/// Profile directory names listed in a Firefox-family `profiles.ini`.
///
/// `None` means there is no `profiles.ini` to consult — normal for Chromium
/// and Tor Browser — and callers should then not filter at all.
fn registered_names(root: &Path) -> Option<Vec<String>> {
    let ini = std::fs::read_to_string(root.join("profiles.ini")).ok()?;
    let names: Vec<String> = ini
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Path="))
        // `Path` is absolute when `IsRelative=0`, so match on the last
        // component rather than the whole string.
        .filter_map(|path| Path::new(path).file_name()?.to_str().map(str::to_owned))
        .collect();
    (!names.is_empty()).then_some(names)
}

fn profiles_under(browser: Browser, root: &Path) -> Vec<Profile> {
    let filename = browser.family().history_filename();
    let mut out = Vec::new();

    // Tor Browser points its root directly at the profile directory, and
    // Chromium keeps `Default` alongside `Profile 1`; checking the root itself
    // covers the former without special-casing it.
    push_if_profile(&mut out, browser, root, filename);

    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    // A hand-made backup copy is indistinguishable from a real profile on
    // disk, and would double-count every URL it shares with the live one.
    let registered = registered_names(root);
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(names) = &registered {
            let listed = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.iter().any(|r| r == n));
            if !listed {
                continue;
            }
        }
        push_if_profile(&mut out, browser, &path, filename);
    }
    out
}

fn push_if_profile(out: &mut Vec<Profile>, browser: Browser, dir: &Path, filename: &str) {
    let history_db = dir.join(filename);
    if !history_db.is_file() {
        return;
    }
    out.push(Profile {
        browser,
        name: dir.file_name().map_or_else(
            || browser.name().to_string(),
            |n| n.to_string_lossy().into(),
        ),
        path: dir.to_path_buf(),
        history_db,
    });
}

/// Resolve the history database path for a profile directory, if one exists.
pub fn history_db_for(dir: &Path, family: Family) -> Option<PathBuf> {
    let db = dir.join(family.history_filename());
    db.is_file().then_some(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        std::fs::write(path, b"").expect("write");
    }

    #[test]
    fn finds_firefox_and_chromium_profiles() {
        let home = tempfile::tempdir().expect("tempdir");
        let h = home.path();
        touch(&h.join(".mozilla/firefox/abc.default-release/places.sqlite"));
        touch(&h.join(".zen/xyz.Default (release)/places.sqlite"));
        touch(&h.join(".config/google-chrome/Default/History"));
        touch(&h.join(".config/google-chrome/Profile 1/History"));

        let profiles = discover_in(h);
        assert_eq!(profiles.len(), 4);

        let zen: Vec<_> = profiles
            .iter()
            .filter(|p| p.browser == Browser::Zen)
            .collect();
        assert_eq!(zen.len(), 1);
        assert_eq!(zen[0].name, "xyz.Default (release)");
        assert_eq!(zen[0].family(), Family::Firefox);

        let chrome: Vec<_> = profiles
            .iter()
            .filter(|p| p.browser == Browser::Chrome)
            .collect();
        assert_eq!(chrome.len(), 2);
    }

    #[test]
    fn ignores_directories_without_history_db() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".config/chromium/Default")).expect("mkdir");
        assert!(discover_in(home.path()).is_empty());
    }

    #[test]
    fn profiles_ini_excludes_unregistered_backup_copies() {
        let home = tempfile::tempdir().expect("tempdir");
        let h = home.path();
        touch(&h.join(".zen/hz8tcfb3.Default (release)/places.sqlite"));
        touch(&h.join(".zen/backup-pre-fix-20260719/places.sqlite"));
        std::fs::write(
            h.join(".zen/profiles.ini"),
            "[Profile0]\nPath=hz8tcfb3.Default (release)\nDefault=1\n",
        )
        .expect("write ini");

        let profiles = discover_in(h);
        assert_eq!(profiles.len(), 1, "backup copy must not count as a profile");
        assert_eq!(profiles[0].name, "hz8tcfb3.Default (release)");
    }

    #[test]
    fn absolute_ini_paths_resolve_by_final_component() {
        let home = tempfile::tempdir().expect("tempdir");
        let h = home.path();
        touch(&h.join(".mozilla/firefox/abc.default-release/places.sqlite"));
        touch(&h.join(".mozilla/firefox/stale-copy/places.sqlite"));
        std::fs::write(
            h.join(".mozilla/firefox/profiles.ini"),
            "[Profile0]\nPath=/somewhere/else/abc.default-release\nIsRelative=0\n",
        )
        .expect("write ini");

        let profiles = discover_in(h);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "abc.default-release");
    }

    #[test]
    fn missing_profiles_ini_disables_filtering() {
        let home = tempfile::tempdir().expect("tempdir");
        let h = home.path();
        touch(&h.join(".config/chromium/Default/History"));
        touch(&h.join(".config/chromium/Profile 1/History"));

        assert_eq!(discover_in(h).len(), 2);
    }

    #[test]
    fn finds_profile_at_root_itself() {
        let home = tempfile::tempdir().expect("tempdir");
        touch(&home.path().join(".camoufox/places.sqlite"));

        let profiles = discover_in(home.path());
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].browser, Browser::Camoufox);
    }
}
