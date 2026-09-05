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

/// Hard bounds for recursive log discovery. Agent stores are user-controlled
/// and may grow forever, while this scan runs on the watcher's polling path.
const MAX_LOG_TREE_ENTRIES: usize = 10_000;
const MAX_LOG_TREE_DEPTH: usize = 16;

/// Visit regular files under `root` without following symlinks.
///
/// Returning `true` stops the walk. Entry and depth budgets guarantee a stale
/// or adversarial history tree cannot stall the watcher indefinitely.
fn walk_log_files(root: &Path, mut visit: impl FnMut(&Path) -> bool) -> bool {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut remaining = MAX_LOG_TREE_ENTRIES;
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(current) else {
            continue;
        };
        for entry in entries.flatten() {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file() {
                if visit(&entry.path()) {
                    return true;
                }
            } else if file_type.is_dir() && depth < MAX_LOG_TREE_DEPTH {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    false
}

/// Whether any regular file beneath a directory was written recently.
fn dir_written_within(dir: &Path, window: Duration) -> bool {
    walk_log_files(dir, |path| written_within(path, window))
}

fn consider_file(path: &Path, consider: &mut impl FnMut(SystemTime)) {
    if let Ok(stamp) = fs::metadata(path).and_then(|metadata| metadata.modified()) {
        consider(stamp);
    }
}

fn consider_database(path: &Path, consider: &mut impl FnMut(SystemTime)) {
    consider_file(path, consider);
    for suffix in ["-wal", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        consider_file(Path::new(&sidecar), consider);
    }
}

fn consider_dir_files(dir: &Path, consider: &mut impl FnMut(SystemTime)) {
    walk_log_files(dir, |path| {
        consider_file(path, consider);
        false
    });
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
            Signal::Database(_) => consider_database(&path, &mut consider),
            Signal::LogFile(_) => consider_file(&path, &mut consider),
            Signal::LogDir(_) => consider_dir_files(&path, &mut consider),
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
    fn detects_nested_file_but_not_stale_nested_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("2026/01/01/rollout.jsonl");
        touch(&log);
        assert!(dir_written_within(dir.path(), Duration::from_millis(500)));
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
    fn database_last_write_includes_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("agent.db");
        touch(&db);
        sleep(Duration::from_millis(1100));
        let wal = dir.path().join("agent.db-wal");
        touch(&wal);
        let signal = Signal::Database(Box::leak(
            db.to_string_lossy().into_owned().into_boxed_str(),
        ));
        let latest = signal_last_write(signal).expect("last write");
        let wal_time = fs::metadata(wal)
            .and_then(|m| m.modified())
            .expect("wal time");
        assert!(latest >= wal_time);
    }

    #[test]
    fn recursive_scan_respects_the_depth_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut nested = dir.path().to_path_buf();
        for depth in 0..=MAX_LOG_TREE_DEPTH {
            nested.push(format!("level-{depth}"));
        }
        touch(&nested.join("too-deep.jsonl"));

        assert!(!dir_written_within(dir.path(), Duration::MAX));
    }

    #[test]
    fn recursive_scan_finds_files_at_the_supported_depth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut nested = dir.path().to_path_buf();
        for depth in 0..MAX_LOG_TREE_DEPTH {
            nested.push(format!("level-{depth}"));
        }
        touch(&nested.join("rollout.jsonl"));

        assert!(dir_written_within(dir.path(), Duration::MAX));
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
