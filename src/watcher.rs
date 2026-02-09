use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use backoff::ExponentialBackoff;
use chrono::{Local, Utc};
use niri_ipc::{Event, Request, Response, Window, socket::Socket};
use rusqlite::Connection;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::config::{Category, get_config_path, get_data_dir, load_config};
use crate::db::{SessionSnapshot, init_db, insert_event, run_migrations};
use crate::error::Error;
use crate::fmt::{fmt_duration_compact, truncate};
use crate::input::start_idle_monitor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Active,
    Passive,
    Idle,
    Locked,
}

impl std::fmt::Display for ActivityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivityState::Active => write!(f, "active"),
            ActivityState::Passive => write!(f, "passive"),
            ActivityState::Idle => write!(f, "idle"),
            ActivityState::Locked => write!(f, "locked"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub app_id: String,
    pub title: String,
}

impl From<&Window> for WindowInfo {
    fn from(w: &Window) -> Self {
        Self {
            app_id: w.app_id.clone().unwrap_or_else(|| "unknown".into()),
            title: w.title.clone().unwrap_or_default(),
        }
    }
}

fn is_session_locked() -> bool {
    let session_id = match Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout
                .lines()
                .find(|line| !line.trim().is_empty())
                .and_then(|line| line.split_whitespace().next())
                .map(|id| id.to_string())
        }
        _ => None,
    };

    let Some(session_id) = session_id else {
        return false;
    };

    match Command::new("loginctl")
        .args(["show-session", &session_id, "-p", "LockedHint", "--value"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.trim().eq_ignore_ascii_case("yes")
        }
        _ => false,
    }
}

pub fn connect_to_niri() -> Result<Socket, Error> {
    let backoff = ExponentialBackoff {
        initial_interval: Duration::from_millis(100),
        multiplier: 2.0,
        max_interval: Duration::from_secs(30),
        max_elapsed_time: Some(Duration::from_secs(300)),
        ..Default::default()
    };

    backoff::retry(backoff, || match Socket::connect() {
        Ok(socket) => Ok(socket),
        Err(e) => {
            eprintln!("Connection to niri failed: {}. Retrying...", e);
            Err(backoff::Error::transient(e))
        }
    })
    .map_err(|e| match e {
        backoff::Error::Transient { err, .. } | backoff::Error::Permanent(err) => {
            Error::NiriIpc(err)
        }
    })
}

pub fn watch(quiet: bool) -> Result<(), Error> {
    use std::collections::HashSet;
    use std::fs::OpenOptions;
    use std::io::Write;

    let config = load_config()?;
    let data_dir = get_data_dir()?;
    let db_path = data_dir.join("activity.db");
    let untracked_log_path = data_dir.join("untracked_apps.log");
    let mut logged_untracked: HashSet<String> = HashSet::new();

    println!("Database: {}", db_path.display());
    println!("Config: {}", get_config_path()?.display());
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
    let input_stats = start_idle_monitor(monitor_start, config.jiggler.clone());
    let idle_threshold_ms = config.idle_threshold_secs.saturating_mul(1000);
    let deep_idle_threshold_ms = config.deep_idle_secs.saturating_mul(1000);

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
        while let Ok(event) = read_event() {
            if tx.send(event).is_err() {
                break;
            }
        }
    });

    let mut windows: HashMap<u64, WindowInfo> = HashMap::new();
    let mut focused_id: Option<u64> = None;
    let mut focus_start = Utc::now();
    let mut accumulated_active_ms: i64 = 0;
    let mut accumulated_passive_ms: i64 = 0;
    let mut accumulated_idle_ms: i64 = 0;
    let mut last_idle_check = Instant::now();
    let mut last_flush = Instant::now();
    let mut is_locked = false;
    let mut jiggler_was_detected = false;
    let mut current_state = ActivityState::Active;
    const FLUSH_INTERVAL: Duration = Duration::from_secs(300);

    println!("\nWatching window focus (event-driven)...");
    println!("Press Ctrl+C to stop gracefully\n");

    loop {
        if shutdown.load(Ordering::SeqCst) {
            eprintln!("\nShutdown signal received, flushing current session...");
            if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                let input = input_stats.snapshot();
                let jiggler = input_stats.jiggler_detected();
                insert_event(
                    &conn,
                    SessionSnapshot {
                        window: info.clone(),
                        config: &config,
                        focus_start,
                        active_ms: accumulated_active_ms,
                        passive_ms: accumulated_passive_ms,
                        idle_ms: accumulated_idle_ms,
                        input,
                        jiggler_detected: jiggler || jiggler_was_detected,
                    },
                )?;
                let total = accumulated_active_ms + accumulated_passive_ms + accumulated_idle_ms;
                eprintln!(
                    "Flushed: {} ({}ms active, {}ms passive, {}ms idle)",
                    info.app_id, accumulated_active_ms, accumulated_passive_ms, accumulated_idle_ms
                );
                eprintln!("Total session time saved: {}ms", total);
            }
            eprintln!("Graceful shutdown complete.");
            return Ok(());
        }

        let now_instant = Instant::now();
        let locked_now = is_session_locked();

        if locked_now && !is_locked {
            if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                let input = input_stats.snapshot();
                let jiggler = input_stats.jiggler_detected();
                insert_event(
                    &conn,
                    SessionSnapshot {
                        window: info.clone(),
                        config: &config,
                        focus_start,
                        active_ms: accumulated_active_ms,
                        passive_ms: accumulated_passive_ms,
                        idle_ms: accumulated_idle_ms,
                        input,
                        jiggler_detected: jiggler || jiggler_was_detected,
                    },
                )?;
            }
            is_locked = true;
            current_state = ActivityState::Locked;
            if !quiet {
                eprintln!("[LOCKED] Screen locked, pausing tracking");
            }
        }

        if !locked_now && is_locked {
            is_locked = false;
            current_state = ActivityState::Active;
            focus_start = Utc::now();
            accumulated_active_ms = 0;
            accumulated_passive_ms = 0;
            accumulated_idle_ms = 0;
            jiggler_was_detected = false;
            last_idle_check = now_instant;
            last_flush = now_instant;
            if !quiet {
                eprintln!("[UNLOCKED] Screen unlocked, resuming tracking");
            }
        }

        if !is_locked {
            let now_ms = monitor_start.elapsed().as_millis() as u64;
            let last_input_ms = input_stats.last_activity_ms();
            let idle_duration_ms = now_ms.saturating_sub(last_input_ms);

            let new_state = if idle_duration_ms > deep_idle_threshold_ms {
                ActivityState::Idle
            } else if idle_duration_ms > idle_threshold_ms {
                ActivityState::Passive
            } else {
                ActivityState::Active
            };

            if new_state != current_state && !quiet {
                eprintln!("[STATE] {} -> {}", current_state, new_state);
            }
            current_state = new_state;

            if input_stats.jiggler_detected() {
                jiggler_was_detected = true;
            }

            let elapsed_since_last_check =
                now_instant.duration_since(last_idle_check).as_millis() as i64;
            match current_state {
                ActivityState::Active => accumulated_active_ms += elapsed_since_last_check,
                ActivityState::Passive => accumulated_passive_ms += elapsed_since_last_check,
                ActivityState::Idle => accumulated_idle_ms += elapsed_since_last_check,
                ActivityState::Locked => {}
            }
            last_idle_check = now_instant;

            if now_instant.duration_since(last_flush) >= FLUSH_INTERVAL {
                if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                    let input = input_stats.snapshot();
                    let jiggler = input_stats.jiggler_detected();
                    insert_event(
                        &conn,
                        SessionSnapshot {
                            window: info.clone(),
                            config: &config,
                            focus_start,
                            active_ms: accumulated_active_ms,
                            passive_ms: accumulated_passive_ms,
                            idle_ms: accumulated_idle_ms,
                            input,
                            jiggler_detected: jiggler || jiggler_was_detected,
                        },
                    )?;
                    if !quiet {
                        let total =
                            accumulated_active_ms + accumulated_passive_ms + accumulated_idle_ms;
                        eprintln!(
                            "[periodic flush] {} ({})",
                            info.app_id,
                            fmt_duration_compact(total)
                        );
                    }
                    focus_start = Utc::now();
                    accumulated_active_ms = 0;
                    accumulated_passive_ms = 0;
                    accumulated_idle_ms = 0;
                    jiggler_was_detected = false;
                }
                last_flush = now_instant;
            }
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
                let _ = input_stats.snapshot();
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
                if is_locked {
                    focused_id = new_focus_id;
                    focus_start = now;
                    accumulated_active_ms = 0;
                    accumulated_passive_ms = 0;
                    accumulated_idle_ms = 0;
                    jiggler_was_detected = false;
                    last_idle_check = now_instant;
                    last_flush = now_instant;
                    continue;
                }
                let input = input_stats.snapshot();
                let jiggler = input_stats.jiggler_detected();

                if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                    let session_jiggler = jiggler || jiggler_was_detected;
                    insert_event(
                        &conn,
                        SessionSnapshot {
                            window: info.clone(),
                            config: &config,
                            focus_start,
                            active_ms: accumulated_active_ms,
                            passive_ms: accumulated_passive_ms,
                            idle_ms: accumulated_idle_ms,
                            input,
                            jiggler_detected: session_jiggler,
                        },
                    )?;

                    let total =
                        accumulated_active_ms + accumulated_passive_ms + accumulated_idle_ms;

                    let category = config.classify(&info.app_id, &info.title);
                    if category == Category::Neutral
                        && !config.categories.contains_key(&info.app_id)
                        && logged_untracked.insert(info.app_id.clone())
                        && let Ok(mut file) = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&untracked_log_path)
                    {
                        let _ = writeln!(file, "{}", info.app_id);
                    }

                    if !quiet && total >= 500 {
                        let idle_pct = if total > 0 {
                            ((accumulated_passive_ms + accumulated_idle_ms) as f64 / total as f64
                                * 100.0) as u32
                        } else {
                            0
                        };
                        let jiggler_tag = if session_jiggler { " [JIGGLER]" } else { "" };
                        println!(
                            "[{}] {} ({}) - \"{}\" ({}, {}% idle, {} keys, {} clicks, {} scroll, {}px){}",
                            focus_start.with_timezone(&Local).format("%H:%M:%S"),
                            info.app_id,
                            category,
                            truncate(&info.title, 40),
                            fmt_duration_compact(total),
                            idle_pct,
                            input.keystrokes,
                            input.mouse_clicks,
                            input.scroll_events,
                            input.mouse_distance,
                            jiggler_tag,
                        );
                    }
                }

                focused_id = new_focus_id;
                focus_start = now;
                accumulated_active_ms = 0;
                accumulated_passive_ms = 0;
                accumulated_idle_ms = 0;
                jiggler_was_detected = false;
                current_state = ActivityState::Active;
                last_idle_check = now_instant;
                last_flush = now_instant;
            }

            _ => {}
        }
    }
}
