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

use crate::config::{Category, Config, get_config_path, get_data_dir, load_config};
use crate::db::{SessionSnapshot, init_db, insert_event, reclassify_all, run_migrations};
use crate::error::Error;
use crate::fmt::{cat_colored, cat_label, fmt_duration_compact, truncate};
use crate::input::{InputSnapshot, start_idle_monitor};
use crate::logind::start_logind_monitor;

/// Saturating conversion from `Duration::as_millis()` (`u128`) to `u64`.
/// Avoids silent truncation per JPL Rule 14 — practically unreachable
/// (would require 500M+ years uptime) but we never silently truncate.
fn millis_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn reclassify_false_active(
    active_ms: &mut i64,
    passive_ms: &mut i64,
    input: &InputSnapshot,
    input_active_ms: u64,
    quiet: bool,
) {
    if *active_ms > 0
        && input.keystrokes == 0
        && input.mouse_clicks == 0
        && u64::try_from(*active_ms).is_ok_and(|ms| ms >= input_active_ms)
    {
        if !quiet {
            eprintln!(
                "{} Reclassifying {}ms active → passive (0 keystrokes, 0 clicks)",
                "[INPUT]".yellow().bold(),
                *active_ms,
            );
        }
        *passive_ms = passive_ms.saturating_add(*active_ms);
        *active_ms = 0;
    }
}

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

/// Immutable context for flush operations, constructed once per watch session.
struct FlushContext<'a> {
    conn: &'a Connection,
    config: &'a Config,
    input_active_ms: u64,
    quiet: bool,
}

/// Mutable session accumulator state passed by reference to `flush_session`.
struct SessionAccum<'a> {
    focus_start: &'a mut chrono::DateTime<chrono::Utc>,
    active_ms: &'a mut i64,
    passive_ms: &'a mut i64,
    idle_ms: &'a mut i64,
    input_baseline_ms: &'a mut u64,
    session_start_mono_ms: &'a mut u64,
    input_offsets: &'a mut Vec<u32>,
    last_seen_input_ms: &'a mut u64,
}

/// Post-flush accumulator values, returned so callers can log before reset.
struct FlushResult {
    active_ms: i64,
    passive_ms: i64,
    idle_ms: i64,
}

/// Controls what state gets reset after flushing.
#[derive(Clone, Copy)]
enum FlushReset {
    /// No reset (shutdown, lock — caller manages post-flush state).
    NoReset,
    /// Reset accumulators and focus_start, preserve input baselines.
    /// Used by enter-away and periodic flush.
    PreserveBaselines {
        new_focus_start: chrono::DateTime<chrono::Utc>,
    },
    /// Full reset including input baselines.
    /// Used by suspend (wall-clock + D-Bus) and focus-change.
    Full {
        new_focus_start: chrono::DateTime<chrono::Utc>,
        new_baseline_ms: u64,
        new_session_mono_ms: u64,
    },
}

/// Flush the current session: reclassify false-active time, insert the DB
/// event, and optionally reset accumulators based on `reset` mode.
///
/// If `info` is `None`, the database insert is skipped but resets still apply
/// (needed for enter-away when no window is focused).
///
/// Returns post-reclassify accumulator values for caller-side logging.
fn flush_session(
    ctx: &FlushContext<'_>,
    info: Option<&WindowInfo>,
    accum: &mut SessionAccum<'_>,
    input: &InputSnapshot,
    jiggler: bool,
    reset: FlushReset,
) -> Result<FlushResult, Error> {
    if let Some(info) = info {
        reclassify_false_active(
            accum.active_ms,
            accum.passive_ms,
            input,
            ctx.input_active_ms,
            ctx.quiet,
        );
        insert_event(
            ctx.conn,
            SessionSnapshot {
                window: info.clone(),
                config: ctx.config,
                focus_start: *accum.focus_start,
                active_ms: *accum.active_ms,
                passive_ms: *accum.passive_ms,
                idle_ms: *accum.idle_ms,
                input: *input,
                jiggler_detected: jiggler,
                input_offsets: std::mem::take(accum.input_offsets),
            },
        )?;
    }

    let result = FlushResult {
        active_ms: *accum.active_ms,
        passive_ms: *accum.passive_ms,
        idle_ms: *accum.idle_ms,
    };

    match reset {
        FlushReset::NoReset => {}
        FlushReset::PreserveBaselines { new_focus_start } => {
            *accum.focus_start = new_focus_start;
            *accum.active_ms = 0;
            *accum.passive_ms = 0;
            *accum.idle_ms = 0;
            accum.input_offsets.clear();
        }
        FlushReset::Full {
            new_focus_start,
            new_baseline_ms,
            new_session_mono_ms,
        } => {
            *accum.focus_start = new_focus_start;
            *accum.active_ms = 0;
            *accum.passive_ms = 0;
            *accum.idle_ms = 0;
            *accum.input_baseline_ms = new_baseline_ms;
            *accum.session_start_mono_ms = new_session_mono_ms;
            *accum.last_seen_input_ms = new_baseline_ms;
            accum.input_offsets.clear();
        }
    }

    Ok(result)
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
            Error::NiriIpc(err.to_string())
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
    println!(
        "Mouse idle threshold: {} raw units",
        config.mouse_idle_threshold
    );
    println!("Input active threshold: {}s", config.input_active_secs);
    println!("Categories configured: {}", config.categories.len());

    let mut conn = Connection::open(&db_path)?;
    init_db(&conn)?;
    run_migrations(&mut conn, &config)?;
    reclassify_all(&mut conn, &config)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            shutdown_clone.store(true, Ordering::SeqCst);
        }
    });

    let monitor_start = Instant::now();
    let input_stats = start_idle_monitor(
        monitor_start,
        config.jiggler.clone(),
        config.mouse_idle_threshold,
    );
    let logind = start_logind_monitor()?;
    let idle_threshold_ms = config.idle_threshold_secs.saturating_mul(1000);
    let deep_idle_threshold_ms = config.deep_idle_secs.saturating_mul(1000);
    let away_threshold_ms = config.away_threshold_secs.saturating_mul(1000);
    let input_active_ms = config.input_active_secs.saturating_mul(1000);

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
    let mut session_start_mono_ms: u64 = millis_u64(monitor_start.elapsed());
    let mut logind_warned = false;
    let mut input_offsets: Vec<u32> = Vec::new();
    let mut last_seen_input_ms: u64 = input_baseline_ms;

    let flush_ctx = FlushContext {
        conn: &conn,
        config: &config,
        input_active_ms,
        quiet,
    };

    println!("\nWatching window focus (event-driven)...");
    println!("Press Ctrl+C to stop gracefully\n");

    loop {
        if shutdown.load(Ordering::SeqCst) {
            eprintln!("\nShutdown signal received, flushing current session...");
            if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                let input = input_stats.snapshot();
                let jiggler = input_stats.jiggler_detected();
                let flushed = flush_session(
                    &flush_ctx,
                    Some(info),
                    &mut SessionAccum {
                        focus_start: &mut focus_start,
                        active_ms: &mut accumulated_active_ms,
                        passive_ms: &mut accumulated_passive_ms,
                        idle_ms: &mut accumulated_idle_ms,
                        input_baseline_ms: &mut input_baseline_ms,
                        session_start_mono_ms: &mut session_start_mono_ms,
                        input_offsets: &mut input_offsets,
                        last_seen_input_ms: &mut last_seen_input_ms,
                    },
                    &input,
                    jiggler,
                    FlushReset::NoReset,
                )?;
                let total = flushed
                    .active_ms
                    .saturating_add(flushed.passive_ms)
                    .saturating_add(flushed.idle_ms);
                eprintln!(
                    "Flushed: {} ({}ms active, {}ms passive, {}ms idle)",
                    info.app_id, flushed.active_ms, flushed.passive_ms, flushed.idle_ms
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
        let mono_elapsed_secs =
            i64::try_from(now_instant.duration_since(last_loop_instant).as_secs())
                .unwrap_or(i64::MAX);
        let time_jump_secs = wall_elapsed_secs.saturating_sub(mono_elapsed_secs);

        // Suspend/resume detection: wall-clock jump OR D-Bus PrepareForSleep(false).
        // `take_suspend_resumed()` clears the flag atomically (swap), so we call it
        // every iteration regardless of whether the wall-clock path fires.
        // When both detect resume simultaneously, wall_clock_resume takes priority
        // and dbus_resume is suppressed — both indicate the same physical event.
        let wall_clock_resume = time_jump_secs > SUSPEND_JUMP_THRESHOLD_SECS;
        let dbus_resume_signalled = logind.take_suspend_resumed();
        let dbus_resume = dbus_resume_signalled && !wall_clock_resume;

        if wall_clock_resume || dbus_resume {
            if !quiet {
                let label = if wall_clock_resume {
                    format!(
                        "System resume detected ({}s wall clock, {}s monotonic, {}s jump)",
                        wall_elapsed_secs, mono_elapsed_secs, time_jump_secs,
                    )
                } else {
                    "System resume detected via D-Bus signal".to_string()
                };
                eprintln!("{} {}", "[SUSPEND]".blue().bold(), label);
            }
            let has_data =
                accumulated_active_ms > 0 || accumulated_passive_ms > 0 || accumulated_idle_ms > 0;
            let info = if has_data {
                focused_id.and_then(|id| windows.get(&id))
            } else {
                None
            };
            let input = input_stats.snapshot();
            let jiggler = input_stats.jiggler_detected();
            flush_session(
                &flush_ctx,
                info,
                &mut SessionAccum {
                    focus_start: &mut focus_start,
                    active_ms: &mut accumulated_active_ms,
                    passive_ms: &mut accumulated_passive_ms,
                    idle_ms: &mut accumulated_idle_ms,
                    input_baseline_ms: &mut input_baseline_ms,
                    session_start_mono_ms: &mut session_start_mono_ms,
                    input_offsets: &mut input_offsets,
                    last_seen_input_ms: &mut last_seen_input_ms,
                },
                &input,
                jiggler,
                FlushReset::Full {
                    new_focus_start: Utc::now(),
                    new_baseline_ms: input_stats.last_activity_ms(),
                    new_session_mono_ms: millis_u64(monitor_start.elapsed()),
                },
            )?;
            // Post-resume invariants:
            // - current_state = Active prevents false idle/away transitions
            // - last_idle_check = now_instant prevents elapsed_since_last_check spike
            // - FlushReset::Full resets baselines so idle_duration_ms starts fresh
            current_state = ActivityState::Active;
            last_idle_check = now_instant;
            last_flush = now_instant;
        }

        last_wall_time = wall_now;
        last_loop_instant = now_instant;

        let locked_now = logind.is_locked();

        // Check for logind listener thread death (warn once)
        if !logind_warned && logind.has_thread_error() {
            eprintln!(
                "[watcher] Warning: logind listener thread died, lock/suspend detection may be degraded"
            );
            logind_warned = true;
        }

        if locked_now && !is_locked {
            if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                let input = input_stats.snapshot();
                let jiggler = input_stats.jiggler_detected();
                flush_session(
                    &flush_ctx,
                    Some(info),
                    &mut SessionAccum {
                        focus_start: &mut focus_start,
                        active_ms: &mut accumulated_active_ms,
                        passive_ms: &mut accumulated_passive_ms,
                        idle_ms: &mut accumulated_idle_ms,
                        input_baseline_ms: &mut input_baseline_ms,
                        session_start_mono_ms: &mut session_start_mono_ms,
                        input_offsets: &mut input_offsets,
                        last_seen_input_ms: &mut last_seen_input_ms,
                    },
                    &input,
                    jiggler,
                    FlushReset::NoReset,
                )?;
            }
            accumulated_active_ms = 0;
            accumulated_passive_ms = 0;
            accumulated_idle_ms = 0;
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
            session_start_mono_ms = millis_u64(monitor_start.elapsed());
            if !quiet {
                eprintln!(
                    "{} Screen unlocked, resuming tracking",
                    "[UNLOCKED]".green().bold()
                );
            }
        }

        if !is_locked {
            let now_ms = millis_u64(monitor_start.elapsed());
            let last_input_ms = input_stats.last_activity_ms();

            if last_input_ms > last_seen_input_ms {
                let offset = last_input_ms.saturating_sub(session_start_mono_ms);
                if let Ok(offset_u32) = u32::try_from(offset) {
                    input_offsets.push(offset_u32);
                }
                last_seen_input_ms = last_input_ms;
            }

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
                let info = focused_id.and_then(|id| windows.get(&id));
                let (input, jiggler) = if info.is_some() {
                    (input_stats.snapshot(), input_stats.jiggler_detected())
                } else {
                    (
                        InputSnapshot {
                            keystrokes: 0,
                            mouse_clicks: 0,
                            scroll_events: 0,
                            mouse_distance: 0,
                        },
                        false,
                    )
                };
                flush_session(
                    &flush_ctx,
                    info,
                    &mut SessionAccum {
                        focus_start: &mut focus_start,
                        active_ms: &mut accumulated_active_ms,
                        passive_ms: &mut accumulated_passive_ms,
                        idle_ms: &mut accumulated_idle_ms,
                        input_baseline_ms: &mut input_baseline_ms,
                        session_start_mono_ms: &mut session_start_mono_ms,
                        input_offsets: &mut input_offsets,
                        last_seen_input_ms: &mut last_seen_input_ms,
                    },
                    &input,
                    jiggler,
                    FlushReset::PreserveBaselines {
                        new_focus_start: Utc::now(),
                    },
                )?;
                if !quiet {
                    eprintln!(
                        "{} Entering away state after {}s idle, pausing tracking",
                        "[AWAY]".blue().bold(),
                        idle_duration_ms / 1000,
                    );
                }
                last_flush = now_instant;
                // DO NOT reset input_baseline_ms or session_start_mono_ms here.
                // The idle timer must keep measuring from the last real input,
                // otherwise idle_duration_ms resets to ~0 and we immediately exit Away
                // on the next loop iteration — causing a 30-min Away→Active cycle.
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
                i64::try_from(now_instant.duration_since(last_idle_check).as_millis())
                    .unwrap_or(i64::MAX);
            match current_state {
                ActivityState::Active => {
                    accumulated_active_ms =
                        accumulated_active_ms.saturating_add(elapsed_since_last_check)
                }
                ActivityState::Passive => {
                    accumulated_passive_ms =
                        accumulated_passive_ms.saturating_add(elapsed_since_last_check)
                }
                ActivityState::Idle => {
                    accumulated_idle_ms =
                        accumulated_idle_ms.saturating_add(elapsed_since_last_check)
                }
                ActivityState::Locked | ActivityState::Away => {}
            }
            last_idle_check = now_instant;

            if current_state != ActivityState::Away
                && current_state != ActivityState::Idle
                && now_instant.duration_since(last_flush) >= FLUSH_INTERVAL
            {
                if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                    let input = input_stats.snapshot();
                    let jiggler = input_stats.jiggler_detected();
                    let flushed = flush_session(
                        &flush_ctx,
                        Some(info),
                        &mut SessionAccum {
                            focus_start: &mut focus_start,
                            active_ms: &mut accumulated_active_ms,
                            passive_ms: &mut accumulated_passive_ms,
                            idle_ms: &mut accumulated_idle_ms,
                            input_baseline_ms: &mut input_baseline_ms,
                            session_start_mono_ms: &mut session_start_mono_ms,
                            input_offsets: &mut input_offsets,
                            last_seen_input_ms: &mut last_seen_input_ms,
                        },
                        &input,
                        jiggler,
                        FlushReset::PreserveBaselines {
                            new_focus_start: Utc::now(),
                        },
                    )?;
                    if !quiet {
                        let total = flushed
                            .active_ms
                            .saturating_add(flushed.passive_ms)
                            .saturating_add(flushed.idle_ms);
                        eprintln!(
                            "{} {} ({})",
                            "[flush]".dimmed(),
                            info.app_id.cyan(),
                            fmt_duration_compact(total).dimmed()
                        );
                    }
                    // Intentionally preserve input_baseline_ms and session_start_mono_ms:
                    // resetting them here would zero idle_duration_ms every flush cycle,
                    // making the Away threshold unreachable without a focus change.
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
                // Flush current session before returning error (data loss prevention)
                if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                    let input = input_stats.snapshot();
                    let jiggler = input_stats.jiggler_detected();
                    if let Err(e) = flush_session(
                        &flush_ctx,
                        Some(info),
                        &mut SessionAccum {
                            focus_start: &mut focus_start,
                            active_ms: &mut accumulated_active_ms,
                            passive_ms: &mut accumulated_passive_ms,
                            idle_ms: &mut accumulated_idle_ms,
                            input_baseline_ms: &mut input_baseline_ms,
                            session_start_mono_ms: &mut session_start_mono_ms,
                            input_offsets: &mut input_offsets,
                            last_seen_input_ms: &mut last_seen_input_ms,
                        },
                        &input,
                        jiggler,
                        FlushReset::NoReset,
                    ) {
                        eprintln!("[watcher] flush on disconnect failed: {}", e);
                    }
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
                // Flush any existing session before clearing windows
                let input = input_stats.snapshot();
                let jiggler = input_stats.jiggler_detected();
                if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                    flush_session(
                        &flush_ctx,
                        Some(info),
                        &mut SessionAccum {
                            focus_start: &mut focus_start,
                            active_ms: &mut accumulated_active_ms,
                            passive_ms: &mut accumulated_passive_ms,
                            idle_ms: &mut accumulated_idle_ms,
                            input_baseline_ms: &mut input_baseline_ms,
                            session_start_mono_ms: &mut session_start_mono_ms,
                            input_offsets: &mut input_offsets,
                            last_seen_input_ms: &mut last_seen_input_ms,
                        },
                        &input,
                        jiggler,
                        FlushReset::NoReset,
                    )?;
                }
                // Unconditionally reset accumulators after flush to prevent
                // double-counting if no window in the new list is focused.
                accumulated_active_ms = 0;
                accumulated_passive_ms = 0;
                accumulated_idle_ms = 0;
                focused_id = None;
                windows.clear();
                for w in &win_list {
                    windows.insert(w.id, WindowInfo::from(w));
                    if w.is_focused {
                        focused_id = Some(w.id);
                        focus_start = now;
                        last_idle_check = now_instant;
                        last_flush = now_instant;
                        input_baseline_ms = input_stats.last_activity_ms();
                        session_start_mono_ms = millis_u64(monitor_start.elapsed());
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
                // Flush session if the closed window was focused (data loss prevention)
                if focused_id == Some(id) {
                    let input = input_stats.snapshot();
                    let jiggler = input_stats.jiggler_detected();
                    if let Some(info) = windows.get(&id) {
                        flush_session(
                            &flush_ctx,
                            Some(info),
                            &mut SessionAccum {
                                focus_start: &mut focus_start,
                                active_ms: &mut accumulated_active_ms,
                                passive_ms: &mut accumulated_passive_ms,
                                idle_ms: &mut accumulated_idle_ms,
                                input_baseline_ms: &mut input_baseline_ms,
                                session_start_mono_ms: &mut session_start_mono_ms,
                                input_offsets: &mut input_offsets,
                                last_seen_input_ms: &mut last_seen_input_ms,
                            },
                            &input,
                            jiggler,
                            FlushReset::NoReset,
                        )?;
                    }
                    focused_id = None;
                    accumulated_active_ms = 0;
                    accumulated_passive_ms = 0;
                    accumulated_idle_ms = 0;
                }
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
                    session_start_mono_ms = millis_u64(monitor_start.elapsed());
                    continue;
                }
                let input = input_stats.snapshot();
                let jiggler = input_stats.jiggler_detected();

                if let Some(info) = focused_id.and_then(|id| windows.get(&id)) {
                    let flushed = flush_session(
                        &flush_ctx,
                        Some(info),
                        &mut SessionAccum {
                            focus_start: &mut focus_start,
                            active_ms: &mut accumulated_active_ms,
                            passive_ms: &mut accumulated_passive_ms,
                            idle_ms: &mut accumulated_idle_ms,
                            input_baseline_ms: &mut input_baseline_ms,
                            session_start_mono_ms: &mut session_start_mono_ms,
                            input_offsets: &mut input_offsets,
                            last_seen_input_ms: &mut last_seen_input_ms,
                        },
                        &input,
                        jiggler,
                        FlushReset::NoReset,
                    )?;

                    let total = flushed
                        .active_ms
                        .saturating_add(flushed.passive_ms)
                        .saturating_add(flushed.idle_ms);

                    let category = config.classify(&info.app_id, &info.title);
                    if category == Category::Neutral
                        && !config.categories.contains_key(&info.app_id)
                        && logged_untracked.insert(info.app_id.clone())
                        && let Ok(mut file) = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&untracked_log_path)
                        && let Err(e) = writeln!(file, "{}", info.app_id)
                    {
                        eprintln!("Warning: failed to write to untracked log: {}", e);
                    }

                    if !quiet && total >= 500 {
                        let idle_pct = if total > 0 {
                            ((flushed.passive_ms.saturating_add(flushed.idle_ms)) as f64
                                / total as f64
                                * 100.0)
                                .round() as u32
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
                session_start_mono_ms = millis_u64(monitor_start.elapsed());
            }

            _ => {}
        }
    }
}
