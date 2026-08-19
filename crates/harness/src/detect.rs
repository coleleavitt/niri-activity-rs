use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::harness::Signal;

/// Expand a leading `~` against `$HOME`.
pub fn expand_tilde(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest)),
        None if path == "~" => std::env::var_os("HOME").map(PathBuf::from),
        None => Some(PathBuf::from(path)),
    }
}

fn matches_wildcard(name: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        None => name == pattern,
        Some((prefix, suffix)) => {
            !suffix.contains('*')
                && name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

/// Resolve a path that may contain a single `*` in its final component.
pub fn resolve(path: &str) -> Vec<PathBuf> {
    let Some(expanded) = expand_tilde(path) else {
        return Vec::new();
    };
    let Some(name) = expanded.file_name().and_then(|n| n.to_str()) else {
        return vec![expanded];
    };
    if !name.contains('*') {
        return vec![expanded];
    }

    let parent = expanded.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| matches_wildcard(n, name))
        })
        .collect()
}

fn written_within(path: &Path, window: Duration) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|m| m.modified()) else {
        return false;
    };
    is_recent(modified, window)
}

/// Whether `stamp` is within `window` of now.
///
/// A file stamped in the future — a clock adjustment, or a copy from another
/// machine — makes `elapsed` fail rather than return zero. Counting that as
/// recent errs toward crediting real work instead of discarding it.
fn is_recent(stamp: SystemTime, window: Duration) -> bool {
    match stamp.elapsed() {
        Ok(age) => age <= window,
        Err(_) => true,
    }
}

/// Whether a SQLite database or either of its sidecars changed recently.
///
/// A write in WAL mode lands in `-wal` and may leave the main file untouched
/// for minutes, so checking only the database misses live activity.
fn database_written_within(path: &Path, window: Duration) -> bool {
    if written_within(path, window) {
        return true;
    }
    for suffix in ["-wal", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        if written_within(Path::new(&sidecar), window) {
            return true;
        }
    }
    false
}

/// Whether any file directly inside a directory was written recently.
///
/// Appending to an existing file does not update its directory's mtime — only
/// creating or removing an entry does — so a single stat on the directory
/// would miss an agent streaming into a session log it opened minutes ago.
/// Scanning stops at the first recent file, which is the common case when an
/// agent is genuinely working.
fn dir_written_within(dir: &Path, window: Duration) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let recent = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| is_recent(t, window));
        if recent {
            return true;
        }
    }
    false
}

/// Whether a signal shows activity within `window`.
pub fn signal_active(signal: Signal, window: Duration) -> bool {
    resolve(signal.path()).into_iter().any(|path| match signal {
        Signal::Database(_) => database_written_within(&path, window),
        Signal::LogFile(_) => written_within(&path, window),
        Signal::LogDir(_) => dir_written_within(&path, window),
    })
}

/// Most recent write across a signal's paths.
pub fn signal_last_write(signal: Signal) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut consider = |t: SystemTime| {
        if newest.is_none_or(|current| t > current) {
            newest = Some(t);
        }
    };

    for path in resolve(signal.path()) {
        match signal {
            Signal::LogDir(_) => {
                if let Ok(entries) = fs::read_dir(&path) {
                    for entry in entries.flatten() {
                        if let Ok(t) = entry.metadata().and_then(|m| m.modified()) {
                            consider(t);
                        }
                    }
                }
            }
            _ => {
                if let Ok(t) = fs::metadata(&path).and_then(|m| m.modified()) {
                    consider(t);
                }
            }
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;

    use super::*;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, "x").expect("write");
    }

    #[test]
    fn wildcard_matches_only_the_final_component() {
        assert!(matches_wildcard("opencode.db", "opencode*.db"));
        assert!(matches_wildcard("opencode-2.db", "opencode*.db"));
        assert!(!matches_wildcard("other.db", "opencode*.db"));
        // Without a length guard, prefix and suffix could overlap and match a
        // name shorter than both.
        assert!(!matches_wildcard("ab", "ab*ab"));
    }

    #[test]
    fn detects_an_append_that_leaves_directory_mtime_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("session.jsonl");
        touch(&log);

        // Let the directory's own mtime age past the window while the file
        // inside it stays fresh; this is what an agent streaming into an
        // already-open session looks like.
        sleep(Duration::from_millis(1100));
        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"more\n"))
            .expect("append");

        assert!(
            dir_written_within(dir.path(), Duration::from_millis(500)),
            "an append must count even though it does not touch the directory"
        );
    }

    #[test]
    fn ignores_writes_older_than_the_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(&dir.path().join("old.jsonl"));
        sleep(Duration::from_millis(1100));
        assert!(!dir_written_within(dir.path(), Duration::from_millis(500)));
    }

    #[test]
    fn database_activity_is_seen_through_the_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("agent.db");
        touch(&db);
        sleep(Duration::from_millis(1100));
        // Only the WAL is fresh, which is what a mid-transaction write looks
        // like in WAL mode.
        touch(&dir.path().join("agent.db-wal"));

        assert!(database_written_within(&db, Duration::from_millis(500)));
    }

    #[test]
    fn missing_paths_are_inactive_not_errors() {
        assert!(!written_within(Path::new("/nonexistent/x"), Duration::MAX));
        assert!(!dir_written_within(
            Path::new("/nonexistent"),
            Duration::MAX
        ));
    }
}
