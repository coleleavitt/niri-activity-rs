use std::collections::HashMap;
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
    let mut accumulated_idle_ms: i64 = 0;
    let mut last_idle_check = Instant::now();
    let mut last_flush = Instant::now();
    const FLUSH_INTERVAL: Duration = Duration::from_secs(300);

    println!("\nWatching window focus (event-driven)...");
    println!("Press Ctrl+C to stop gracefully\n");

    loop {
        if shutdown.load(Ordering::SeqCst) {
            eprintln!("\nShutdown signal received, flushing current session...");
            if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                let input = input_stats.snapshot();
                insert_event(
                    &conn,
                    SessionSnapshot {
                        window: info.clone(),
                        config: &config,
                        focus_start,
                        active_ms: accumulated_active_ms,
                        idle_ms: accumulated_idle_ms,
                        input,
                    },
                )?;
                let total = accumulated_active_ms + accumulated_idle_ms;
                eprintln!(
                    "Flushed: {} ({}ms active, {}ms idle)",
                    info.app_id, accumulated_active_ms, accumulated_idle_ms
                );
                eprintln!("Total session time saved: {}ms", total);
            }
            eprintln!("Graceful shutdown complete.");
            return Ok(());
        }

        let now_instant = Instant::now();
        let now_ms = monitor_start.elapsed().as_millis() as u64;
        let last_input_ms = input_stats.last_activity_ms();
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
            if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                let input = input_stats.snapshot();
                insert_event(
                    &conn,
                    SessionSnapshot {
                        window: info.clone(),
                        config: &config,
                        focus_start,
                        active_ms: accumulated_active_ms,
                        idle_ms: accumulated_idle_ms,
                        input,
                    },
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
                let input = input_stats.snapshot();

                if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                    insert_event(
                        &conn,
                        SessionSnapshot {
                            window: info.clone(),
                            config: &config,
                            focus_start,
                            active_ms: accumulated_active_ms,
                            idle_ms: accumulated_idle_ms,
                            input,
                        },
                    )?;

                    let total = accumulated_active_ms + accumulated_idle_ms;

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
                            (accumulated_idle_ms as f64 / total as f64 * 100.0) as u32
                        } else {
                            0
                        };
                        println!(
                            "[{}] {} ({}) - \"{}\" ({}, {}% idle, {} keys, {} clicks, {} scroll, {}px)",
                            focus_start.with_timezone(&Local).format("%H:%M:%S"),
                            info.app_id,
                            category,
                            truncate(&info.title, 40),
                            fmt_duration_compact(total),
                            idle_pct,
                            input.keystrokes,
                            input.mouse_clicks,
                            input.scroll_events,
                            input.mouse_distance
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
