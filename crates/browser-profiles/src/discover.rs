use std::collections::HashSet;
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

    // Sort first so that, when aliases point at the same database, the
    // lexicographically first spelling wins deterministically.
    found.sort_by(|a, b| a.history_db.cmp(&b.history_db));
    let mut canonical = HashSet::new();
    found.retain(|profile| {
        let key = std::fs::canonicalize(&profile.history_db)
            .unwrap_or_else(|_| profile.history_db.clone());
        canonical.insert(key)
    });
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

/// Profile directories listed in a Firefox-family `profiles.ini`.
///
/// `None` means there is no readable `profiles.ini` to consult — normal for
/// Chromium and Tor Browser — and callers should use normal directory
/// discovery. Relative paths are resolved from the directory containing the
/// INI; absolute paths may point outside it.
fn registered_paths(root: &Path) -> Option<Vec<PathBuf>> {
    let ini = std::fs::read_to_string(root.join("profiles.ini")).ok()?;
    let mut paths = Vec::new();
    let mut in_profile = false;
    let mut path: Option<&str> = None;
    let mut is_relative: Option<bool> = None;

    let finish_section = |paths: &mut Vec<PathBuf>, path: Option<&str>, is_relative| {
        if let (Some(path), Some(is_relative)) = (path, is_relative) {
            paths.push(if is_relative {
                root.join(path)
            } else {
                PathBuf::from(path)
            });
        }
    };

    for raw_line in ini.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            if in_profile {
                finish_section(&mut paths, path, is_relative);
            }
            in_profile = line[1..line.len() - 1].starts_with("Profile");
            path = None;
            is_relative = None;
        } else if in_profile {
            if let Some(value) = line.strip_prefix("Path=") {
                path = Some(value);
            } else if let Some(value) = line.strip_prefix("IsRelative=") {
                is_relative = match value {
                    "0" => Some(false),
                    "1" => Some(true),
                    _ => None,
                };
            }
        }
    }
    if in_profile {
        finish_section(&mut paths, path, is_relative);
    }
    (!paths.is_empty()).then_some(paths)
}

fn profiles_under(browser: Browser, root: &Path) -> Vec<Profile> {
    let filename = browser.family().history_filename();
    let mut out = Vec::new();

    // Tor Browser points its root directly at the profile directory.
    push_if_profile(&mut out, browser, root, filename);

    if let Some(registered) = registered_paths(root) {
        for path in registered {
            push_if_profile(&mut out, browser, &path, filename);
        }
        return out;
    }

    // Chromium and Firefox-family roots without an INI use conventional
    // immediate child directories.
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            push_if_profile(&mut out, browser, &path, filename);
        }
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
            "[Profile0]\nPath=hz8tcfb3.Default (release)\nIsRelative=1\nDefault=1\n",
        )
        .expect("write ini");

        let profiles = discover_in(h);
        assert_eq!(profiles.len(), 1, "backup copy must not count as a profile");
        assert_eq!(profiles[0].name, "hz8tcfb3.Default (release)");
    }

    #[test]
    fn profiles_ini_resolves_nested_relative_path_exactly() {
        let home = tempfile::tempdir().expect("tempdir");
        let h = home.path();
        touch(&h.join(".mozilla/firefox/nested/abc.default/places.sqlite"));
        touch(&h.join(".mozilla/firefox/abc.default/places.sqlite"));
        std::fs::write(
            h.join(".mozilla/firefox/profiles.ini"),
            "[Install123]\nPath=abc.default\nIsRelative=1\n[Profile0]\nPath=nested/abc.default\nIsRelative=1\n",
        )
        .expect("write ini");

        let profiles = discover_in(h);
        assert_eq!(profiles.len(), 1);
        assert_eq!(
            profiles[0].path,
            h.join(".mozilla/firefox/nested/abc.default")
        );
    }

    #[test]
    fn profiles_ini_resolves_external_absolute_path() {
        let home = tempfile::tempdir().expect("tempdir");
        let external = tempfile::tempdir().expect("external tempdir");
        let h = home.path();
        let profile = external.path().join("abc.default");
        touch(&profile.join("places.sqlite"));
        std::fs::create_dir_all(h.join(".mozilla/firefox")).expect("mkdir");
        std::fs::write(
            h.join(".mozilla/firefox/profiles.ini"),
            format!("[Profile0]\nPath={}\nIsRelative=0\n", profile.display()),
        )
        .expect("write ini");

        let profiles = discover_in(h);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].path, profile);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_are_deduplicated_by_canonical_history_path() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("tempdir");
        let h = home.path();
        let root = h.join(".config/chromium");
        touch(&root.join("Profile 1/History"));
        symlink(root.join("Profile 1"), root.join("Profile Alias")).expect("symlink");

        let profiles = discover_in(h);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "Profile 1");
    }

    #[test]
    fn empty_or_incomplete_profiles_ini_falls_back_to_directory_discovery() {
        for contents in [
            "",
            "[General]\nStartWithLastProfile=1\n",
            "[Profile0]\nPath=nested\n",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().join(".mozilla/firefox");
            let profile = root.join("fallback.default");
            std::fs::create_dir_all(&profile).expect("profile");
            std::fs::write(profile.join("places.sqlite"), b"").expect("history");
            std::fs::write(root.join("profiles.ini"), contents).expect("ini");

            let found = profiles_under(Browser::Firefox, &root);
            assert_eq!(found.len(), 1, "contents={contents:?}");
            assert_eq!(found[0].path, profile);
        }
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
