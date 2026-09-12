use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::detect;
use crate::harness::Harness;

/// Seconds per minute bucket.
///
/// Agent logs record events, not durations, so activity is reconstructed by
/// bucketing timestamps. A minute is coarse enough that a single streamed
/// response lands in one bucket rather than fragmenting, and fine enough to
/// intersect a focus session meaningfully.
pub const BUCKET_SECS: i64 = 60;

/// Minute buckets during which an agent left evidence of working.
#[derive(Debug, Clone)]
pub struct BusyMinutes {
    minutes: BTreeSet<i64>,
    complete: bool,
}

impl Default for BusyMinutes {
    fn default() -> Self {
        Self {
            minutes: BTreeSet::new(),
            complete: true,
        }
    }
}

impl BusyMinutes {
    pub fn is_empty(&self) -> bool {
        self.minutes.is_empty()
    }

    /// Whether every configured historical source fit within its scan budget.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    fn mark_incomplete(&mut self) {
        self.complete = false;
    }

    pub fn len(&self) -> usize {
        self.minutes.len()
    }

    pub fn contains(&self, unix_secs: i64) -> bool {
        self.minutes.contains(&bucket(unix_secs))
    }

    /// Milliseconds of `[start_ms, end_ms)` that fall in a busy bucket.
    ///
    /// Buckets are whole minutes, so a session is credited the overlap of each
    /// busy minute with its own span rather than the full minute.
    pub fn overlap_ms(&self, start_ms: i64, end_ms: i64) -> i64 {
        if end_ms <= start_ms {
            return 0;
        }
        let bucket_ms = BUCKET_SECS * 1000;
        let first = bucket(start_ms / 1000);
        let last = bucket((end_ms - 1) / 1000);

        let mut total = 0i64;
        for minute in self.minutes.range(first..=last) {
            let bucket_start = minute * 1000;
            let overlap = (end_ms.min(bucket_start + bucket_ms)) - (start_ms.max(bucket_start));
            if overlap > 0 {
                total = total.saturating_add(overlap);
            }
        }
        total
    }

    fn insert(&mut self, unix_secs: i64) {
        self.minutes.insert(bucket(unix_secs));
    }
}

fn bucket(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(BUCKET_SECS) * BUCKET_SECS
}

/// Parse an RFC 3339 timestamp to a Unix second.
pub(crate) fn parse_rfc3339(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let num = |from: usize, to: usize| s.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&min)
        || !(0..=59).contains(&sec)
    {
        return None;
    }
    let leap = year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day < 1 || day > month_days[usize::try_from(month - 1).ok()?] {
        return None;
    }

    let mut zone = 19;
    if bytes.get(zone) == Some(&b'.') {
        zone += 1;
        let fraction_start = zone;
        while bytes.get(zone).is_some_and(u8::is_ascii_digit) {
            zone += 1;
        }
        if zone == fraction_start {
            return None;
        }
    }
    let offset = match bytes.get(zone) {
        Some(b'Z') if zone + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-'))
            if zone + 6 == bytes.len() && bytes.get(zone + 3) == Some(&b':') =>
        {
            let hours = s.get(zone + 1..zone + 3)?.parse::<i64>().ok()?;
            let minutes = s.get(zone + 4..zone + 6)?.parse::<i64>().ok()?;
            if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
                return None;
            }
            let seconds = hours * 3_600 + minutes * 60;
            if *sign == b'+' { seconds } else { -seconds }
        }
        _ => return None,
    };

    // Days from the civil epoch, per Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + hour * 3_600 + min * 60 + sec - offset)
}

/// Extract the first RFC 3339 timestamp from a `"timestamp":"..."` field.
fn timestamp_field(line: &str) -> Option<i64> {
    for key in ["\"timestamp\":\"", "\"ts\":\""] {
        if let Some(start) = line.find(key) {
            let rest = &line[start + key.len()..];
            if let Some(end) = rest.find('"') {
                if let Some(secs) = parse_rfc3339(&rest[..end]) {
                    return Some(secs);
                }
            }
        }
    }
    None
}

/// Files under `dir` modified within the window, newest first.
fn recent_files(dir: &Path, since_secs: i64) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
                continue;
            }
            // A log untouched since the window opened cannot contain events
            // inside it, so skipping saves opening tens of thousands of files.
            let fresh = meta.modified().is_ok_and(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .is_ok_and(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX) >= since_secs)
            });
            if fresh {
                out.push(entry.path());
            }
        }
    }
    out
}

fn scan_log(path: &Path, since: i64, until: i64, busy: &mut BusyMinutes) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Some(secs) = timestamp_field(&line) {
            if secs >= since && secs < until {
                busy.insert(secs);
            }
        }
    }
}

/// Prime transcripts contain user and agent records in the same append-only
/// file. Only completed assistant/tool-result messages prove agent work; a
/// user prompt proves human input, not that a model produced anything.
fn prime_message_timestamp(line: &str) -> Option<i64> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "message" {
        return None;
    }
    match value.get("message")?.get("role")?.as_str()? {
        "assistant" | "toolResult" => {}
        _ => return None,
    }
    value.get("timestamp")?.as_str().and_then(parse_rfc3339)
}

fn scan_prime_log(path: &Path, since: i64, until: i64, busy: &mut BusyMinutes) {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            busy.mark_incomplete();
            return;
        }
    };
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {
                busy.mark_incomplete();
                return;
            }
        };
        if let Some(secs) = prime_message_timestamp(&line) {
            if secs >= since && secs < until {
                busy.insert(secs);
            }
        }
    }
}

const MAX_PRIME_HISTORY_ENTRIES: usize = 50_000;
const MAX_PRIME_HISTORY_FILES: usize = 10_000;
const MAX_PRIME_HISTORY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PRIME_HISTORY_DEPTH: usize = 32;

fn metadata_modified_is_fresh(
    modified: std::io::Result<std::time::SystemTime>,
    since: i64,
) -> Option<bool> {
    let modified = match modified {
        Ok(modified) => modified,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .is_ok_and(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX) >= since),
    )
}

fn bounded_recent_prime_files(
    roots: impl IntoIterator<Item = PathBuf>,
    since: i64,
) -> Option<Vec<PathBuf>> {
    bounded_recent_prime_files_with_limits(
        roots,
        since,
        MAX_PRIME_HISTORY_ENTRIES,
        MAX_PRIME_HISTORY_FILES,
        MAX_PRIME_HISTORY_BYTES,
    )
}

fn bounded_recent_prime_files_with_limits(
    roots: impl IntoIterator<Item = PathBuf>,
    since: i64,
    max_entries: usize,
    max_files: usize,
    max_bytes: u64,
) -> Option<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut bytes = 0u64;
    let mut entries_seen = 0usize;
    let mut stack: Vec<_> = roots.into_iter().map(|root| (root, 0usize)).collect();
    while let Some((current, depth)) = stack.pop() {
        let entries = match fs::read_dir(current) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > max_entries {
                return None;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            if metadata.is_dir() {
                if depth >= MAX_PRIME_HISTORY_DEPTH {
                    return None;
                }
                stack.push((entry.path(), depth + 1));
                continue;
            }
            let path = entry.path();
            if path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            let fresh = metadata_modified_is_fresh(metadata.modified(), since)?;
            if !fresh {
                continue;
            }
            files.push(path);
            bytes = bytes.saturating_add(metadata.len());
            if files.len() > max_files || bytes > max_bytes {
                return None;
            }
        }
    }
    Some(files)
}

fn scan_prime_roots(
    roots: impl IntoIterator<Item = PathBuf>,
    since: i64,
    until: i64,
    busy: &mut BusyMinutes,
) {
    let Some(files) = bounded_recent_prime_files(roots, since) else {
        busy.mark_incomplete();
        return;
    };
    for path in files {
        scan_prime_log(&path, since, until, busy);
    }
}

fn prime_session_roots() -> Vec<PathBuf> {
    let sessions = std::env::var_os("PRIME_AGENT_SESSION_DIR")
        .or_else(|| std::env::var_os("PRIME_AGENT_CODING_AGENT_SESSION_DIR"))
        .map(PathBuf::from)
        .or_else(|| detect::expand_tilde("~/.prime/agent/sessions"));
    let Some(sessions) = sessions else {
        return Vec::new();
    };
    let artifacts = sessions
        .parent()
        .map(|parent| parent.join("session-artifacts"));
    std::iter::once(sessions).chain(artifacts).collect()
}

fn scan_prime(since: i64, until: i64, busy: &mut BusyMinutes) {
    scan_prime_roots(prime_session_roots(), since, until, busy);
}

const OPENCODE_STEP_TIMES: &str = "\
    SELECT time_updated / 1000 FROM part \
    WHERE time_updated BETWEEN ?1 AND ?2 \
      AND json_extract(data,'$.type') = 'step-finish'";

fn scan_opencode(since: i64, until: i64, busy: &mut BusyMinutes) {
    scan_opencode_paths(
        detect::resolve("~/.local/share/opencode/opencode*.db"),
        since,
        until,
        busy,
    );
}

fn scan_opencode_paths(
    paths: impl IntoIterator<Item = PathBuf>,
    since: i64,
    until: i64,
    busy: &mut BusyMinutes,
) {
    for path in paths {
        if !path.exists() {
            continue;
        }
        let Ok(conn) = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let _ = conn.busy_timeout(std::time::Duration::ZERO);
        let Ok(mut stmt) = conn.prepare(OPENCODE_STEP_TIMES) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([since * 1000, until * 1000], |row| row.get::<_, i64>(0))
        else {
            continue;
        };
        for secs in rows.flatten() {
            busy.insert(secs);
        }
    }
}

/// Reconstruct when agents were working between two Unix timestamps.
///
/// Reads historical logs rather than current file timestamps, so it can
/// backfill a period that has already passed.
pub fn busy_minutes(since: i64, until: i64) -> BusyMinutes {
    let mut busy = BusyMinutes::default();
    if until <= since {
        return busy;
    }

    scan_opencode(since, until, &mut busy);
    scan_prime(since, until, &mut busy);

    for harness in Harness::ALL {
        if matches!(harness, Harness::OpenCode | Harness::PrimeAgent) {
            continue;
        }
        for signal in harness.signals() {
            for path in detect::resolve(signal.path()) {
                if path.is_dir() {
                    for file in recent_files(&path, since) {
                        scan_log(&file, since, until, &mut busy);
                    }
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    scan_log(&path, since, until, &mut busy);
                }
            }
        }
    }
    busy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_timestamp_shapes_agents_emit() {
        // 2026-01-01T00:00:00Z
        assert_eq!(parse_rfc3339("2026-01-01T00:00:00Z"), Some(1_767_225_600));
        assert_eq!(
            parse_rfc3339("2026-01-01T00:00:00.126Z"),
            Some(1_767_225_600)
        );
        assert_eq!(
            parse_rfc3339("2026-01-01T00:00:01+00:00"),
            Some(1_767_225_601)
        );
        assert_eq!(
            parse_rfc3339("2026-01-01T01:00:00+01:00"),
            Some(1_767_225_600)
        );
        assert_eq!(
            parse_rfc3339("2025-12-31T23:30:00-01:00"),
            Some(1_767_227_400)
        );
        assert_eq!(parse_rfc3339("2026-01-01T00:00:00"), None);
        assert_eq!(parse_rfc3339("2026-01-01T00:00:00+24:00"), None);
        assert_eq!(parse_rfc3339("2026-01-01T00:00:00--1:00"), None);
        assert_eq!(parse_rfc3339("2026-01-01T00:00:00+-1:00"), None);
        assert_eq!(parse_rfc3339("2026-01-01T-1:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-01-01T00:-1:00Z"), None);
        assert_eq!(parse_rfc3339("2026-01-01T00:00:-1Z"), None);
        assert_eq!(parse_rfc3339("2026-02-30T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("not a date"), None);
        assert_eq!(parse_rfc3339("2026-01-01"), None);
    }

    #[test]
    fn extracts_timestamps_from_log_lines() {
        let line = r#"{"timestamp":"2026-01-01T00:00:00.126Z","type":"event_msg"}"#;
        assert_eq!(timestamp_field(line), Some(1_767_225_600));
        assert_eq!(
            timestamp_field(r#"{"ts":"2026-01-01T00:00:00Z"}"#),
            Some(1_767_225_600)
        );
        assert_eq!(timestamp_field("{}"), None);
    }

    #[test]
    fn buckets_collapse_to_the_containing_minute() {
        let mut busy = BusyMinutes::default();
        busy.insert(1_767_225_600);
        busy.insert(1_767_225_659);
        assert_eq!(busy.len(), 1, "same minute must not create two buckets");
        assert!(busy.contains(1_767_225_630));
        assert!(!busy.contains(1_767_225_660));
    }

    #[test]
    fn overlap_credits_only_the_intersecting_part_of_a_minute() {
        let mut busy = BusyMinutes::default();
        busy.insert(1_767_225_600);
        let minute_start_ms = 1_767_225_600 * 1000;

        // A session covering the last 20s of a busy minute earns 20s, not 60.
        assert_eq!(
            busy.overlap_ms(minute_start_ms + 40_000, minute_start_ms + 60_000),
            20_000
        );
        // A session spanning the whole minute earns the whole minute.
        assert_eq!(
            busy.overlap_ms(minute_start_ms, minute_start_ms + 60_000),
            60_000
        );
        // An adjacent quiet minute earns nothing.
        assert_eq!(
            busy.overlap_ms(minute_start_ms + 60_000, minute_start_ms + 120_000),
            0
        );
    }

    #[test]
    fn overlap_sums_across_several_busy_minutes() {
        let mut busy = BusyMinutes::default();
        busy.insert(1_767_225_600);
        busy.insert(1_767_225_720);
        let start = 1_767_225_600 * 1000;
        // Spans three minutes, of which the first and third are busy.
        assert_eq!(busy.overlap_ms(start, start + 180_000), 120_000);
    }

    #[test]
    fn prime_history_counts_only_agent_generated_messages() {
        let assistant = r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant"}}"#;
        let tool = r#"{"type":"message","timestamp":"2026-01-01T00:01:00Z","message":{"role":"toolResult"}}"#;
        let user =
            r#"{"type":"message","timestamp":"2026-01-01T00:02:00Z","message":{"role":"user"}}"#;
        let custom = r#"{"type":"custom_message","timestamp":"2026-01-01T00:03:00Z","message":{"role":"assistant"}}"#;

        assert_eq!(prime_message_timestamp(assistant), Some(1_767_225_600));
        assert_eq!(prime_message_timestamp(tool), Some(1_767_225_660));
        assert_eq!(prime_message_timestamp(user), None);
        assert_eq!(prime_message_timestamp(custom), None);
        assert_eq!(prime_message_timestamp("not json"), None);
    }

    #[test]
    fn prime_history_scans_top_level_and_nested_session_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let top = dir.path().join("sessions/top.jsonl");
        let child = dir
            .path()
            .join("session-artifacts/root/child/session.jsonl");
        fs::create_dir_all(top.parent().expect("top parent")).expect("mkdir");
        fs::create_dir_all(child.parent().expect("child parent")).expect("mkdir");
        fs::write(
            &top,
            concat!(
                r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant"}}"#,
                "\n",
                r#"{"type":"message","timestamp":"2026-01-01T00:01:00Z","message":{"role":"user"}}"#,
                "\n",
            ),
        )
        .expect("top transcript");
        fs::write(
            &child,
            concat!(
                r#"{"type":"message","timestamp":"2026-01-01T00:02:00Z","message":{"role":"toolResult"}}"#,
                "\n",
            ),
        )
        .expect("child transcript");

        let mut busy = BusyMinutes::default();
        scan_prime_roots(
            [
                dir.path().join("sessions"),
                dir.path().join("session-artifacts"),
            ],
            1_767_225_500,
            1_767_225_800,
            &mut busy,
        );
        assert_eq!(busy.len(), 2);
        assert!(busy.contains(1_767_225_600));
        assert!(!busy.contains(1_767_225_660));
        assert!(busy.contains(1_767_225_720));
    }

    #[test]
    fn prime_history_budget_overflow_is_explicit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("sessions/one.jsonl");
        fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
        fs::write(&transcript, "{}\n").expect("transcript");

        assert!(
            bounded_recent_prime_files_with_limits(
                [dir.path().join("sessions")],
                0,
                0,
                usize::MAX,
                u64::MAX,
            )
            .is_none(),
            "overflow must not look like a complete empty scan"
        );
    }

    #[test]
    fn prime_history_modified_time_errors_fail_closed_except_not_found() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(metadata_modified_is_fresh(Err(missing), 0), Some(false));

        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(metadata_modified_is_fresh(Err(denied), 0), None);
    }

    #[test]
    fn prime_history_missing_optional_roots_remain_complete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut busy = BusyMinutes::default();

        scan_prime_roots(
            [dir.path().join("missing-sessions")],
            1_767_225_500,
            1_767_225_800,
            &mut busy,
        );

        assert!(busy.is_complete());
        assert!(busy.is_empty());
    }

    #[test]
    fn prime_history_non_not_found_io_errors_are_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_directory = dir.path().join("sessions");
        fs::write(&not_a_directory, "not a directory").expect("fixture");
        let mut busy = BusyMinutes::default();

        scan_prime_roots([not_a_directory], 1_767_225_500, 1_767_225_800, &mut busy);

        assert!(!busy.is_complete());
    }

    #[test]
    fn prime_history_transcript_read_errors_are_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("sessions/broken.jsonl");
        fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
        fs::write(&transcript, [0xff, b'\n']).expect("fixture");
        let mut busy = BusyMinutes::default();

        scan_prime_roots([dir.path().join("sessions")], 0, i64::MAX, &mut busy);

        assert!(!busy.is_complete());
    }

    #[test]
    fn prime_history_directory_beyond_depth_limit_is_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut deepest = dir.path().join("sessions");
        for level in 0..=MAX_PRIME_HISTORY_DEPTH {
            deepest = deepest.join(format!("child-{level}"));
        }
        fs::create_dir_all(&deepest).expect("deep tree");
        let mut busy = BusyMinutes::default();

        scan_prime_roots(
            [dir.path().join("sessions")],
            1_767_225_500,
            1_767_225_800,
            &mut busy,
        );

        assert!(!busy.is_complete());
    }

    #[test]
    fn prime_history_reports_complete_for_a_bounded_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("sessions/one.jsonl");
        fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
        fs::write(
            &transcript,
            concat!(
                r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant"}}"#,
                "\n",
            ),
        )
        .expect("transcript");
        let mut busy = BusyMinutes::default();
        scan_prime_roots(
            [dir.path().join("sessions")],
            1_767_225_500,
            1_767_225_800,
            &mut busy,
        );
        assert!(busy.is_complete());
        assert_eq!(busy.len(), 1);
    }

    fn create_opencode(path: &Path, seconds: i64) {
        let conn = rusqlite::Connection::open(path).expect("open");
        conn.execute("CREATE TABLE part (time_updated INTEGER, data TEXT)", [])
            .expect("schema");
        conn.execute(
            r#"INSERT INTO part VALUES (?1, '{"type":"step-finish"}')"#,
            [seconds * 1000],
        )
        .expect("insert");
    }

    #[test]
    fn opencode_history_aggregates_every_matching_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("one.db");
        let second = dir.path().join("two.db");
        create_opencode(&first, 1_767_225_600);
        create_opencode(&second, 1_767_225_720);
        let mut busy = BusyMinutes::default();
        scan_opencode_paths([first, second], 1_767_225_500, 1_767_225_800, &mut busy);
        assert_eq!(busy.len(), 2);
        assert!(busy.contains(1_767_225_600));
        assert!(busy.contains(1_767_225_720));
    }

    #[test]
    fn degenerate_ranges_yield_nothing() {
        let mut busy = BusyMinutes::default();
        busy.insert(1_767_225_600);
        assert_eq!(busy.overlap_ms(100, 100), 0);
        assert_eq!(busy.overlap_ms(200, 100), 0);
        assert!(busy_minutes(100, 100).is_empty());
    }
}
