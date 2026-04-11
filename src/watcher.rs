use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use backon::{BlockingRetryable, ExponentialBuilder};
use chrono::{Local, Utc};
use niri_ipc::socket::Socket;
use niri_ipc::{Event, Request, Response, Window};
use owo_colors::OwoColorize;
use rusqlite::Connection;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::agent_activity::AgentMonitor;
use crate::config::{Category, Config, get_config_path, get_data_dir, load_config};
use crate::db::{SessionSnapshot, init_db, insert_event, reclassify_all, run_migrations};
use crate::error::Error;
use crate::fmt::{cat_colored, cat_label, fmt_duration_compact, truncate};
use crate::input::{InputSnapshot, start_idle_monitor};
use crate::logind::start_logind_monitor;
use crate::scheduler::check_scheduled_reports;

// Duration constants
const FLUSH_INTERVAL_SECS: u64 = 300; // 5 minutes
const HEARTBEAT_CHECK_INTERVAL_SECS: u64 = 30;

// Niri connection backoff constants (separate from flush/suspend semantics)
const NIRI_BACKOFF_MAX_INTERVAL_SECS: u64 = 300;

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
            tracing::debug!(
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
            app_id: w.app_id.as_deref().unwrap_or("unknown").to_owned(),
            title: w.title.as_deref().unwrap_or_default().to_owned(),
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
#[allow(clippy::struct_field_names)] // `_ms` suffix denotes unit
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

/// Mutable state for the watch loop, grouped for cleaner handler signatures.
struct WatchState {
    windows: HashMap<u64, WindowInfo>,
    focused_id: Option<u64>,
    focus_start: chrono::DateTime<chrono::Utc>,
    accumulated_active_ms: i64,
    accumulated_passive_ms: i64,
    accumulated_idle_ms: i64,
    last_idle_check: Instant,
    last_flush: Instant,
    is_locked: bool,
    current_state: ActivityState,
    last_wall_time: chrono::DateTime<chrono::Utc>,
    last_loop_instant: Instant,
    input_baseline_ms: u64,
    session_start_mono_ms: u64,
    logind_warned: bool,
    input_offsets: Vec<u32>,
    last_seen_input_ms: u64,
    last_heartbeat_value: u64,
    last_heartbeat_check: Instant,
    input_thread_warned: bool,
    last_schedule_check: Instant,
}

impl WatchState {
    fn new(input_baseline_ms: u64, session_start_mono_ms: u64) -> Self {
        let now_instant = Instant::now();
        Self {
            windows: HashMap::new(),
            focused_id: None,
            focus_start: Utc::now(),
            accumulated_active_ms: 0,
            accumulated_passive_ms: 0,
            accumulated_idle_ms: 0,
            last_idle_check: now_instant,
            last_flush: now_instant,
            is_locked: false,
            current_state: ActivityState::Active,
            last_wall_time: Utc::now(),
            last_loop_instant: now_instant,
            input_baseline_ms,
            session_start_mono_ms,
            logind_warned: false,
            input_offsets: Vec::new(),
            last_seen_input_ms: input_baseline_ms,
            last_heartbeat_value: 0,
            last_heartbeat_check: now_instant,
            input_thread_warned: false,
            last_schedule_check: now_instant,
        }
    }

    fn make_accum(&mut self) -> SessionAccum<'_> {
        SessionAccum {
            focus_start: &mut self.focus_start,
            active_ms: &mut self.accumulated_active_ms,
            passive_ms: &mut self.accumulated_passive_ms,
            idle_ms: &mut self.accumulated_idle_ms,
            input_baseline_ms: &mut self.input_baseline_ms,
            session_start_mono_ms: &mut self.session_start_mono_ms,
            input_offsets: &mut self.input_offsets,
            last_seen_input_ms: &mut self.last_seen_input_ms,
        }
    }

    fn reset_accumulators(&mut self) {
        self.accumulated_active_ms = 0;
        self.accumulated_passive_ms = 0;
        self.accumulated_idle_ms = 0;
    }

    fn reset_session(&mut self, now_instant: Instant, input_baseline: u64, session_mono: u64) {
        self.focus_start = Utc::now();
        self.reset_accumulators();
        self.last_idle_check = now_instant;
        self.last_flush = now_instant;
        self.input_baseline_ms = input_baseline;
        self.session_start_mono_ms = session_mono;
        self.last_seen_input_ms = input_baseline;
        self.input_offsets.clear();
    }
}

/// Result of shutdown handler: true = should exit, false = continue loop.
fn handle_shutdown(
    shutdown: &AtomicBool,
    state: &mut WatchState,
    flush_ctx: &FlushContext<'_>,
    input_stats: &crate::input::InputStats,
) -> Result<bool, Error> {
    if !shutdown.load(Ordering::SeqCst) {
        return Ok(false);
    }

    tracing::info!("Shutdown signal received, flushing current session...");
    let info = state
        .focused_id
        .and_then(|id| state.windows.get(&id))
        .cloned();
    if let Some(ref info) = info {
        tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_shutdown");
        let input = input_stats.snapshot();
        let jiggler = input_stats.jiggler_detected();
        let flushed = flush_session(
            flush_ctx,
            Some(info),
            &mut state.make_accum(),
            &input,
            jiggler,
            FlushReset::NoReset,
        )?;
        let total = flushed
            .active_ms
            .saturating_add(flushed.passive_ms)
            .saturating_add(flushed.idle_ms);
        tracing::info!(
            "Flushed: {} ({}ms active, {}ms passive, {}ms idle)",
            info.app_id,
            flushed.active_ms,
            flushed.passive_ms,
            flushed.idle_ms
        );
        tracing::info!("Total session time saved: {}ms", total);
    }
    tracing::info!("Graceful shutdown complete.");
    Ok(true)
}

/// Handle suspend/resume detection via wall-clock jump or D-Bus signal.
/// Returns true if resume was detected and handled.
fn handle_suspend_resume(
    state: &mut WatchState,
    logind: &crate::logind::LogindMonitor,
    flush_ctx: &FlushContext<'_>,
    input_stats: &crate::input::InputStats,
    monitor_start: Instant,
    now_instant: Instant,
    quiet: bool,
) -> Result<bool, Error> {
    const SUSPEND_JUMP_THRESHOLD_SECS: i64 = 30;

    let wall_now = Utc::now();
    let wall_elapsed_secs = (wall_now - state.last_wall_time).num_seconds();
    let mono_elapsed_secs = i64::try_from(
        now_instant
            .duration_since(state.last_loop_instant)
            .as_secs(),
    )
    .unwrap_or(i64::MAX);
    let time_jump_secs = wall_elapsed_secs.saturating_sub(mono_elapsed_secs);

    let wall_clock_resume = time_jump_secs > SUSPEND_JUMP_THRESHOLD_SECS;
    let dbus_resume_signalled = logind.take_suspend_resumed();
    let dbus_resume = dbus_resume_signalled && !wall_clock_resume;

    state.last_wall_time = wall_now;
    state.last_loop_instant = now_instant;

    if !wall_clock_resume && !dbus_resume {
        return Ok(false);
    }

    tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_suspend_resume");

    if !quiet {
        let label = if wall_clock_resume {
            format!(
                "System resume detected ({}s wall clock, {}s monotonic, {}s jump)",
                wall_elapsed_secs, mono_elapsed_secs, time_jump_secs,
            )
        } else {
            "System resume detected via D-Bus signal".to_string()
        };
        tracing::info!("{} {}", "[SUSPEND]".blue().bold(), label);
    }

    let has_data = state.accumulated_active_ms > 0
        || state.accumulated_passive_ms > 0
        || state.accumulated_idle_ms > 0;
    let info = if has_data {
        state
            .focused_id
            .and_then(|id| state.windows.get(&id))
            .cloned()
    } else {
        None
    };
    let input = input_stats.snapshot();
    let jiggler = input_stats.jiggler_detected();

    flush_session(
        flush_ctx,
        info.as_ref(),
        &mut state.make_accum(),
        &input,
        jiggler,
        FlushReset::Full {
            new_focus_start: Utc::now(),
            new_baseline_ms: input_stats.last_activity_ms(),
            new_session_mono_ms: millis_u64(monitor_start.elapsed()),
        },
    )?;

    state.current_state = ActivityState::Active;
    state.last_idle_check = now_instant;
    state.last_flush = now_instant;

    Ok(true)
}

/// Handle lock/unlock state transitions. Returns true if state changed.
fn handle_lock_unlock(
    state: &mut WatchState,
    logind: &crate::logind::LogindMonitor,
    flush_ctx: &FlushContext<'_>,
    input_stats: &crate::input::InputStats,
    monitor_start: Instant,
    now_instant: Instant,
    quiet: bool,
) -> Result<bool, Error> {
    let locked_now = logind.is_locked();

    if !state.logind_warned && logind.has_thread_error() {
        tracing::warn!("logind listener thread died, lock/suspend detection may be degraded");
        state.logind_warned = true;
    }

    // Transition: unlocked → locked
    if locked_now && !state.is_locked {
        let info = state
            .focused_id
            .and_then(|id| state.windows.get(&id))
            .cloned();
        if let Some(ref info) = info {
            tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_lock_unlock (lock)");
            let input = input_stats.snapshot();
            let jiggler = input_stats.jiggler_detected();
            flush_session(
                flush_ctx,
                Some(info),
                &mut state.make_accum(),
                &input,
                jiggler,
                FlushReset::NoReset,
            )?;
            // Zero accumulators after flush to prevent double-counting
            state.reset_accumulators();
        }
        state.is_locked = true;
        state.current_state = ActivityState::Locked;
        if !quiet {
            tracing::info!(
                "{} Screen locked, pausing tracking",
                "[LOCKED]".magenta().bold()
            );
        }
        return Ok(true);
    }

    // Transition: locked → unlocked
    if !locked_now && state.is_locked {
        state.is_locked = false;
        state.current_state = ActivityState::Active;
        state.reset_session(
            now_instant,
            input_stats.last_activity_ms(),
            millis_u64(monitor_start.elapsed()),
        );
        if !quiet {
            tracing::info!(
                "{} Screen unlocked, resuming tracking",
                "[UNLOCKED]".green().bold()
            );
        }
        return Ok(true);
    }

    Ok(false)
}

/// Check input thread heartbeat for stalls.
fn check_input_heartbeat(
    state: &mut WatchState,
    input_stats: &crate::input::InputStats,
    now_instant: Instant,
) {
    if now_instant.duration_since(state.last_heartbeat_check)
        < Duration::from_secs(HEARTBEAT_CHECK_INTERVAL_SECS)
    {
        return;
    }

    let current_hb = input_stats.heartbeat();
    if current_hb == state.last_heartbeat_value && current_hb > 0 && !state.input_thread_warned {
        tracing::warn!(
            "input-poll thread heartbeat stalled at {} — input tracking may be degraded",
            current_hb,
        );
        state.input_thread_warned = true;
    } else if current_hb != state.last_heartbeat_value && state.input_thread_warned {
        tracing::info!("input-poll thread heartbeat resumed");
        state.input_thread_warned = false;
    }
    state.last_heartbeat_value = current_hb;
    state.last_heartbeat_check = now_instant;
}

/// Idle state machine thresholds, computed once from config.
#[allow(clippy::struct_field_names)]
struct IdleThresholds {
    idle_ms: u64,
    deep_idle_ms: u64,
    away_ms: u64,
}

/// Handle idle state transitions (Active→Passive→Idle→Away) and periodic flush.
#[allow(clippy::too_many_arguments)]
fn handle_idle_transitions(
    state: &mut WatchState,
    flush_ctx: &FlushContext<'_>,
    input_stats: &crate::input::InputStats,
    agent_monitor: &mut AgentMonitor,
    thresholds: &IdleThresholds,
    monitor_start: Instant,
    now_instant: Instant,
    quiet: bool,
) -> Result<(), Error> {
    if state.is_locked {
        return Ok(());
    }

    let now_ms = millis_u64(monitor_start.elapsed());
    let last_input_ms = input_stats.last_activity_ms();

    if last_input_ms > state.last_seen_input_ms {
        let offset = last_input_ms.saturating_sub(state.session_start_mono_ms);
        if let Ok(offset_u32) = u32::try_from(offset) {
            state.input_offsets.push(offset_u32);
        }
        state.last_seen_input_ms = last_input_ms;
    }

    let idle_duration_ms = if last_input_ms > state.input_baseline_ms {
        now_ms.saturating_sub(last_input_ms)
    } else {
        now_ms.saturating_sub(state.session_start_mono_ms)
    };

    let agent_active = agent_monitor.is_active(idle_duration_ms);
    let new_state = compute_activity_state(idle_duration_ms, agent_active, thresholds);

    if new_state == ActivityState::Away && state.current_state != ActivityState::Away {
        handle_enter_away(
            state,
            flush_ctx,
            input_stats,
            now_instant,
            idle_duration_ms,
            quiet,
        )?;
    }

    if state.current_state == ActivityState::Away && new_state != ActivityState::Away {
        handle_exit_away(
            state,
            input_stats,
            monitor_start,
            now_instant,
            now_ms,
            quiet,
        );
    }

    if new_state != state.current_state && !quiet {
        let from = color_state(state.current_state);
        let to = color_state(new_state);
        tracing::debug!("{} {} → {}", "[STATE]".dimmed(), from, to);
    }
    accumulate_state_time(state, now_instant);

    state.current_state = new_state;

    if should_periodic_flush(state, now_instant) {
        handle_periodic_flush(state, flush_ctx, input_stats, now_instant, quiet)?;
    }

    Ok(())
}

fn compute_activity_state(
    idle_duration_ms: u64,
    agent_active: bool,
    thresholds: &IdleThresholds,
) -> ActivityState {
    if idle_duration_ms > thresholds.away_ms {
        ActivityState::Away
    } else if agent_active {
        ActivityState::Active
    } else if idle_duration_ms > thresholds.deep_idle_ms {
        ActivityState::Idle
    } else if idle_duration_ms > thresholds.idle_ms {
        ActivityState::Passive
    } else {
        ActivityState::Active
    }
}

fn handle_enter_away(
    state: &mut WatchState,
    flush_ctx: &FlushContext<'_>,
    input_stats: &crate::input::InputStats,
    now_instant: Instant,
    idle_duration_ms: u64,
    quiet: bool,
) -> Result<(), Error> {
    let info = state
        .focused_id
        .and_then(|id| state.windows.get(&id))
        .cloned();
    let (input, jiggler) = if info.is_some() {
        tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_enter_away");
        (input_stats.snapshot(), input_stats.jiggler_detected())
    } else {
        (InputSnapshot::default(), false)
    };
    flush_session(
        flush_ctx,
        info.as_ref(),
        &mut state.make_accum(),
        &input,
        jiggler,
        FlushReset::PreserveBaselines {
            new_focus_start: Utc::now(),
        },
    )?;
    if !quiet {
        tracing::info!(
            "{} Entering away state after {}s idle, pausing tracking",
            "[AWAY]".blue().bold(),
            idle_duration_ms / 1000,
        );
    }
    state.last_flush = now_instant;
    Ok(())
}

fn handle_exit_away(
    state: &mut WatchState,
    input_stats: &crate::input::InputStats,
    _monitor_start: Instant,
    now_instant: Instant,
    now_ms: u64,
    quiet: bool,
) {
    if !quiet {
        tracing::info!(
            "{} Resuming tracking (user activity detected)",
            "[RESUMED]".green().bold(),
        );
    }
    state.focus_start = Utc::now();
    state.reset_accumulators();
    state.last_idle_check = now_instant;
    state.last_flush = now_instant;
    state.input_baseline_ms = input_stats.last_activity_ms();
    state.session_start_mono_ms = now_ms;
}

fn accumulate_state_time(state: &mut WatchState, now_instant: Instant) {
    let elapsed_since_last_check = i64::try_from(
        now_instant
            .duration_since(state.last_idle_check)
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    match state.current_state {
        ActivityState::Active => {
            state.accumulated_active_ms = state
                .accumulated_active_ms
                .saturating_add(elapsed_since_last_check);
        }
        ActivityState::Passive => {
            state.accumulated_passive_ms = state
                .accumulated_passive_ms
                .saturating_add(elapsed_since_last_check);
        }
        ActivityState::Idle => {
            state.accumulated_idle_ms = state
                .accumulated_idle_ms
                .saturating_add(elapsed_since_last_check);
        }
        ActivityState::Locked | ActivityState::Away => {}
    }
    state.last_idle_check = now_instant;
}

fn should_periodic_flush(state: &WatchState, now_instant: Instant) -> bool {
    state.current_state != ActivityState::Away
        && state.current_state != ActivityState::Idle
        && now_instant.duration_since(state.last_flush) >= Duration::from_secs(FLUSH_INTERVAL_SECS)
}

fn handle_periodic_flush(
    state: &mut WatchState,
    flush_ctx: &FlushContext<'_>,
    input_stats: &crate::input::InputStats,
    now_instant: Instant,
    quiet: bool,
) -> Result<(), Error> {
    let info = state
        .focused_id
        .and_then(|id| state.windows.get(&id))
        .cloned();
    if let Some(ref info) = info {
        tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_periodic_flush");
        let input = input_stats.snapshot();
        let jiggler = input_stats.jiggler_detected();
        let flushed = flush_session(
            flush_ctx,
            Some(info),
            &mut state.make_accum(),
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
            tracing::debug!(
                "{} {} ({})",
                "[flush]".dimmed(),
                info.app_id.cyan(),
                fmt_duration_compact(total).dimmed()
            );
        }
    }
    state.last_flush = now_instant;
    Ok(())
}

/// Context for niri event handling, avoiding repeated parameter passing.
struct NiriEventContext<'a> {
    flush_ctx: &'a FlushContext<'a>,
    input_stats: &'a crate::input::InputStats,
    config: &'a Config,
    monitor_start: Instant,
    now: chrono::DateTime<chrono::Utc>,
    now_instant: Instant,
    quiet: bool,
    untracked_log_path: &'a std::path::Path,
    logged_untracked: &'a mut std::collections::HashSet<String>,
}

fn handle_windows_changed(
    state: &mut WatchState,
    ctx: &NiriEventContext<'_>,
    win_list: Vec<Window>,
) -> Result<(), Error> {
    let info = state
        .focused_id
        .and_then(|id| state.windows.get(&id))
        .cloned();
    if let Some(ref info) = info {
        tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_windows_changed");
        let input = ctx.input_stats.snapshot();
        let jiggler = ctx.input_stats.jiggler_detected();
        flush_session(
            ctx.flush_ctx,
            Some(info),
            &mut state.make_accum(),
            &input,
            jiggler,
            FlushReset::NoReset,
        )?;
    }

    state.reset_accumulators();
    state.focused_id = None;
    state.windows.clear();

    for w in &win_list {
        state.windows.insert(w.id, WindowInfo::from(w));
        if w.is_focused {
            state.focused_id = Some(w.id);
            state.focus_start = ctx.now;
            state.last_idle_check = ctx.now_instant;
            state.last_flush = ctx.now_instant;
            state.input_baseline_ms = ctx.input_stats.last_activity_ms();
            state.session_start_mono_ms = millis_u64(ctx.monitor_start.elapsed());
            state.last_seen_input_ms = state.input_baseline_ms;
            state.input_offsets.clear();
        }
    }

    tracing::debug!(
        "Loaded {} windows, focused: {:?}",
        state.windows.len(),
        state.focused_id
    );
    Ok(())
}

fn handle_window_closed(
    state: &mut WatchState,
    ctx: &NiriEventContext<'_>,
    id: u64,
) -> Result<(), Error> {
    if state.focused_id == Some(id) {
        tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_window_closed");
        let input = ctx.input_stats.snapshot();
        let jiggler = ctx.input_stats.jiggler_detected();
        let info = state.windows.get(&id).cloned();
        if let Some(ref info) = info {
            flush_session(
                ctx.flush_ctx,
                Some(info),
                &mut state.make_accum(),
                &input,
                jiggler,
                FlushReset::NoReset,
            )?;
        }
        state.focused_id = None;
        state.reset_accumulators();
    }
    state.windows.remove(&id);
    Ok(())
}

fn handle_focus_changed(
    state: &mut WatchState,
    ctx: &mut NiriEventContext<'_>,
    new_focus_id: Option<u64>,
) -> Result<(), Error> {
    if state.is_locked {
        state.focused_id = new_focus_id;
        state.focus_start = ctx.now;
        state.reset_accumulators();
        state.last_idle_check = ctx.now_instant;
        state.last_flush = ctx.now_instant;
        state.input_baseline_ms = ctx.input_stats.last_activity_ms();
        state.session_start_mono_ms = millis_u64(ctx.monitor_start.elapsed());
        return Ok(());
    }

    tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_focus_changed");
    let input = ctx.input_stats.snapshot();
    let jiggler = ctx.input_stats.jiggler_detected();
    let focus_start = state.focus_start;

    let info = state
        .focused_id
        .and_then(|id| state.windows.get(&id))
        .cloned();
    if let Some(ref info) = info {
        let flushed = flush_session(
            ctx.flush_ctx,
            Some(info),
            &mut state.make_accum(),
            &input,
            jiggler,
            FlushReset::NoReset,
        )?;

        log_untracked_app(info, ctx);
        print_focus_change_status(info, &flushed, &input, jiggler, focus_start, ctx);
    }

    state.focused_id = new_focus_id;
    state.focus_start = ctx.now;
    state.reset_accumulators();
    state.current_state = ActivityState::Active;
    state.last_idle_check = ctx.now_instant;
    state.last_flush = ctx.now_instant;
    state.input_baseline_ms = ctx.input_stats.last_activity_ms();
    state.session_start_mono_ms = millis_u64(ctx.monitor_start.elapsed());
    state.last_seen_input_ms = state.input_baseline_ms;
    state.input_offsets.clear();

    Ok(())
}

fn log_untracked_app(info: &WindowInfo, ctx: &mut NiriEventContext<'_>) {
    use std::fs::OpenOptions;
    use std::io::Write;

    let category = ctx.config.classify(&info.app_id, &info.title);
    if category == Category::Neutral
        && !ctx.config.categories.contains_key(&info.app_id)
        && ctx.logged_untracked.insert(info.app_id.clone())
        && let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(ctx.untracked_log_path)
        && let Err(e) = writeln!(file, "{}", info.app_id)
    {
        tracing::warn!("failed to write to untracked log: {}", e);
    }
}

fn print_focus_change_status(
    info: &WindowInfo,
    flushed: &FlushResult,
    input: &InputSnapshot,
    jiggler: bool,
    focus_start: chrono::DateTime<chrono::Utc>,
    ctx: &NiriEventContext<'_>,
) {
    let total = flushed
        .active_ms
        .saturating_add(flushed.passive_ms)
        .saturating_add(flushed.idle_ms);

    if ctx.quiet || total < 500 {
        return;
    }

    let category = ctx.config.classify(&info.app_id, &info.title);
    let idle_pct = if total > 0 {
        ((flushed.passive_ms.saturating_add(flushed.idle_ms)) as f64 / total as f64 * 100.0).round()
            as u32
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
        format!("[{}]", focus_start.with_timezone(&Local).format("%H:%M:%S")).dimmed(),
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

fn flush_on_disconnect(
    state: &mut WatchState,
    flush_ctx: &FlushContext<'_>,
    input_stats: &crate::input::InputStats,
) {
    let info = state
        .focused_id
        .and_then(|id| state.windows.get(&id))
        .cloned();
    if let Some(ref info) = info {
        tracing::debug!(target: "input_debug", "SNAPSHOT caller: flush_on_disconnect");
        let input = input_stats.snapshot();
        let jiggler = input_stats.jiggler_detected();
        if let Err(e) = flush_session(
            flush_ctx,
            Some(info),
            &mut state.make_accum(),
            &input,
            jiggler,
            FlushReset::NoReset,
        ) {
            tracing::warn!("flush on disconnect failed: {}", e);
        }
    }
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

/// Connect to the niri window manager with exponential backoff retry logic.
pub fn connect_to_niri() -> Result<Socket, Error> {
    let backoff = ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(100))
        .with_max_delay(Duration::from_secs(NIRI_BACKOFF_MAX_INTERVAL_SECS))
        .with_max_times(20); // ~5 minutes with exponential growth

    (|| {
        Socket::connect().map_err(|e| {
            tracing::warn!("Connection to niri failed: {}. Retrying...", e);
            e
        })
    })
    .retry(backoff)
    .sleep(std::thread::sleep)
    .call()
    .map_err(|_| Error::NiriConnectionFailed)
}

/// Monitor window focus and activity state, recording events to the database.
pub fn watch(quiet: bool) -> Result<(), Error> {
    use std::collections::HashSet;

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
    let mut agent_monitor = AgentMonitor::new(&config.agent_activity);
    if config.agent_activity.enabled {
        println!(
            "Agent activity detection: enabled ({}s window, {} build processes)",
            config.agent_activity.activity_window_secs,
            agent_monitor.process_whitelist_count()
        );
    }

    let thresholds = IdleThresholds {
        idle_ms: config.idle_threshold_secs.saturating_mul(1000),
        deep_idle_ms: config.deep_idle_secs.saturating_mul(1000),
        away_ms: config.away_threshold_secs.saturating_mul(1000),
    };
    let input_active_ms = config.input_active_secs.saturating_mul(1000);

    let mut socket = connect_to_niri()?;
    let reply = socket.send(Request::EventStream)?;
    match reply {
        Ok(Response::Handled) => {}
        Ok(_other) => {
            return Err(Error::UnexpectedResponse);
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

    let mut state = WatchState::new(
        input_stats.last_activity_ms(),
        millis_u64(monitor_start.elapsed()),
    );

    let flush_ctx = FlushContext {
        conn: &conn,
        config: &config,
        input_active_ms,
        quiet,
    };

    println!("\nWatching window focus (event-driven)...");
    println!("Press Ctrl+C to stop gracefully\n");

    loop {
        if handle_shutdown(&shutdown, &mut state, &flush_ctx, &input_stats)? {
            return Ok(());
        }

        let now_instant = Instant::now();

        handle_suspend_resume(
            &mut state,
            &logind,
            &flush_ctx,
            &input_stats,
            monitor_start,
            now_instant,
            quiet,
        )?;

        check_input_heartbeat(&mut state, &input_stats, now_instant);

        handle_lock_unlock(
            &mut state,
            &logind,
            &flush_ctx,
            &input_stats,
            monitor_start,
            now_instant,
            quiet,
        )?;

        handle_idle_transitions(
            &mut state,
            &flush_ctx,
            &input_stats,
            &mut agent_monitor,
            &thresholds,
            monitor_start,
            now_instant,
            quiet,
        )?;

        // Check scheduled reports every 5 minutes (piggybacks on flush interval)
        if now_instant.duration_since(state.last_schedule_check)
            >= Duration::from_secs(FLUSH_INTERVAL_SECS)
        {
            state.last_schedule_check = now_instant;
            check_scheduled_reports(&conn, &config, quiet);
        }

        let event = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                if shutdown.load(Ordering::SeqCst) {
                    continue;
                }
                flush_on_disconnect(&mut state, &flush_ctx, &input_stats);
                return Err(Error::NiriEventStreamClosed);
            }
        };

        let Some(event) = event else {
            continue;
        };

        let now = Utc::now();
        let mut niri_ctx = NiriEventContext {
            flush_ctx: &flush_ctx,
            input_stats: &input_stats,
            config: &config,
            monitor_start,
            now,
            now_instant,
            quiet,
            untracked_log_path: &untracked_log_path,
            logged_untracked: &mut logged_untracked,
        };

        match event {
            Event::WindowsChanged { windows: win_list } => {
                handle_windows_changed(&mut state, &niri_ctx, win_list)?;
            }
            Event::WindowOpenedOrChanged { window } => {
                state.windows.insert(window.id, WindowInfo::from(&window));
            }
            Event::WindowClosed { id } => {
                handle_window_closed(&mut state, &niri_ctx, id)?;
            }
            Event::WindowFocusChanged { id: new_focus_id } => {
                handle_focus_changed(&mut state, &mut niri_ctx, new_focus_id)?;
            }
            _ => {}
        }
    }
}
