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

use owo_colors::OwoColorize;

use crate::config::{Category, get_config_path, get_data_dir, load_config};
use crate::db::{SessionSnapshot, init_db, insert_event, reclassify_all, run_migrations};
use crate::error::Error;
use crate::fmt::{cat_colored, cat_label, fmt_duration_compact, truncate};
use crate::input::start_idle_monitor;
use crate::logind::start_logind_monitor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Active,
    Passive,
    Idle,
    Locked,
    Away,
}

impl std::fmt::Display for ActivityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivityState::Active => write!(f, "active"),
            ActivityState::Passive => write!(f, "passive"),
            ActivityState::Idle => write!(f, "idle"),
            ActivityState::Locked => write!(f, "locked"),
            ActivityState::Away => write!(f, "away"),
        }
    }
}

fn color_state(state: ActivityState) -> String {
    match state {
        ActivityState::Active => "active".green().bold().to_string(),
        ActivityState::Passive => "passive".yellow().bold().to_string(),
        ActivityState::Idle => "idle".red().to_string(),
        ActivityState::Locked => "locked".magenta().bold().to_string(),
        ActivityState::Away => "away".blue().bold().to_string(),
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
    println!("Away threshold: {}s", config.away_threshold_secs);
    println!("Mouse idle threshold: {} raw units", config.mouse_idle_threshold);
    println!("Categories configured: {}", config.categories.len());

    let conn = Connection::open(&db_path)?;
    init_db(&conn)?;
    run_migrations(&conn, &config)?;
    reclassify_all(&conn, &config)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            shutdown_clone.store(true, Ordering::SeqCst);
        }
    });

    let monitor_start = Instant::now();
    let input_stats = start_idle_monitor(monitor_start, config.jiggler.clone(), config.mouse_idle_threshold);
    let logind = start_logind_monitor()?;
    let idle_threshold_ms = config.idle_threshold_secs.saturating_mul(1000);
    let deep_idle_threshold_ms = config.deep_idle_secs.saturating_mul(1000);
    let away_threshold_ms = config.away_threshold_secs.saturating_mul(1000);

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
    let mut current_state = ActivityState::Active;
    const FLUSH_INTERVAL: Duration = Duration::from_secs(300);
    const SUSPEND_JUMP_THRESHOLD_SECS: i64 = 30;
    let mut last_wall_time = Utc::now();
    let mut last_loop_instant = Instant::now();
    let mut input_baseline_ms: u64 = input_stats.last_activity_ms();
    let mut session_start_mono_ms: u64 = monitor_start.elapsed().as_millis() as u64;

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
                        jiggler_detected: jiggler,
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

        // Suspend detection: wall clock advancing faster than monotonic clock
        // means the system was suspended (CLOCK_MONOTONIC pauses during suspend).
        let wall_now = Utc::now();
        let wall_elapsed_secs = (wall_now - last_wall_time).num_seconds();
        let mono_elapsed_secs = now_instant
            .duration_since(last_loop_instant)
            .as_secs() as i64;
        let time_jump_secs = wall_elapsed_secs.saturating_sub(mono_elapsed_secs);

        if time_jump_secs > SUSPEND_JUMP_THRESHOLD_SECS {
            if !quiet {
                eprintln!(
                    "{} System resume detected ({}s wall clock, {}s monotonic, {}s jump)",
                    "[SUSPEND]".blue().bold(),
                    wall_elapsed_secs,
                    mono_elapsed_secs,
                    time_jump_secs,
                );
            }
            if (accumulated_active_ms > 0
                || accumulated_passive_ms > 0
                || accumulated_idle_ms > 0)
                && let Some(info) = focused_id.and_then(|id| windows.get(&id))
            {
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
                        jiggler_detected: jiggler,
                    },
                )?;
            }
            focus_start = Utc::now();
            accumulated_active_ms = 0;
            accumulated_passive_ms = 0;
            accumulated_idle_ms = 0;
            current_state = ActivityState::Active;
            last_idle_check = now_instant;
            last_flush = now_instant;
            input_baseline_ms = input_stats.last_activity_ms();
            session_start_mono_ms = monitor_start.elapsed().as_millis() as u64;
        }
        last_wall_time = wall_now;
        last_loop_instant = now_instant;

        // D-Bus PrepareForSleep(false) complements wall-clock detection.
        // If the wall-clock jump already handled the resume above (time_jump_secs >
        // threshold), this flag was set too — but no harm in clearing it. If the
        // wall-clock method missed it (very short suspend), this catches it.
        if logind.take_suspend_resumed() && time_jump_secs <= SUSPEND_JUMP_THRESHOLD_SECS {
            if !quiet {
                eprintln!(
                    "{} System resume detected via D-Bus signal",
                    "[SUSPEND]".blue().bold(),
                );
            }
            if (accumulated_active_ms > 0
                || accumulated_passive_ms > 0
                || accumulated_idle_ms > 0)
                && let Some(info) = focused_id.and_then(|id| windows.get(&id))
            {
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
                        jiggler_detected: jiggler,
                    },
                )?;
            }
            focus_start = Utc::now();
            accumulated_active_ms = 0;
            accumulated_passive_ms = 0;
            accumulated_idle_ms = 0;
            current_state = ActivityState::Active;
            last_idle_check = now_instant;
            last_flush = now_instant;
            input_baseline_ms = input_stats.last_activity_ms();
            session_start_mono_ms = monitor_start.elapsed().as_millis() as u64;
        }

        let locked_now = logind.is_locked();

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
                        jiggler_detected: jiggler,
                    },
                )?;
            }
            is_locked = true;
            current_state = ActivityState::Locked;
            if !quiet {
                eprintln!(
                    "{} Screen locked, pausing tracking",
                    "[LOCKED]".magenta().bold()
                );
            }
        }

        if !locked_now && is_locked {
            is_locked = false;
            current_state = ActivityState::Active;
            focus_start = Utc::now();
            accumulated_active_ms = 0;
            accumulated_passive_ms = 0;
            accumulated_idle_ms = 0;
            last_idle_check = now_instant;
            last_flush = now_instant;
            input_baseline_ms = input_stats.last_activity_ms();
            session_start_mono_ms = monitor_start.elapsed().as_millis() as u64;
            if !quiet {
                eprintln!(
                    "{} Screen unlocked, resuming tracking",
                    "[UNLOCKED]".green().bold()
                );
            }
        }

        if !is_locked {
            let now_ms = monitor_start.elapsed().as_millis() as u64;
            let last_input_ms = input_stats.last_activity_ms();
            let idle_duration_ms = if last_input_ms > input_baseline_ms {
                now_ms.saturating_sub(last_input_ms)
            } else {
                now_ms.saturating_sub(session_start_mono_ms)
            };

            let new_state = if idle_duration_ms > away_threshold_ms {
                ActivityState::Away
            } else if idle_duration_ms > deep_idle_threshold_ms {
                ActivityState::Idle
            } else if idle_duration_ms > idle_threshold_ms {
                ActivityState::Passive
            } else {
                ActivityState::Active
            };

            if new_state == ActivityState::Away && current_state != ActivityState::Away {
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
                            jiggler_detected: jiggler,
                        },
                    )?;
                }
                if !quiet {
                    eprintln!(
                        "{} Entering away state after {}s idle, pausing tracking",
                        "[AWAY]".blue().bold(),
                        idle_duration_ms / 1000,
                    );
                }
                focus_start = Utc::now();
                accumulated_active_ms = 0;
                accumulated_passive_ms = 0;
                accumulated_idle_ms = 0;
                last_flush = now_instant;
                input_baseline_ms = input_stats.last_activity_ms();
                session_start_mono_ms = now_ms;
            }

            if current_state == ActivityState::Away && new_state != ActivityState::Away {
                if !quiet {
                    eprintln!(
                        "{} Resuming tracking (user activity detected)",
                        "[RESUMED]".green().bold(),
                    );
                }
                focus_start = Utc::now();
                accumulated_active_ms = 0;
                accumulated_passive_ms = 0;
                accumulated_idle_ms = 0;
                last_idle_check = now_instant;
                last_flush = now_instant;
                input_baseline_ms = input_stats.last_activity_ms();
                session_start_mono_ms = now_ms;
            }

            if new_state != current_state && !quiet {
                let from = color_state(current_state);
                let to = color_state(new_state);
                eprintln!("{} {} → {}", "[STATE]".dimmed(), from, to);
            }
            current_state = new_state;

            let elapsed_since_last_check =
                now_instant.duration_since(last_idle_check).as_millis() as i64;
            match current_state {
                ActivityState::Active => accumulated_active_ms += elapsed_since_last_check,
                ActivityState::Passive => accumulated_passive_ms += elapsed_since_last_check,
                ActivityState::Idle => accumulated_idle_ms += elapsed_since_last_check,
                ActivityState::Locked | ActivityState::Away => {}
            }
            last_idle_check = now_instant;

            if current_state != ActivityState::Away
                && now_instant.duration_since(last_flush) >= FLUSH_INTERVAL
            {
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
                            jiggler_detected: jiggler,
                        },
                    )?;
                    if !quiet {
                        let total =
                            accumulated_active_ms + accumulated_passive_ms + accumulated_idle_ms;
                        eprintln!(
                            "{} {} ({})",
                            "[flush]".dimmed(),
                            info.app_id.cyan(),
                            fmt_duration_compact(total).dimmed()
                        );
                    }
                    focus_start = Utc::now();
                    accumulated_active_ms = 0;
                    accumulated_passive_ms = 0;
                    accumulated_idle_ms = 0;
                    input_baseline_ms = input_stats.last_activity_ms();
                    session_start_mono_ms = now_ms;
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
                        accumulated_passive_ms = 0;
                        accumulated_idle_ms = 0;
                        last_idle_check = now_instant;
                        input_baseline_ms = input_stats.last_activity_ms();
                        session_start_mono_ms = monitor_start.elapsed().as_millis() as u64;
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
                    last_idle_check = now_instant;
                    last_flush = now_instant;
                    input_baseline_ms = input_stats.last_activity_ms();
                    session_start_mono_ms = monitor_start.elapsed().as_millis() as u64;
                    continue;
                }
                let input = input_stats.snapshot();
                let jiggler = input_stats.jiggler_detected();

                if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
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
                            jiggler_detected: jiggler,
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
                        let jiggler_tag = if jiggler {
                            format!(" {}", "[JIGGLER]".red().bold())
                        } else {
                            String::new()
                        };
                        let idle_str = if idle_pct > 80 {
                            format!("{}% idle", idle_pct).red().to_string()
                        } else if idle_pct > 50 {
                            format!("{}% idle", idle_pct).yellow().to_string()
                        } else {
                            format!("{}% idle", idle_pct).dimmed().to_string()
                        };
                        println!(
                            "{} {} ({}) - {} ({}, {}, {} keys, {} clicks, {} scroll, {}px){}",
                            format!("[{}]", focus_start.with_timezone(&Local).format("%H:%M:%S"))
                                .dimmed(),
                            cat_colored(category, &info.app_id),
                            cat_label(category),
                            format!("\"{}\"", truncate(&info.title, 40)).dimmed(),
                            fmt_duration_compact(total).bold(),
                            idle_str,
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
                current_state = ActivityState::Active;
                last_idle_check = now_instant;
                last_flush = now_instant;
                input_baseline_ms = input_stats.last_activity_ms();
                session_start_mono_ms = monitor_start.elapsed().as_millis() as u64;
            }

            _ => {}
        }
    }
}
