use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::config::AgentActivityConfig;

pub struct AgentMonitor {
    enabled: bool,
    poll_interval_ms: u64,
    activity_window_ms: i64,
    databases: Vec<PathBuf>,
    process_whitelist: HashSet<String>,
    process_recency_ms: u64,
    last_poll: Instant,
    cached_agent_active: bool,
    cached_process_active: bool,
}

impl AgentMonitor {
    pub fn new(config: &AgentActivityConfig) -> Self {
        let databases = config
            .databases
            .iter()
            .filter_map(|path| expand_tilde(path))
            .filter(|p| p.exists())
            .collect();

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
            databases,
            process_whitelist,
            process_recency_ms: config.process_recency_secs.saturating_mul(1000),
            last_poll: Instant::now()
                .checked_sub(poll_duration)
                .unwrap_or_else(Instant::now),
            cached_agent_active: false,
            cached_process_active: false,
        }
    }

    pub fn is_active(&mut self, idle_duration_ms: u64) -> bool {
        if !self.enabled {
            return false;
        }

        let now = Instant::now();
        if now.duration_since(self.last_poll).as_millis() < u128::from(self.poll_interval_ms) {
            return self.cached_agent_active
                || (self.cached_process_active && idle_duration_ms < self.process_recency_ms);
        }

        self.last_poll = now;
        self.cached_agent_active = self.check_databases();
        self.cached_process_active = self.check_processes();

        self.cached_agent_active
            || (self.cached_process_active && idle_duration_ms < self.process_recency_ms)
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

        let query =
            "SELECT COUNT(*) FROM message WHERE time_created > (strftime('%s', 'now') * 1000 - ?1)";

        let count: i64 = conn
            .query_row(query, [self.activity_window_ms], |row| row.get(0))
            .unwrap_or(0);

        count > 0
    }

    fn check_processes(&self) -> bool {
        if self.process_whitelist.is_empty() {
            return false;
        }

        let Ok(entries) = fs::read_dir("/proc") else {
            return false;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if !name_str.chars().all(|c| c.is_ascii_digit()) {
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
