use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use backoff::ExponentialBackoff;
use inotify::{Inotify, WatchMask};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use chrono::{Local, Timelike, Utc};
use clap::{Parser, Subcommand};
use niri_ipc::{Event, Request, Response, Window, socket::Socket};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("niri ipc: {0}")]
    NiriIpc(#[from] std::io::Error),
    #[error("niri error: {0}")]
    NiriError(String),
    #[error("database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("config: {0}")]
    Config(#[from] toml::de::Error),
    #[error("unexpected response from niri")]
    UnexpectedResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Productive,
    Unproductive,
    Neutral,
}

impl Category {
    fn as_str(&self) -> &'static str {
        match self {
            Category::Productive => "productive",
            Category::Unproductive => "unproductive",
            Category::Neutral => "neutral",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TitleRule {
    pub pattern: String,
    pub category: Category,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_idle_threshold")]
    pub idle_threshold_secs: u64,
    #[serde(default)]
    pub categories: HashMap<String, Category>,
    #[serde(default)]
    pub title_rules: Vec<TitleRule>,
}

fn default_idle_threshold() -> u64 {
    60
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_threshold_secs: default_idle_threshold(),
            categories: HashMap::new(),
            title_rules: Vec::new(),
        }
    }
}

#[derive(Parser)]
#[command(name = "niri-activity-rs")]
#[command(about = "Track window focus on Niri compositor")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the watcher daemon
    Watch {
        /// Suppress per-event output (still logs untracked apps)
        #[arg(short, long)]
        quiet: bool,
    },
    /// Show today's activity
    Today,
    /// Show productivity metrics
    Metrics {
        /// Number of days to show
        #[arg(short, long, default_value = "1")]
        days: u32,
    },
    /// Show activity timeline in 15-min buckets
    Timeline {
        /// Number of days back (0 = today)
        #[arg(short, long, default_value = "0")]
        days: u32,
        /// Bucket size in minutes
        #[arg(short, long, default_value = "15")]
        bucket: u32,
    },
    /// Generate a full activity report
    Report {
        /// Number of days to cover
        #[arg(short, long, default_value = "1")]
        days: u32,
    },
    /// Initialize config file with examples
    Init,
}

fn get_data_dir() -> PathBuf {
    let dirs = directories::ProjectDirs::from("", "", "niri-activity-rs")
        .expect("Could not determine data directory");
    let data_dir = dirs.data_dir().to_path_buf();
    fs::create_dir_all(&data_dir).expect("Could not create data directory");
    data_dir
}

fn get_config_path() -> PathBuf {
    let dirs = directories::ProjectDirs::from("", "", "niri-activity-rs")
        .expect("Could not determine config directory");
    let config_dir = dirs.config_dir();
    fs::create_dir_all(config_dir).expect("Could not create config directory");
    config_dir.join("config.toml")
}

fn load_config() -> Result<Config, Error> {
    let config_path = get_config_path();
    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        Ok(toml::from_str(&content)?)
    } else {
        Ok(Config::default())
    }
}

fn init_db(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;",
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY,
            timestamp TEXT NOT NULL,
            app_id TEXT,
            title TEXT,
            category TEXT NOT NULL,
            active_ms INTEGER NOT NULL,
            idle_ms INTEGER NOT NULL,
            keystrokes INTEGER NOT NULL DEFAULT 0,
            mouse_clicks INTEGER NOT NULL DEFAULT 0,
            scroll_events INTEGER NOT NULL DEFAULT 0,
            mouse_distance INTEGER NOT NULL DEFAULT 0
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_timestamp ON events(timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_category ON events(category)",
        [],
    )?;
    Ok(())
}

fn run_migrations(conn: &Connection, config: &Config) -> Result<(), Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS migrations (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            applied_at TEXT NOT NULL
        )",
        [],
    )?;

    let applied: Vec<String> = {
        let mut stmt = conn.prepare("SELECT name FROM migrations")?;
        stmt.query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect()
    };

    if !applied.contains(&"001_fix_historical_categories".to_string()) {
        let mut updated = 0i64;
        for (app_id, category) in &config.categories {
            let count = conn.execute(
                "UPDATE events SET category = ?1 WHERE app_id = ?2 AND category != ?1",
                params![category.as_str(), app_id],
            )?;
            updated += count as i64;
        }
        conn.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["001_fix_historical_categories", Utc::now().to_rfc3339()],
        )?;
        if updated > 0 {
            eprintln!("Migration 001: fixed {} events with stale categories", updated);
        }
    }

    if !applied.contains(&"002_apply_title_rules".to_string()) && !config.title_rules.is_empty() {
        let mut stmt = conn.prepare("SELECT id, app_id, title FROM events")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut updated = 0i64;
        for (id, app_id, title) in &rows {
            let correct = classify(app_id, title, config);
            let count = conn.execute(
                "UPDATE events SET category = ?1 WHERE id = ?2 AND category != ?1",
                params![correct.as_str(), id],
            )?;
            updated += count as i64;
        }

        conn.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["002_apply_title_rules", Utc::now().to_rfc3339()],
        )?;
        if updated > 0 {
            eprintln!("Migration 002: reclassified {} events with title rules", updated);
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct WindowInfo {
    app_id: String,
    title: String,
}

impl From<&Window> for WindowInfo {
    fn from(w: &Window) -> Self {
        Self {
            app_id: w.app_id.clone().unwrap_or_else(|| "unknown".into()),
            title: w.title.clone().unwrap_or_default(),
        }
    }
}

struct InputStats {
    last_activity_ms: Arc<AtomicU64>,
    keystrokes: Arc<AtomicU64>,
    mouse_clicks: Arc<AtomicU64>,
    scroll_events: Arc<AtomicU64>,
    mouse_distance: Arc<AtomicU64>,
}

impl InputStats {
    fn reset_counts(&self) -> (u64, u64, u64, u64) {
        let keys = self.keystrokes.swap(0, Ordering::Relaxed);
        let clicks = self.mouse_clicks.swap(0, Ordering::Relaxed);
        let scrolls = self.scroll_events.swap(0, Ordering::Relaxed);
        let distance = self.mouse_distance.swap(0, Ordering::Relaxed);
        (keys, clicks, scrolls, distance)
    }
}

fn enumerate_input_devices() -> Vec<evdev::Device> {
    evdev::enumerate()
        .filter_map(|(path, device)| {
            let supported = device.supported_events();
            let has_keys = supported.contains(evdev::EventType::KEY);
            let has_relative = supported.contains(evdev::EventType::RELATIVE);
            let has_absolute = supported.contains(evdev::EventType::ABSOLUTE);

            if has_keys || has_relative || has_absolute {
                match evdev::Device::open(&path) {
                    Ok(dev) => {
                        let _ = dev.set_nonblocking(true);
                        Some(dev)
                    }
                    Err(_) => None,
                }
            } else {
                None
            }
        })
        .collect()
}

fn start_idle_monitor(start: Instant) -> InputStats {
    let stats = InputStats {
        last_activity_ms: Arc::new(AtomicU64::new(0)),
        keystrokes: Arc::new(AtomicU64::new(0)),
        mouse_clicks: Arc::new(AtomicU64::new(0)),
        scroll_events: Arc::new(AtomicU64::new(0)),
        mouse_distance: Arc::new(AtomicU64::new(0)),
    };

    let last_activity = Arc::clone(&stats.last_activity_ms);
    let keystrokes = Arc::clone(&stats.keystrokes);
    let mouse_clicks = Arc::clone(&stats.mouse_clicks);
    let scroll_events = Arc::clone(&stats.scroll_events);
    let mouse_distance = Arc::clone(&stats.mouse_distance);

    let devices_changed = Arc::new(AtomicBool::new(false));
    let devices_changed_clone = Arc::clone(&devices_changed);

    thread::spawn(move || {
        let mut inotify = match Inotify::init() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("Warning: Failed to init inotify: {}. Device hotplug disabled.", e);
                return;
            }
        };

        if let Err(e) = inotify.watches().add("/dev/input", WatchMask::CREATE | WatchMask::DELETE) {
            eprintln!("Warning: Failed to watch /dev/input: {}. Device hotplug disabled.", e);
            return;
        }

        let mut buffer = [0; 1024];
        loop {
            match inotify.read_events_blocking(&mut buffer) {
                Ok(events) => {
                    for event in events {
                        if let Some(name) = event.name {
                            let name_str = name.to_string_lossy();
                            if name_str.starts_with("event") {
                                eprintln!("Input device changed: {:?}", name);
                                devices_changed_clone.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("inotify error: {}", e);
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    });

    thread::spawn(move || {
        let mut devices = enumerate_input_devices();

        if devices.is_empty() {
            eprintln!("Warning: No input devices found. Idle detection disabled.");
            eprintln!("  (May need to add user to 'input' group: sudo usermod -aG input $USER)");
            return;
        }

        eprintln!("Monitoring {} input device(s) for activity", devices.len());

        let mut last_reenumerate = Instant::now();
        let mut last_mouse_event = Instant::now();
        let mut last_keyboard_event = Instant::now();

        const REENUMERATE_INTERVAL: Duration = Duration::from_secs(60);
        const STALE_MOUSE_THRESHOLD: Duration = Duration::from_secs(30);
        // Don't re-enumerate more than once per 10s even if stale detection fires
        const REENUMERATE_COOLDOWN: Duration = Duration::from_secs(10);

        loop {
            let loop_now = Instant::now();

            // Inotify-triggered re-enumerate (physical hotplug)
            if devices_changed.swap(false, Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
                let new_devices = enumerate_input_devices();
                eprintln!(
                    "Re-enumerated (hotplug): {} -> {} devices",
                    devices.len(),
                    new_devices.len()
                );
                devices = new_devices;
                last_reenumerate = loop_now;
                last_mouse_event = loop_now;
            }

            // Periodic re-enumerate (catches USB autosuspend/wake)
            if loop_now.duration_since(last_reenumerate) >= REENUMERATE_INTERVAL {
                let new_devices = enumerate_input_devices();
                if new_devices.len() != devices.len() {
                    eprintln!(
                        "Re-enumerated (periodic): {} -> {} devices",
                        devices.len(),
                        new_devices.len()
                    );
                }
                devices = new_devices;
                last_reenumerate = loop_now;
            }

            // Stale mouse detection: keyboard is active but mouse hasn't
            // produced events in 30s — likely USB suspend without removal
            if loop_now.duration_since(last_keyboard_event) < Duration::from_secs(5)
                && loop_now.duration_since(last_mouse_event) >= STALE_MOUSE_THRESHOLD
                && loop_now.duration_since(last_reenumerate) >= REENUMERATE_COOLDOWN
            {
                eprintln!(
                    "Stale mouse detected (no mouse events for {}s, keyboard active). Re-enumerating...",
                    loop_now.duration_since(last_mouse_event).as_secs()
                );
                devices = enumerate_input_devices();
                last_reenumerate = loop_now;
                last_mouse_event = loop_now;
            }

            for device in &mut devices {
                if let Ok(events) = device.fetch_events() {
                    for ev in events {
                        let now = start.elapsed().as_millis() as u64;

                        match ev.event_type() {
                            evdev::EventType::KEY => {
                                if ev.value() == 1 {
                                    let code = ev.code();
                                    if (272..=279).contains(&code) {
                                        mouse_clicks.fetch_add(1, Ordering::Relaxed);
                                        last_activity.store(now, Ordering::Relaxed);
                                        last_mouse_event = Instant::now();
                                    } else {
                                        keystrokes.fetch_add(1, Ordering::Relaxed);
                                        last_activity.store(now, Ordering::Relaxed);
                                        last_keyboard_event = Instant::now();
                                    }
                                }
                            }
                            evdev::EventType::RELATIVE => {
                                let code = ev.code();
                                if code == 0 || code == 1 {
                                    mouse_distance.fetch_add(
                                        ev.value().unsigned_abs() as u64,
                                        Ordering::Relaxed,
                                    );
                                    last_activity.store(now, Ordering::Relaxed);
                                    last_mouse_event = Instant::now();
                                } else if code == 8 || code == 11 {
                                    scroll_events.fetch_add(1, Ordering::Relaxed);
                                    last_activity.store(now, Ordering::Relaxed);
                                    last_mouse_event = Instant::now();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    });

    stats
}

fn classify(app_id: &str, title: &str, config: &Config) -> Category {
    let title_lower = title.to_lowercase();
    for rule in &config.title_rules {
        if title_lower.contains(&rule.pattern.to_lowercase()) {
            return rule.category;
        }
    }
    config
        .categories
        .get(app_id)
        .copied()
        .unwrap_or(Category::Neutral)
}

fn flush_session(
    conn: &Connection,
    info: &WindowInfo,
    config: &Config,
    focus_start: chrono::DateTime<Utc>,
    active_ms: i64,
    idle_ms: i64,
    keystrokes: u64,
    mouse_clicks: u64,
    scroll_events: u64,
    mouse_distance: u64,
) -> Result<(), Error> {
    let category = classify(&info.app_id, &info.title, config);
    conn.execute(
        "INSERT INTO events (timestamp, app_id, title, category, active_ms, idle_ms, keystrokes, mouse_clicks, scroll_events, mouse_distance) 
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            focus_start.to_rfc3339(),
            &info.app_id,
            &info.title,
            category.as_str(),
            active_ms,
            idle_ms,
            keystrokes as i64,
            mouse_clicks as i64,
            scroll_events as i64,
            mouse_distance as i64
        ],
    )?;
    Ok(())
}

fn connect_to_niri() -> Result<Socket, Error> {
    let backoff = ExponentialBackoff {
        initial_interval: Duration::from_millis(100),
        multiplier: 2.0,
        max_interval: Duration::from_secs(30),
        max_elapsed_time: Some(Duration::from_secs(300)),
        ..Default::default()
    };

    backoff::retry(backoff, || {
        match Socket::connect() {
            Ok(socket) => Ok(socket),
            Err(e) => {
                eprintln!("Connection to niri failed: {}. Retrying...", e);
                Err(backoff::Error::transient(e))
            }
        }
    })
    .map_err(|e| match e {
        backoff::Error::Transient { err, .. } | backoff::Error::Permanent(err) => {
            Error::NiriIpc(err)
        }
    })
}

fn watch(quiet: bool) -> Result<(), Error> {
    use std::collections::HashSet;
    use std::fs::OpenOptions;
    use std::io::Write;

    let config = load_config()?;
    let db_path = get_data_dir().join("activity.db");
    let untracked_log_path = get_data_dir().join("untracked_apps.log");
    let mut logged_untracked: HashSet<String> = HashSet::new();

    println!("Database: {}", db_path.display());
    println!("Config: {}", get_config_path().display());
    println!("Idle threshold: {}s", config.idle_threshold_secs);
    println!("Categories configured: {}", config.categories.len());

    let conn = Connection::open(&db_path)?;
    init_db(&conn)?;
    run_migrations(&conn, &config)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            shutdown_clone.store(true, Ordering::SeqCst);
        }
    });

    let monitor_start = Instant::now();
    let input_stats = start_idle_monitor(monitor_start);
    let idle_threshold_ms = config.idle_threshold_secs * 1000;

    let mut socket = connect_to_niri()?;
    let reply = socket.send(Request::EventStream)?;
    match reply {
        Ok(Response::Handled) => {}
        Ok(other) => {
            return Err(Error::NiriError(format!(
                "unexpected response: {:?}",
                other
            )));
        }
        Err(e) => return Err(Error::NiriError(e)),
    }

    let (tx, rx) = mpsc::channel::<Event>();

    thread::spawn(move || {
        let mut read_event = socket.read_events();
        loop {
            match read_event() {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut windows: HashMap<u64, WindowInfo> = HashMap::new();
    let mut focused_id: Option<u64> = None;
    let mut focus_start = Utc::now();
    let mut accumulated_active_ms: i64 = 0;
    let mut accumulated_idle_ms: i64 = 0;
    let mut last_idle_check = Instant::now();
    let mut last_flush = Instant::now();
    const FLUSH_INTERVAL: Duration = Duration::from_secs(300);

    println!("\nWatching window focus (event-driven)...");
    println!("Press Ctrl+C to stop gracefully\n");

    loop {
        if shutdown.load(Ordering::SeqCst) {
            eprintln!("\nShutdown signal received, flushing current session...");
            if let Some(prev_id) = focused_id {
                if let Some(info) = windows.get(&prev_id) {
                    let (keystrokes, mouse_clicks, scroll_events, mouse_distance) =
                        input_stats.reset_counts();
                    flush_session(
                        &conn,
                        info,
                        &config,
                        focus_start,
                        accumulated_active_ms,
                        accumulated_idle_ms,
                        keystrokes,
                        mouse_clicks,
                        scroll_events,
                        mouse_distance,
                    )?;
                    let total = accumulated_active_ms + accumulated_idle_ms;
                    eprintln!(
                        "Flushed: {} ({}ms active, {}ms idle)",
                        info.app_id, accumulated_active_ms, accumulated_idle_ms
                    );
                    eprintln!("Total session time saved: {}ms", total);
                }
            }
            eprintln!("Graceful shutdown complete.");
            return Ok(());
        }

        let now_instant = Instant::now();
        let now_ms = monitor_start.elapsed().as_millis() as u64;
        let last_input_ms = input_stats.last_activity_ms.load(Ordering::Relaxed);
        let idle_duration_ms = now_ms.saturating_sub(last_input_ms);
        let is_idle = idle_duration_ms > idle_threshold_ms;

        let elapsed_since_last_check =
            now_instant.duration_since(last_idle_check).as_millis() as i64;
        if is_idle {
            accumulated_idle_ms += elapsed_since_last_check;
        } else {
            accumulated_active_ms += elapsed_since_last_check;
        }
        last_idle_check = now_instant;

        if now_instant.duration_since(last_flush) >= FLUSH_INTERVAL {
            if let Some(id) = focused_id {
                if let Some(info) = windows.get(&id) {
                    let (keystrokes, mouse_clicks, scroll_events, mouse_distance) =
                        input_stats.reset_counts();
                    flush_session(
                        &conn,
                        info,
                        &config,
                        focus_start,
                        accumulated_active_ms,
                        accumulated_idle_ms,
                        keystrokes,
                        mouse_clicks,
                        scroll_events,
                        mouse_distance,
                    )?;
                    if !quiet {
                        eprintln!(
                            "[periodic flush] {} ({}ms)",
                            info.app_id,
                            accumulated_active_ms + accumulated_idle_ms
                        );
                    }
                    focus_start = Utc::now();
                    accumulated_active_ms = 0;
                    accumulated_idle_ms = 0;
                }
            }
            last_flush = now_instant;
        }

        let event = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                if shutdown.load(Ordering::SeqCst) {
                    continue;
                }
                return Err(Error::NiriError("event stream disconnected".into()));
            }
        };

        let Some(event) = event else {
            continue;
        };

        let now = Utc::now();

        match event {
            Event::WindowsChanged { windows: win_list } => {
                windows.clear();
                input_stats.reset_counts();
                for w in &win_list {
                    windows.insert(w.id, WindowInfo::from(w));
                    if w.is_focused {
                        focused_id = Some(w.id);
                        focus_start = now;
                        accumulated_active_ms = 0;
                        accumulated_idle_ms = 0;
                        last_idle_check = now_instant;
                    }
                }
                eprintln!(
                    "Loaded {} windows, focused: {:?}",
                    windows.len(),
                    focused_id
                );
            }

            Event::WindowOpenedOrChanged { window } => {
                windows.insert(window.id, WindowInfo::from(&window));
            }

            Event::WindowClosed { id } => {
                windows.remove(&id);
            }

            Event::WindowFocusChanged { id: new_focus_id } => {
                let (keystrokes, mouse_clicks, scroll_events, mouse_distance) =
                    input_stats.reset_counts();

                if let Some(prev_id) = focused_id
                    && let Some(info) = windows.get(&prev_id)
                {
                    flush_session(
                        &conn,
                        info,
                        &config,
                        focus_start,
                        accumulated_active_ms,
                        accumulated_idle_ms,
                        keystrokes,
                        mouse_clicks,
                        scroll_events,
                        mouse_distance,
                    )?;

                    let total = accumulated_active_ms + accumulated_idle_ms;

                    let category = classify(&info.app_id, &info.title, &config);
                    if category == Category::Neutral
                        && !config.categories.contains_key(&info.app_id)
                    {
                        if logged_untracked.insert(info.app_id.clone()) {
                            if let Ok(mut file) = OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&untracked_log_path)
                            {
                                let _ = writeln!(file, "{}", info.app_id);
                            }
                        }
                    }

                    if !quiet && total >= 500 {
                        let idle_pct = if total > 0 {
                            (accumulated_idle_ms as f64 / total as f64 * 100.0) as u32
                        } else {
                            0
                        };
                        println!(
                            "[{}] {} ({}) - \"{}\" ({}, {}% idle, {} keys, {} clicks, {} scroll, {}px)",
                            focus_start.with_timezone(&Local).format("%H:%M:%S"),
                            info.app_id,
                            category.as_str(),
                            truncate(&info.title, 40),
                            fmt_duration_compact(total),
                            idle_pct,
                            keystrokes,
                            mouse_clicks,
                            scroll_events,
                            mouse_distance
                        );
                    }
                }

                focused_id = new_focus_id;
                focus_start = now;
                accumulated_active_ms = 0;
                accumulated_idle_ms = 0;
                last_idle_check = now_instant;
                last_flush = now_instant;
            }

            _ => {}
        }
    }
}

fn fmt_duration_compact(ms: i64) -> String {
    let total_secs = ms / 1000;
    if total_secs < 60 {
        format!("{}s", total_secs)
    } else if total_secs < 3600 {
        format!("{}m {:02}s", total_secs / 60, total_secs % 60)
    } else {
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        format!("{}h {:02}m", h, m)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{}...", end)
    }
}

fn show_today() -> Result<(), Error> {
    let config = load_config()?;
    let db_path = get_data_dir().join("activity.db");
    let conn = Connection::open(&db_path)?;
    run_migrations(&conn, &config)?;

    let local_today = Local::now().date_naive();
    let day_start_utc = local_today
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
        .with_timezone(&Utc)
        .to_rfc3339();

    let mut stmt = conn.prepare(
        "SELECT app_id, category, SUM(active_ms + idle_ms) as total_ms 
         FROM events 
         WHERE timestamp >= ?1 
         GROUP BY app_id 
         ORDER BY total_ms DESC",
    )?;

    let rows = stmt.query_map([&day_start_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    println!("Activity for today ({}):\n", local_today);
    println!("{:<30} {:>12} {:>10}", "Application", "Category", "Time");
    println!("{}", "-".repeat(54));

    for row in rows {
        let (app_id, category, total_ms) = row?;
        let hours = total_ms / 3_600_000;
        let mins = (total_ms % 3_600_000) / 60_000;
        let secs = (total_ms % 60_000) / 1000;

        println!(
            "{:<30} {:>12} {:>2}h {:>2}m {:>2}s",
            truncate(&app_id, 28),
            category,
            hours,
            mins,
            secs
        );
    }

    Ok(())
}

#[derive(Default)]
struct Metrics {
    total_ms: i64,
    productive_ms: i64,
    unproductive_ms: i64,
    productive_active_ms: i64,
    productive_idle_ms: i64,
}

fn show_metrics(days: u32) -> Result<(), Error> {
    let config = load_config()?;
    let db_path = get_data_dir().join("activity.db");
    let conn = Connection::open(&db_path)?;
    run_migrations(&conn, &config)?;

    let since_local = Local::now().date_naive() - chrono::Duration::days(days as i64);
    let since_utc = since_local
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
        .with_timezone(&Utc)
        .to_rfc3339();

    let mut stmt = conn.prepare(
        "SELECT category, SUM(active_ms) as active, SUM(idle_ms) as idle
         FROM events 
         WHERE timestamp >= ?1 
         GROUP BY category",
    )?;

    let mut m = Metrics::default();

    let rows = stmt.query_map([&since_utc], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    for row in rows {
        let (category, active_ms, idle_ms) = row?;
        let total = active_ms + idle_ms;
        m.total_ms += total;

        match category.as_str() {
            "productive" => {
                m.productive_ms += total;
                m.productive_active_ms += active_ms;
                m.productive_idle_ms += idle_ms;
            }
            "unproductive" => {
                m.unproductive_ms += total;
            }
            _ => {}
        }
    }

    fn fmt_duration(ms: i64) -> String {
        let hours = ms / 3_600_000;
        let mins = (ms % 3_600_000) / 60_000;
        format!("{}h {}m", hours, mins)
    }

    fn pct(part: i64, total: i64) -> String {
        if total == 0 {
            "0%".to_string()
        } else {
            format!("{:.1}%", part as f64 / total as f64 * 100.0)
        }
    }

    println!(
        "=== Productivity Metrics ({} day{}) ===\n",
        days,
        if days == 1 { "" } else { "s" }
    );

    println!("Total Time:              {}", fmt_duration(m.total_ms));
    println!(
        "Productive Time:         {} ({})",
        fmt_duration(m.productive_ms),
        pct(m.productive_ms, m.total_ms)
    );
    println!(
        "Unproductive Time:       {} ({})",
        fmt_duration(m.unproductive_ms),
        pct(m.unproductive_ms, m.total_ms)
    );
    println!();
    println!(
        "Productive Active Time:  {} ({})",
        fmt_duration(m.productive_active_ms),
        pct(m.productive_active_ms, m.productive_ms)
    );
    println!(
        "Productive Passive Time: {} ({})",
        fmt_duration(m.productive_idle_ms),
        pct(m.productive_idle_ms, m.productive_ms)
    );

    Ok(())
}

fn show_timeline(days_back: u32, bucket_min: u32) -> Result<(), Error> {
    assert!(bucket_min > 0, "bucket size must be positive");
    let config = load_config()?;
    let db_path = get_data_dir().join("activity.db");
    let conn = Connection::open(&db_path)?;
    run_migrations(&conn, &config)?;

    let target_local = Local::now().date_naive() - chrono::Duration::days(days_back as i64);
    let day_start_utc = target_local
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
        .with_timezone(&Utc)
        .to_rfc3339();
    let day_end_utc = (target_local + chrono::Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
        .with_timezone(&Utc)
        .to_rfc3339();

    let mut stmt = conn.prepare(
        "SELECT timestamp, app_id, category, active_ms, idle_ms, keystrokes
         FROM events
         WHERE timestamp >= ?1 AND timestamp < ?2
         ORDER BY timestamp",
    )?;

    struct EventRow {
        timestamp: String,
        app_id: String,
        category: String,
        active_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
    }

    let events: Vec<EventRow> = stmt
        .query_map(params![&day_start_utc, &day_end_utc], |row| {
            Ok(EventRow {
                timestamp: row.get(0)?,
                app_id: row.get(1)?,
                category: row.get(2)?,
                active_ms: row.get(3)?,
                idle_ms: row.get(4)?,
                keystrokes: row.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    if events.is_empty() {
        println!("No activity recorded for {}", target_local);
        return Ok(());
    }

    struct Bucket {
        productive_ms: i64,
        neutral_ms: i64,
        unproductive_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
        dominant_app: String,
        dominant_app_ms: i64,
    }

    let mut buckets: std::collections::BTreeMap<u32, Bucket> = std::collections::BTreeMap::new();

    for ev in &events {
        let ts = chrono::DateTime::parse_from_rfc3339(&ev.timestamp)
            .map(|dt| dt.with_timezone(&chrono::Local))
            .unwrap_or_else(|_| chrono::Local::now());

        let minutes_since_midnight = ts.hour() * 60 + ts.minute();
        let bucket_key = minutes_since_midnight / bucket_min * bucket_min;
        let total_ms = ev.active_ms + ev.idle_ms;

        let b = buckets.entry(bucket_key).or_insert_with(|| Bucket {
            productive_ms: 0,
            neutral_ms: 0,
            unproductive_ms: 0,
            idle_ms: 0,
            keystrokes: 0,
            dominant_app: String::new(),
            dominant_app_ms: 0,
        });

        match ev.category.as_str() {
            "productive" => b.productive_ms += total_ms,
            "unproductive" => b.unproductive_ms += total_ms,
            _ => b.neutral_ms += total_ms,
        }
        b.idle_ms += ev.idle_ms;
        b.keystrokes += ev.keystrokes;

        if total_ms > b.dominant_app_ms {
            b.dominant_app_ms = total_ms;
            b.dominant_app.clone_from(&ev.app_id);
        }
    }

    println!("=== Timeline for {} ({}min buckets) ===\n", target_local, bucket_min);

    let bar_width: usize = 20;

    for (&bucket_key, b) in &buckets {
        let hour = bucket_key / 60;
        let min = bucket_key % 60;
        let total = b.productive_ms + b.neutral_ms + b.unproductive_ms;
        if total == 0 {
            continue;
        }

        let idle_pct = if total > 0 {
            b.idle_ms as f64 / total as f64
        } else {
            0.0
        };

        let prod_frac = b.productive_ms as f64 / total as f64;
        let neutral_frac = b.neutral_ms as f64 / total as f64;
        let unprod_frac = b.unproductive_ms as f64 / total as f64;

        let prod_chars = (prod_frac * bar_width as f64).round() as usize;
        let neutral_chars = (neutral_frac * bar_width as f64).round() as usize;
        let unprod_chars = (unprod_frac * bar_width as f64).round() as usize;
        let remaining = bar_width.saturating_sub(prod_chars + neutral_chars + unprod_chars);

        let bar = format!(
            "{}{}{}{}",
            "█".repeat(prod_chars),
            "▒".repeat(neutral_chars),
            "░".repeat(unprod_chars),
            " ".repeat(remaining),
        );

        let idle_marker = if idle_pct > 0.8 {
            " [AFK]"
        } else if idle_pct > 0.5 {
            " [mostly idle]"
        } else {
            ""
        };

        println!(
            "{:02}:{:02} {} {:>6} {:>4} keys  {:<20}{}",
            hour,
            min,
            bar,
            fmt_duration_compact(total),
            b.keystrokes,
            truncate(&b.dominant_app, 20),
            idle_marker,
        );
    }

    println!("\n  █ productive  ▒ neutral  ░ unproductive");

    Ok(())
}

fn generate_report(days: u32) -> Result<(), Error> {
    assert!(days > 0, "report must cover at least 1 day");
    let config = load_config()?;
    let db_path = get_data_dir().join("activity.db");
    let conn = Connection::open(&db_path)?;
    run_migrations(&conn, &config)?;

    let since_local = Local::now().date_naive() - chrono::Duration::days(days as i64);
    let since_utc = since_local
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
        .with_timezone(&Utc)
        .to_rfc3339();
    let now_str = Local::now().format("%Y-%m-%d %H:%M").to_string();

    println!("╔══════════════════════════════════════════════════════╗");
    println!("║           ACTIVITY REPORT                           ║");
    println!(
        "║  Period: {} → {}              ║",
        since_local, now_str
    );
    println!("╚══════════════════════════════════════════════════════╝\n");

    let (total_ms, active_ms, idle_ms, total_keys, total_clicks, total_scroll, total_events): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = conn.query_row(
        "SELECT COALESCE(SUM(active_ms + idle_ms),0), COALESCE(SUM(active_ms),0),
                COALESCE(SUM(idle_ms),0), COALESCE(SUM(keystrokes),0),
                COALESCE(SUM(mouse_clicks),0), COALESCE(SUM(scroll_events),0),
                COUNT(*)
         FROM events WHERE timestamp >= ?1",
        params![&since_utc],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;

    if total_events == 0 {
        println!("No activity recorded for this period.");
        return Ok(());
    }

    fn fmt_hrs(ms: i64) -> String {
        let h = ms / 3_600_000;
        let m = (ms % 3_600_000) / 60_000;
        format!("{}h {}m", h, m)
    }

    fn pct(part: i64, total: i64) -> String {
        if total == 0 {
            "0.0%".to_string()
        } else {
            format!("{:.1}%", part as f64 / total as f64 * 100.0)
        }
    }

    println!("── Overview ──────────────────────────────────────────");
    println!("  Total Time:         {}", fmt_hrs(total_ms));
    println!(
        "  Active:             {} ({})",
        fmt_hrs(active_ms),
        pct(active_ms, total_ms)
    );
    println!(
        "  Idle/AFK:           {} ({})",
        fmt_hrs(idle_ms),
        pct(idle_ms, total_ms)
    );
    println!("  Focus Switches:     {}", total_events);
    println!("  Keystrokes:         {}", total_keys);
    println!("  Mouse Clicks:       {}", total_clicks);
    println!("  Scroll Events:      {}", total_scroll);

    println!("\n── Productivity ──────────────────────────────────────");

    let mut cat_stmt = conn.prepare(
        "SELECT category, SUM(active_ms + idle_ms), SUM(active_ms), SUM(idle_ms)
         FROM events WHERE timestamp >= ?1
         GROUP BY category ORDER BY SUM(active_ms + idle_ms) DESC",
    )?;

    let cats: Vec<(String, i64, i64, i64)> = cat_stmt
        .query_map(params![&since_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (cat, cat_total, cat_active, _cat_idle) in &cats {
        let bar_len = (*cat_total as f64 / total_ms as f64 * 30.0).round() as usize;
        let icon = match cat.as_str() {
            "productive" => "●",
            "unproductive" => "○",
            _ => "◌",
        };
        println!(
            "  {} {:<14} {} {} (active: {})",
            icon,
            cat,
            "█".repeat(bar_len),
            fmt_hrs(*cat_total),
            pct(*cat_active, *cat_total),
        );
    }

    println!("\n── Top Applications ──────────────────────────────────");

    let mut app_stmt = conn.prepare(
        "SELECT app_id, category, SUM(active_ms + idle_ms), SUM(keystrokes), SUM(mouse_clicks)
         FROM events WHERE timestamp >= ?1
         GROUP BY app_id ORDER BY SUM(active_ms + idle_ms) DESC
         LIMIT 10",
    )?;

    let apps: Vec<(String, String, i64, i64, i64)> = app_stmt
        .query_map(params![&since_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (app, cat, app_total, keys, clicks) in &apps {
        let bar_len = (*app_total as f64 / total_ms as f64 * 25.0).round() as usize;
        println!(
            "  {:<22} {} {:>8}  {:>5} keys {:>3} clicks  ({})",
            truncate(app, 22),
            "█".repeat(bar_len),
            fmt_hrs(*app_total),
            keys,
            clicks,
            cat,
        );
    }

    // Daily breakdown and peak hours: fetch raw timestamps, bucket in Rust for correct local time
    let mut raw_stmt = conn.prepare(
        "SELECT timestamp, active_ms, idle_ms, keystrokes
         FROM events WHERE timestamp >= ?1",
    )?;

    struct RawRow {
        timestamp: String,
        active_ms: i64,
        idle_ms: i64,
        keystrokes: i64,
    }

    let raw_rows: Vec<RawRow> = raw_stmt
        .query_map(params![&since_utc], |row| {
            Ok(RawRow {
                timestamp: row.get(0)?,
                active_ms: row.get(1)?,
                idle_ms: row.get(2)?,
                keystrokes: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    println!("\n── Daily Breakdown ───────────────────────────────────");

    let mut daily: std::collections::BTreeMap<String, (i64, i64, i64, i64)> =
        std::collections::BTreeMap::new();
    let mut hourly: HashMap<u32, (i64, i64)> = HashMap::new();

    for row in &raw_rows {
        let local_dt = chrono::DateTime::parse_from_rfc3339(&row.timestamp)
            .map(|dt| dt.with_timezone(&Local))
            .unwrap_or_else(|_| Local::now());

        let day_key = local_dt.format("%Y-%m-%d").to_string();
        let hour_key = local_dt.hour();
        let total = row.active_ms + row.idle_ms;

        let d = daily.entry(day_key).or_insert((0, 0, 0, 0));
        d.0 += total;
        d.1 += row.active_ms;
        d.2 += row.keystrokes;
        d.3 += 1;

        let h = hourly.entry(hour_key).or_insert((0, 0));
        h.0 += total;
        h.1 += row.keystrokes;
    }

    for (day, (day_total, day_active, day_keys, switches)) in &daily {
        println!(
            "  {}  {:>8}  active: {}  {:>5} keys  {} switches",
            day,
            fmt_hrs(*day_total),
            pct(*day_active, *day_total),
            day_keys,
            switches,
        );
    }

    println!("\n── Peak Hours ────────────────────────────────────────");

    let mut hour_vec: Vec<(u32, i64, i64)> = hourly
        .into_iter()
        .map(|(h, (total, keys))| (h, total, keys))
        .collect();
    hour_vec.sort_by(|a, b| b.1.cmp(&a.1));
    hour_vec.truncate(5);

    let max_hour_ms = hour_vec.first().map(|h| h.1).unwrap_or(1);
    for (hour, hour_total, hour_keys) in &hour_vec {
        let bar_len = (*hour_total as f64 / max_hour_ms as f64 * 20.0).round() as usize;
        println!(
            "  {:02}:00  {} {:>8}  {:>5} keys",
            hour,
            "█".repeat(bar_len),
            fmt_hrs(*hour_total),
            hour_keys,
        );
    }

    println!();

    Ok(())
}

fn init_config() -> Result<(), Error> {
    let config_path = get_config_path();

    if config_path.exists() {
        println!("Config already exists: {}", config_path.display());
        return Ok(());
    }

    let example = r#"# Idle threshold in seconds (default: 120)
idle_threshold_secs = 120

# Map app_id to category: productive, unproductive, neutral
[categories]
# IDEs / Development
"jetbrains-rustrover" = "productive"
"jetbrains-idea" = "productive"
"jetbrains-pycharm" = "productive"
"code" = "productive"
"zed" = "productive"
"neovim" = "productive"
"vim" = "productive"

# Terminal
"Alacritty" = "productive"
"kitty" = "productive"
"foot" = "productive"
"wezterm" = "productive"

# Browser (productive by default — override specific sites with [[title_rules]] below)
"zen" = "productive"
"firefox" = "productive"
"chromium" = "productive"

# Notes / Docs
"obsidian" = "productive"
"logseq" = "productive"
"notion" = "productive"

# Email
"thunderbird" = "productive"

# Communication
"slack" = "neutral"
"discord" = "unproductive"
"vesktop" = "unproductive"
"teams" = "productive"
"zoom" = "neutral"

# Entertainment
"spotify" = "unproductive"
"steam" = "unproductive"
"vlc" = "unproductive"
"mpv" = "unproductive"

# Title rules — override category based on window title (case-insensitive substring match)
# Checked BEFORE app-level categories, so these take priority.
# Useful for browsers where the same app_id can be productive or not.

[[title_rules]]
pattern = "YouTube"
category = "unproductive"

[[title_rules]]
pattern = "Instagram"
category = "unproductive"

[[title_rules]]
pattern = "Spotify"
category = "unproductive"

[[title_rules]]
pattern = "Discord"
category = "unproductive"

[[title_rules]]
pattern = "Reddit"
category = "unproductive"

[[title_rules]]
pattern = "GitHub"
category = "productive"

[[title_rules]]
pattern = "LinkedIn"
category = "productive"

[[title_rules]]
pattern = "Stack Overflow"
category = "productive"

[[title_rules]]
pattern = "docs.rs"
category = "productive"
"#;

    fs::write(&config_path, example)?;
    println!("Created config: {}", config_path.display());
    println!("\nEdit this file to customize your categories.");

    Ok(())
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Watch { quiet } => watch(quiet),
        Commands::Today => show_today(),
        Commands::Metrics { days } => show_metrics(days),
        Commands::Timeline { days, bucket } => show_timeline(days, bucket),
        Commands::Report { days } => generate_report(days),
        Commands::Init => init_config(),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
