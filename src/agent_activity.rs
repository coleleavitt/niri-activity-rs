use std::path::PathBuf;
use std::time::Instant;

use rusqlite::Connection;

use crate::config::AgentActivityConfig;

pub struct AgentMonitor {
    enabled: bool,
    poll_interval_ms: u64,
    activity_window_ms: i64,
    databases: Vec<PathBuf>,
    last_poll: Instant,
    cached_active: bool,
}

impl AgentMonitor {
    pub fn new(config: &AgentActivityConfig) -> Self {
        let databases = config
            .databases
            .iter()
            .filter_map(|path| expand_tilde(path))
            .filter(|p| p.exists())
            .collect();

        Self {
            enabled: config.enabled,
            poll_interval_ms: config.poll_interval_secs.saturating_mul(1000),
            activity_window_ms: i64::try_from(config.activity_window_secs)
                .unwrap_or(30)
                .saturating_mul(1000),
            databases,
            last_poll: Instant::now(),
            cached_active: false,
        }
    }

    pub fn is_agent_active(&mut self) -> bool {
        if !self.enabled || self.databases.is_empty() {
            return false;
        }

        let now = Instant::now();
        if now.duration_since(self.last_poll).as_millis() < u128::from(self.poll_interval_ms) {
            return self.cached_active;
        }

        self.last_poll = now;
        self.cached_active = self.check_databases();
        self.cached_active
    }

    fn check_databases(&self) -> bool {
        for db_path in &self.databases {
            if self.check_opencode_db(db_path) {
                return true;
            }
        }
        false
    }

    fn check_opencode_db(&self, path: &PathBuf) -> bool {
        let conn = match Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(c) => c,
            Err(_) => return false,
        };

        // OpenCode stores message timestamps as milliseconds since epoch
        // Check if any messages were created within the activity window
        let query =
            "SELECT COUNT(*) FROM message WHERE time_created > (strftime('%s', 'now') * 1000 - ?1)";

        let count: i64 = conn
            .query_row(query, [self.activity_window_ms], |row| row.get(0))
            .unwrap_or(0);

        count > 0
    }
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
