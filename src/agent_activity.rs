use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::config::AgentActivityConfig;

pub struct AgentMonitor {
    enabled: bool,
    poll_interval_ms: u64,
    activity_window_ms: i64,
    /// Configured patterns, not resolved paths: the watcher outlives agent
    /// installs, so resolution happens per poll.
    database_patterns: Vec<String>,
    process_whitelist: HashSet<String>,
    process_recency_ms: u64,
    last_poll: Instant,
    cached_agent_active: bool,
    cached_process_active: bool,
}

impl AgentMonitor {
    pub fn new(config: &AgentActivityConfig) -> Self {
        let _linkscope_agent_new = linkscope::phase("agent.new");
        let database_patterns = config.databases.clone();

        let process_whitelist = config
            .process_whitelist
            .iter()
            .cloned()
            .collect::<HashSet<_>>();

        let poll_interval_ms = config.poll_interval_secs.saturating_mul(1000);
        let poll_duration = Duration::from_millis(poll_interval_ms);
        Self {
            enabled: config.enabled,
            poll_interval_ms,
            activity_window_ms: i64::try_from(config.activity_window_secs)
                .unwrap_or(30)
                .saturating_mul(1000),
            database_patterns,
            process_whitelist,
            process_recency_ms: config.process_recency_secs.saturating_mul(1000),
            last_poll: Instant::now()
                .checked_sub(poll_duration)
                .unwrap_or_else(Instant::now),
            cached_agent_active: false,
            cached_process_active: false,
        }
    }

    pub fn is_active(
        &mut self,
        idle_duration_ms: u64,
        focused_app_id: Option<&str>,
        focused_title: Option<&str>,
    ) -> bool {
        let _linkscope_agent_active = linkscope::phase("agent.is_active");
        linkscope::record_items("agent.is_active", 1);
        if !self.enabled {
            return false;
        }

        // A streaming-agent title only counts while the user was recently
        // present. Terminal titles can get stuck on a spinner frame, so a
        // stale title must not pin the session Active forever while the user
        // is idle/asleep (see docs/SLEEP_DETECTION.lean, bug B1). The final
        // Away decision is enforced in watcher::compute_activity_state, but we
        // gate here too so agent-title never marks activity past recency.
        if idle_duration_ms < self.process_recency_ms
            && focused_app_id.is_some_and(crate::terminal::is_terminal_app)
            && focused_title.is_some_and(jfc_streaming_title_active)
        {
            linkscope::record_items("agent.title_active", 1);
            return true;
        }

        let now = Instant::now();
        if now.duration_since(self.last_poll).as_millis() < u128::from(self.poll_interval_ms) {
            linkscope::record_items("agent.cache_hit", 1);
            return self.cached_agent_active
                || (self.cached_process_active && idle_duration_ms < self.process_recency_ms);
        }

        let _linkscope_agent_poll = linkscope::phase("agent.poll");
        linkscope::record_items("agent.poll", 1);
        self.last_poll = now;
        self.cached_agent_active = self.check_databases() || self.check_harnesses();
        self.cached_process_active = self.check_processes();

        self.cached_agent_active
            || (self.cached_process_active && idle_duration_ms < self.process_recency_ms)
    }

    /// Timestamps decide this, not token counts. Generating tokens always
    /// writes to a store, so a token query can only confirm what the mtime
    /// already said while costing a full scan of a multi-gigabyte table.
    fn check_harnesses(&self) -> bool {
        let _linkscope_agent_harness = linkscope::phase("agent.harness");
        let window =
            Duration::from_millis(u64::try_from(self.activity_window_ms).unwrap_or(30_000));
        let active = harness::any_active(window);
        if active {
            linkscope::record_items("agent.harness.files", 1);
        }
        active
    }

    /// Resolve the configured patterns against the filesystem.
    ///
    /// Resolving once at startup dropped every database that did not exist
    /// yet, so installing or first-running an agent left it undetectable for
    /// the life of the watcher process. Missing paths cost a failed stat in
    /// `database_recently_changed`, which is cheaper than the SQL scan it
    /// guards.
    fn resolve_databases(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        self.database_patterns
            .iter()
            .flat_map(|pattern| expand_database_paths(pattern))
            .filter(|path| seen.insert(path.clone()))
            .collect()
    }

    fn check_databases(&self) -> bool {
        let _linkscope_agent_dbs = linkscope::phase("agent.databases");
        let databases = self.resolve_databases();
        linkscope::record_items(
            "agent.databases",
            u64::try_from(databases.len()).unwrap_or(u64::MAX),
        );
        databases.iter().any(|path| self.check_opencode_db(path))
    }

    fn check_opencode_db(&self, path: &Path) -> bool {
        let _linkscope_agent_db = linkscope::phase("agent.db");
        linkscope::record_items("agent.db", 1);
        if !database_recently_changed(path, self.activity_window_ms) {
            linkscope::record_items("agent.db.stale_mtime", 1);
            return false;
        }

        let conn = match Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(c) => c,
            Err(_) => return false,
        };

        let now_ms = unix_now_ms();
        let lower_bound_ms = now_ms.saturating_sub(self.activity_window_ms);
        OPENCODE_ACTIVITY_COLUMNS.iter().any(|(table, column)| {
            has_recent_timestamp(&conn, table, column, lower_bound_ms, now_ms)
        })
    }

    fn check_processes(&self) -> bool {
        let _linkscope_agent_processes = linkscope::phase("agent.processes");
        if self.process_whitelist.is_empty() {
            return false;
        }

        let Ok(entries) = fs::read_dir("/proc") else {
            return false;
        };
        let Ok(current_uid) = fs::metadata("/proc/self").map(|metadata| metadata.uid()) else {
            return false;
        };

        for entry in entries.flatten() {
            linkscope::record_items("agent.proc.entries", 1);
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if !name_str.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if !entry
                .metadata()
                .is_ok_and(|metadata| metadata.uid() == current_uid)
            {
                continue;
            }

            let comm_path = entry.path().join("comm");
            if let Ok(comm) = fs::read_to_string(&comm_path) {
                let proc_name = comm.trim();
                if self.process_whitelist.contains(proc_name) {
                    return true;
                }
            }
        }

        false
    }

    pub fn process_whitelist_count(&self) -> usize {
        self.process_whitelist.len()
    }
}

const OPENCODE_ACTIVITY_COLUMNS: &[(&str, &str)] = &[
    // New assistant messages.
    ("message", "time_created"),
    // Streaming/tool updates on existing messages.
    ("message", "time_updated"),
    ("part", "time_created"),
    ("part", "time_updated"),
    // Session-level updates, including token/cost/title updates.
    ("session", "time_updated"),
    // Newer OpenCode versions also maintain session_message projections.
    ("session_message", "time_created"),
    ("session_message", "time_updated"),
];

/// Current wall clock in milliseconds, or 0 if the clock predates the epoch.
///
/// A broken clock yields an empty activity window rather than a wide one, so
/// unmeasurable time is never credited as agent work.
fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// Whether the table holds a row whose timestamp lies inside the activity
/// window.
///
/// The bounds are half-open on both sides and the column is cast, because
/// SQLite orders every numeric value before every text value: an un-cast
/// comparison made a malformed `'yesterday'` string, and any clock-skewed
/// future timestamp, look like activity that just happened. Both would credit
/// idle time as agent work, which is the one error this measurement must not
/// make.
fn has_recent_timestamp(
    conn: &Connection,
    table: &str,
    column: &str,
    lower_bound_ms: i64,
    now_ms: i64,
) -> bool {
    let _linkscope_agent_query = linkscope::phase("agent.db.query");
    linkscope::record_items("agent.db.query", 1);
    let query = format!(
        "SELECT EXISTS(SELECT 1 FROM {table} \
         WHERE CAST({column} AS INTEGER) > ?1 \
           AND CAST({column} AS INTEGER) <= ?2 LIMIT 1)"
    );
    conn.query_row(&query, [lower_bound_ms, now_ms], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
        != 0
}

fn database_recently_changed(path: &Path, activity_window_ms: i64) -> bool {
    let Ok(window_ms) = u64::try_from(activity_window_ms) else {
        return false;
    };
    let window = Duration::from_millis(window_ms);
    recently_modified(path, window)
        || recently_modified(&path_with_suffix(path, "-wal"), window)
        || recently_modified(&path_with_suffix(path, "-journal"), window)
}

fn recently_modified(path: &Path, window: Duration) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    match modified.elapsed() {
        Ok(age) => age <= window,
        Err(_) => true,
    }
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

fn expand_tilde(path: &str) -> Option<PathBuf> {
    if path.starts_with("~/") {
        std::env::var("HOME")
            .ok()
            .map(|home| PathBuf::from(home).join(&path[2..]))
    } else if path == "~" {
        std::env::var("HOME").ok().map(PathBuf::from)
    } else {
        Some(PathBuf::from(path))
    }
}

fn expand_database_paths(path: &str) -> Vec<PathBuf> {
    let Some(expanded) = expand_tilde(path) else {
        return Vec::new();
    };

    let Some(file_name) = expanded.file_name().and_then(|name| name.to_str()) else {
        return vec![expanded];
    };
    if !file_name.contains('*') {
        return vec![expanded];
    }

    let parent = expanded.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };

    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| matches_single_wildcard(name, file_name))
        })
        .collect()
}

fn matches_single_wildcard(name: &str, pattern: &str) -> bool {
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return name == pattern;
    };
    !suffix.contains('*')
        && name.len() >= prefix.len().saturating_add(suffix.len())
        && name.starts_with(prefix)
        && name.ends_with(suffix)
}

fn jfc_streaming_title_active(title: &str) -> bool {
    let Some(rest) = title.trim_start().strip_prefix("\u{25cf} ") else {
        return false;
    };
    let mut fields = rest.split(" \u{00b7} ");
    let Some(first) = fields.next() else {
        return false;
    };
    let Some(model) = fields.next() else {
        return false;
    };
    let Some(third) = fields.next() else {
        return false;
    };
    if fields.next().is_some() || first.is_empty() || model.is_empty() || third.is_empty() {
        return false;
    }

    // Current JFC: `● <session> · <model> · jfc`.
    // Older installed versions used `● jfc · <model> · <project>`.
    // Match every field in either grammar. A suffix/prefix-only check lets an
    // unrelated custom terminal title masquerade as JFC activity.
    // The dot remains latch-prone evidence, so callers additionally apply the
    // short process-recency window rather than trusting it until Away.
    third == "jfc" || first == "jfc"
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn now_ms() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_millis(),
        )
        .expect("current time fits in i64")
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let unique = format!(
            "niri-activity-rs-{name}-{}-{}.db",
            std::process::id(),
            now_ms()
        );
        std::env::temp_dir().join(unique)
    }

    fn temp_dir_path(name: &str) -> PathBuf {
        let unique = format!(
            "niri-activity-rs-{name}-{}-{}",
            std::process::id(),
            now_ms()
        );
        std::env::temp_dir().join(unique)
    }

    fn monitor_with_window(activity_window_ms: i64) -> AgentMonitor {
        monitor_with_databases(activity_window_ms, Vec::new())
    }

    fn monitor_with_databases(
        activity_window_ms: i64,
        database_patterns: Vec<String>,
    ) -> AgentMonitor {
        AgentMonitor {
            enabled: true,
            poll_interval_ms: 5_000,
            activity_window_ms,
            database_patterns,
            process_whitelist: HashSet::new(),
            process_recency_ms: 300_000,
            last_poll: Instant::now(),
            cached_agent_active: false,
            cached_process_active: false,
        }
    }

    #[test]
    fn harness_check_runs_when_no_database_is_configured() {
        // With no configured database the monitor used to see nothing, which
        // left eleven of twelve installed agents invisible.
        let monitor = monitor_with_window(30_000);
        assert_eq!(monitor.resolve_databases(), [] as [PathBuf; 0]);
        assert_eq!(
            monitor.check_harnesses(),
            harness::any_active(Duration::from_secs(30)),
            "check_harnesses must delegate to the harness crate, not short-circuit"
        );
    }

    #[test]
    fn a_zero_window_reports_no_harness_activity() {
        assert!(
            !monitor_with_window(0).check_harnesses(),
            "nothing can have been written within a zero-length window"
        );
    }

    #[test]
    fn harness_activity_reaches_is_active_without_a_database() {
        let window = Duration::from_secs(86_400);
        let window_ms = i64::try_from(window.as_millis()).expect("window fits in i64");
        let mut monitor = monitor_with_window(window_ms);
        monitor.last_poll = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("instant predates process start");

        // Comparing against the crate rather than asserting `true` keeps this
        // honest on a machine with no agents installed, where both are false.
        assert_eq!(
            monitor.is_active(0, None, None),
            harness::any_active(window),
            "with no database or process configured, is_active must be decided \
             solely by harness detection"
        );
    }

    #[test]
    fn opencode_activity_detects_recent_part_update() {
        let path = temp_db_path("part-update");
        {
            let conn = Connection::open(&path).expect("create temp opencode db");
            conn.execute("CREATE TABLE part (time_updated INTEGER NOT NULL)", [])
                .expect("create part table");
            conn.execute("INSERT INTO part (time_updated) VALUES (?1)", [now_ms()])
                .expect("insert recent part update");
        }

        let monitor = monitor_with_window(60_000);
        assert!(monitor.check_opencode_db(&path));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn wildcard_prefix_and_suffix_must_not_overlap() {
        assert!(matches_single_wildcard("opencode.db", "opencode*.db"));
        assert!(matches_single_wildcard("opencode-2.db", "opencode*.db"));
        assert!(!matches_single_wildcard("abc", "ab*bc"));
        assert!(!matches_single_wildcard("anything", "a*b*c"));
    }

    #[test]
    fn jfc_streaming_title_counts_as_active_when_user_recently_present() {
        let mut monitor = monitor_with_window(60_000);

        // process_recency_ms is 300_000 (5 min); a recent user gets the title
        // credit.
        assert!(monitor.is_active(
            60 * 1000,
            Some("foot"),
            Some("\u{25cf} my session \u{00b7} claude-sonnet \u{00b7} jfc")
        ));
    }

    #[test]
    fn jfc_streaming_title_is_ignored_once_user_is_long_idle() {
        let mut monitor = monitor_with_window(60_000);

        // A stale spinner title must not pin Active after the user has been
        // idle past the recency window (docs/SLEEP_DETECTION.lean bug B1).
        assert!(!monitor.is_active(
            10 * 60 * 1000,
            Some("foot"),
            Some("\u{25cf} my session \u{00b7} claude-sonnet \u{00b7} jfc")
        ));
    }

    #[test]
    fn jfc_streaming_title_supports_complete_current_and_legacy_grammars() {
        assert!(jfc_streaming_title_active(
            "\u{25cf} session \u{00b7} claude-sonnet \u{00b7} jfc"
        ));
        assert!(jfc_streaming_title_active(
            "\u{25cf} jfc \u{00b7} claude-sonnet \u{00b7} project"
        ));
    }

    #[test]
    fn jfc_streaming_title_rejects_partial_and_lookalike_grammars() {
        for title in [
            "\u{25cf} session \u{00b7} claude-sonnet \u{00b7} jfc extra",
            "\u{25cf} session \u{00b7} extra \u{00b7} claude-sonnet \u{00b7} jfc",
            "\u{25cf} session \u{00b7}  \u{00b7} jfc",
            "\u{25cf} jfc \u{00b7} claude-sonnet",
            "session \u{00b7} claude-sonnet \u{00b7} jfc",
        ] {
            assert!(!jfc_streaming_title_active(title), "accepted {title:?}");
        }
    }

    #[test]
    fn jfc_streaming_title_requires_a_supported_terminal_app() {
        let mut monitor = monitor_with_window(60_000);

        assert!(!monitor.is_active(
            60 * 1000,
            Some("zen"),
            Some("\u{25cf} my session \u{00b7} claude-sonnet \u{00b7} jfc")
        ));
    }

    #[test]
    fn jfc_idle_title_does_not_count_as_active() {
        let mut monitor = monitor_with_window(60_000);

        assert!(!monitor.is_active(
            10 * 60 * 1000,
            Some("foot"),
            Some("jfc \u{00b7} claude-sonnet \u{00b7} jfc")
        ));
    }

    #[test]
    fn opencode_activity_detects_recent_session_message_update() {
        let path = temp_db_path("session-message-update");
        {
            let conn = Connection::open(&path).expect("create temp opencode db");
            conn.execute(
                "CREATE TABLE session_message (time_updated INTEGER NOT NULL)",
                [],
            )
            .expect("create session_message table");
            conn.execute(
                "INSERT INTO session_message (time_updated) VALUES (?1)",
                [now_ms()],
            )
            .expect("insert recent session_message update");
        }

        let monitor = monitor_with_window(60_000);
        assert!(monitor.check_opencode_db(&path));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn opencode_activity_ignores_stale_message_create() {
        let path = temp_db_path("stale-message");
        {
            let conn = Connection::open(&path).expect("create temp opencode db");
            conn.execute("CREATE TABLE message (time_created INTEGER NOT NULL)", [])
                .expect("create message table");
            conn.execute(
                "INSERT INTO message (time_created) VALUES (?1)",
                [now_ms() - 120_000],
            )
            .expect("insert stale message");
        }

        let monitor = monitor_with_window(30_000);
        assert!(!monitor.check_opencode_db(&path));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn opencode_activity_skips_stale_database_file_before_sql_scan() {
        let path = temp_db_path("stale-db-file");
        {
            let conn = Connection::open(&path).expect("create temp opencode db");
            conn.execute("CREATE TABLE message (time_created INTEGER NOT NULL)", [])
                .expect("create message table");
            conn.execute("INSERT INTO message (time_created) VALUES (?1)", [now_ms()])
                .expect("insert recent message");
        }
        let stale = filetime::FileTime::from_unix_time((now_ms() / 1000) - 120, 0);
        filetime::set_file_mtime(&path, stale).expect("set stale db mtime");

        let monitor = monitor_with_window(30_000);
        assert!(!monitor.check_opencode_db(&path));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_database_created_after_startup_is_still_detected() {
        let dir = temp_dir_path("late-database");
        fs::create_dir(&dir).expect("create temp directory");
        let pattern = format!("{}/opencode*.db", dir.display());
        let monitor = monitor_with_databases(60_000, vec![pattern]);

        assert!(
            !monitor.check_databases(),
            "an empty directory has no agent activity"
        );

        let path = dir.join("opencode.db");
        {
            let conn = Connection::open(&path).expect("create temp opencode db");
            conn.execute("CREATE TABLE part (time_updated INTEGER NOT NULL)", [])
                .expect("create part table");
            conn.execute("INSERT INTO part (time_updated) VALUES (?1)", [now_ms()])
                .expect("insert recent part update");
        }

        assert!(
            monitor.check_databases(),
            "a database created after startup must still be polled"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_and_future_timestamps_are_not_recent_activity() {
        let path = temp_db_path("timestamp-bounds");
        let now = now_ms();
        {
            let conn = Connection::open(&path).expect("create temp opencode db");
            conn.execute("CREATE TABLE part (time_updated)", [])
                .expect("create part table");
            for value in ["'yesterday'", "'not-a-timestamp'"] {
                conn.execute(&format!("INSERT INTO part VALUES ({value})"), [])
                    .expect("insert malformed timestamp");
            }
            conn.execute(
                "INSERT INTO part VALUES (?1)",
                [now.saturating_add(3_600_000)],
            )
            .expect("insert future timestamp");
        }

        let conn = Connection::open(&path).expect("open temp opencode db");
        assert!(
            !has_recent_timestamp(&conn, "part", "time_updated", now - 30_000, now),
            "text and future timestamps must not count as activity"
        );

        conn.execute("INSERT INTO part VALUES (?1)", [now - 1_000])
            .expect("insert recent timestamp");
        assert!(
            has_recent_timestamp(&conn, "part", "time_updated", now - 30_000, now),
            "a genuine recent timestamp must still count"
        );

        drop(conn);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn expands_opencode_channel_database_pattern() {
        let dir = temp_dir_path("opencode-pattern");
        fs::create_dir(&dir).expect("create temp directory");

        let main = dir.join("opencode.db");
        let local = dir.join("opencode-local.db");
        let backup = dir.join("opencode.db.backup");
        let wal = dir.join("opencode.db-wal");

        fs::write(&main, []).expect("write main db");
        fs::write(&local, []).expect("write local db");
        fs::write(&backup, []).expect("write backup");
        fs::write(&wal, []).expect("write wal");

        let mut paths = expand_database_paths(&format!("{}/opencode*.db", dir.display()));
        paths.sort();

        let mut expected = vec![local, main];
        expected.sort();
        assert_eq!(paths, expected);

        let _ = fs::remove_dir_all(dir);
    }
}
