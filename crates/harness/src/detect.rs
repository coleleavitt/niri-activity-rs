use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
/// A matching visitor stops the walk. Entry and depth budgets guarantee a
/// stale or adversarial history tree cannot stall the watcher indefinitely.
/// Truncation is distinct from a complete scan that found no match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkOutcome {
    Found,
    Complete,
    Truncated,
}

fn walk_log_files(root: &Path, visit: impl FnMut(&Path) -> bool) -> WalkOutcome {
    walk_log_files_with_budget(root, MAX_LOG_TREE_ENTRIES, visit)
}

fn walk_log_files_with_budget(
    root: &Path,
    mut remaining: usize,
    mut visit: impl FnMut(&Path) -> bool,
) -> WalkOutcome {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(current) else {
            continue;
        };
        for entry in entries.flatten() {
            if remaining == 0 {
                return WalkOutcome::Truncated;
            }
            remaining -= 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file() {
                if visit(&entry.path()) {
                    return WalkOutcome::Found;
                }
            } else if file_type.is_dir() && depth < MAX_LOG_TREE_DEPTH {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    WalkOutcome::Complete
}

/// Whether any regular file beneath a directory was written recently.
fn dir_written_within(dir: &Path, window: Duration) -> bool {
    // An incomplete bounded scan cannot prove inactivity. Treat truncation as
    // active so a large or adversarial tree cannot hide a working agent.
    window != Duration::ZERO
        && walk_log_files(dir, |path| written_within(path, window)) != WalkOutcome::Complete
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

// Prime rotates its shared log, but one append may carry it beyond the nominal
// cap. This bound covers the full current generation without touching archives.
// Prime's configured rotation size is small, but a single append may carry a
// generation beyond the nominal cap. Read a bounded tail from both the current
// and immediately previous generation so an open span survives rotation.
const PRIME_TRACE_TAIL_BYTES: u64 = 32 * 1024 * 1024;

fn bounded_tail_lines(path: &Path) -> Vec<String> {
    bounded_tail_lines_with_limit(path, PRIME_TRACE_TAIL_BYTES)
}

fn bounded_tail_lines_with_limit(path: &Path, max_bytes: u64) -> Vec<String> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(max_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    // Read no farther than the size observed above. The daemon may keep
    // appending after `metadata()`, so `read_to_end` on the bare file can chase
    // a moving EOF and defeat the tail bound.
    let read_len = len - start;
    let mut bytes = Vec::new();
    if file.take(read_len).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        // The byte boundary may land in the middle of a JSON record.
        lines.next();
    }
    lines.map(str::to_owned).collect()
}

fn prime_timestamp(value: &serde_json::Value) -> Option<(i64, u32)> {
    let timestamp = value.get("ts")?.as_str()?;
    let seconds = crate::history::parse_rfc3339(timestamp)?;
    let Some(fraction) = timestamp
        .get(19..)
        .and_then(|suffix| suffix.strip_prefix('.'))
    else {
        return Some((seconds, 0));
    };
    let digits = fraction.bytes().take_while(u8::is_ascii_digit);
    let mut nanos = 0_u32;
    let mut count = 0_u32;
    for digit in digits.take(9) {
        nanos = nanos
            .checked_mul(10)?
            .checked_add(u32::from(digit - b'0'))?;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    Some((seconds, nanos.checked_mul(10_u32.pow(9 - count))?))
}

fn prime_prompt_record(line: &str) -> Option<(serde_json::Value, i64, u32)> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("component")?.as_str()? != "trace"
        || value.get("name")?.as_str()? != "agent.prompt"
        || !matches!(value.get("msg")?.as_str()?, "span_start" | "span_end")
        || value.get("pid")?.as_u64().is_none()
        || value.get("traceId")?.as_str()?.is_empty()
        || value.get("spanId")?.as_str()?.is_empty()
    {
        return None;
    }
    let (seconds, nanos) = prime_timestamp(&value)?;
    Some((value, seconds, nanos))
}

/// Whether the current/previous trace generations contain an open
/// `agent.prompt` span owned by the same live process that emitted it.
fn prime_trace_has_open_prompt_with(
    current: &Path,
    pid_is_live_since: impl Fn(u64, i64, u32) -> bool,
) -> bool {
    let mut previous = current.as_os_str().to_owned();
    previous.push(".old");
    let mut lines = bounded_tail_lines(Path::new(&previous));
    lines.extend(bounded_tail_lines(current));

    let mut decided = HashSet::new();
    for line in lines.into_iter().rev() {
        let Some((value, start_secs, start_nanos)) = prime_prompt_record(&line) else {
            continue;
        };
        let Some(pid) = value.get("pid").and_then(|value| value.as_u64()) else {
            continue;
        };
        let Some(trace_id) = value.get("traceId").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(span_id) = value.get("spanId").and_then(|value| value.as_str()) else {
            continue;
        };
        let key = (pid, trace_id.to_owned(), span_id.to_owned());
        if !decided.insert(key) {
            continue;
        }
        if value.get("msg").and_then(|value| value.as_str()) != Some("span_start") {
            continue;
        }
        if pid_is_live_since(pid, start_secs, start_nanos) {
            return true;
        }
    }
    false
}

fn prime_trace_has_open_prompt(path: &Path) -> bool {
    prime_trace_has_open_prompt_with(path, |pid, start_secs, start_nanos| {
        crate::process::pid_matches_harness_since(
            pid,
            crate::Harness::PrimeAgent,
            start_secs,
            start_nanos,
        )
    })
}

/// Whether a signal shows activity within `window`.
pub fn signal_active(signal: Signal, window: Duration) -> bool {
    resolve(signal.path()).into_iter().any(|path| match signal {
        Signal::Database(_) => database_written_within(&path, window),
        Signal::LogFile(_) => written_within(&path, window),
        Signal::LogDir(_) => dir_written_within(&path, window),
        Signal::PrimeTrace(_) => window != Duration::ZERO && prime_trace_has_open_prompt(&path),
    })
}

fn prime_trace_last_activity(current: &Path) -> Option<SystemTime> {
    let mut previous = current.as_os_str().to_owned();
    previous.push(".old");
    bounded_tail_lines(Path::new(&previous))
        .into_iter()
        .chain(bounded_tail_lines(current))
        .filter_map(|line| {
            let (_, seconds, nanos) = prime_prompt_record(&line)?;
            let seconds = u64::try_from(seconds).ok()?;
            UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
        })
        .max()
}

/// Most recent semantic activity across a signal's paths.
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
            Signal::PrimeTrace(_) => {
                if let Some(stamp) = prime_trace_last_activity(&path) {
                    consider(stamp);
                }
            }
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

    fn prime_trace(pid: u32, trace: &str, span: &str, message: &str) -> String {
        format!(
            r#"{{"pid":{pid},"ts":"2026-01-01T00:00:00Z","component":"trace","name":"agent.prompt","traceId":"{trace}","spanId":"{span}","msg":"{message}"}}"#
        )
    }

    #[test]
    fn prime_trace_requires_an_unclosed_prompt_span() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(
            &log,
            format!(
                "{}\n{}\n",
                prime_trace(1, "old", "closed", "span_start"),
                prime_trace(1, "old", "closed", "span_end")
            ),
        )
        .expect("closed trace");
        assert!(!prime_trace_has_open_prompt_with(&log, |_, _, _| true));

        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .and_then(|mut file| {
                std::io::Write::write_all(
                    &mut file,
                    format!("{}\n", prime_trace(2, "live", "open", "span_start")).as_bytes(),
                )
            })
            .expect("open trace");
        assert!(prime_trace_has_open_prompt_with(&log, |_, _, _| true));
    }

    #[test]
    fn prime_trace_open_span_survives_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(
            dir.path().join("agent.jsonl.old"),
            format!("{}\n", prime_trace(2, "live", "open", "span_start")),
        )
        .expect("previous generation");
        fs::write(&log, "").expect("current generation");

        assert!(prime_trace_has_open_prompt_with(&log, |_, _, _| true));
    }

    #[test]
    fn prime_trace_rejects_a_reused_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(
            &log,
            format!("{}\n", prime_trace(2, "stale", "open", "span_start")),
        )
        .expect("stale trace");

        assert!(!prime_trace_has_open_prompt_with(&log, |_, _, _| false));
    }

    #[test]
    fn prime_tail_read_is_capped_at_the_observed_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(&log, "discarded\nfirst\nsecond\n").expect("trace");

        let lines = bounded_tail_lines_with_limit(&log, 13);

        assert_eq!(lines, ["second"]);
        assert!(lines.iter().map(String::len).sum::<usize>() <= 13);
    }

    #[test]
    fn prime_last_activity_ignores_newer_diagnostics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(
            &log,
            concat!(
                "{\"pid\":1,\"ts\":\"2026-01-01T00:00:00.123456789Z\",\"component\":\"trace\",\"name\":\"agent.prompt\",\"traceId\":\"a\",\"spanId\":\"b\",\"msg\":\"span_end\"}\n",
                "{\"ts\":\"2027-01-01T00:00:00Z\",\"component\":\"trace\",\"name\":\"kernel.cell\",\"msg\":\"span_end\"}\n",
            ),
        )
        .expect("trace");

        let activity = prime_trace_last_activity(&log).expect("semantic activity");
        assert_eq!(
            activity.duration_since(UNIX_EPOCH).expect("after epoch"),
            Duration::new(1_767_225_600, 123_456_789),
        );
    }

    #[test]
    fn prime_last_activity_requires_valid_prompt_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(
            &log,
            "{\"ts\":\"2026-01-01T00:00:00Z\",\"component\":\"trace\",\"name\":\"agent.prompt\"\n",
        )
        .expect("trace");

        assert_eq!(prime_trace_last_activity(&log), None);
    }

    #[test]
    fn prime_last_activity_rejects_missing_or_bogus_span_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(
            &log,
            concat!(
                "{\"pid\":1,\"ts\":\"2026-01-01T00:00:00Z\",\"component\":\"trace\",\"name\":\"agent.prompt\",\"traceId\":\"a\",\"spanId\":\"b\"}\n",
                "{\"pid\":1,\"ts\":\"2026-01-01T00:00:01Z\",\"component\":\"trace\",\"name\":\"agent.prompt\",\"traceId\":\"a\",\"spanId\":\"b\",\"msg\":\"diagnostic\"}\n",
            ),
        )
        .expect("trace");

        assert_eq!(prime_trace_last_activity(&log), None);
    }

    #[test]
    fn prime_last_activity_requires_complete_span_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(
            &log,
            concat!(
                "{\"ts\":\"2026-01-01T00:00:00Z\",\"component\":\"trace\",\"name\":\"agent.prompt\",\"traceId\":\"a\",\"spanId\":\"b\",\"msg\":\"span_start\"}\n",
                "{\"pid\":1,\"ts\":\"2026-01-01T00:00:01Z\",\"component\":\"trace\",\"name\":\"agent.prompt\",\"spanId\":\"b\",\"msg\":\"span_start\"}\n",
                "{\"pid\":1,\"ts\":\"2026-01-01T00:00:02Z\",\"component\":\"trace\",\"name\":\"agent.prompt\",\"traceId\":\"a\",\"msg\":\"span_end\"}\n",
            ),
        )
        .expect("trace");

        assert_eq!(prime_trace_last_activity(&log), None);
    }

    #[test]
    fn unrelated_prime_trace_writes_do_not_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("agent.jsonl");
        fs::write(
            &log,
            "{\"component\":\"trace\",\"name\":\"kernel.cell\",\"traceId\":\"a\",\"spanId\":\"b\",\"msg\":\"span_start\"}\n",
        )
        .expect("trace");
        assert!(!prime_trace_has_open_prompt_with(&log, |_, _, _| true));
    }

    #[test]
    fn truncated_scan_cannot_report_complete_inactivity() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(&dir.path().join("stale-one.jsonl"));
        touch(&dir.path().join("stale-two.jsonl"));

        let outcome = walk_log_files_with_budget(dir.path(), 1, |_| false);
        assert_eq!(outcome, WalkOutcome::Truncated);
        assert_ne!(outcome, WalkOutcome::Complete);
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
