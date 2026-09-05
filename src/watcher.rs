use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, thread};

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
use crate::project;
use crate::scheduler::{Scheduler, check_scheduled_reports};

// Duration constants
const FLUSH_INTERVAL_SECS: u64 = 300; // 5 minutes
const HEARTBEAT_CHECK_INTERVAL_SECS: u64 = 30;
// Minimum spacing between title-only session flushes. Animated terminal titles
// (spinner frames, clocks, progress) can change ~1/second; without this
// debounce each change wrote a new events row, the production "flush storm"
// that produced ~86k junk rows/day. App switches are never debounced.
const TITLE_FLUSH_DEBOUNCE_SECS: u64 = 60;

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
        && input.scroll_events == 0
        && input.qualifying_mouse_movements == 0
        && u64::try_from(*active_ms).is_ok_and(|ms| ms >= input_active_ms)
    {
        if !quiet {
            tracing::debug!(
                "{} Reclassifying {}ms active → passive (no qualifying input)",
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
    pub pid: Option<i32>,
}

impl From<&Window> for WindowInfo {
    fn from(w: &Window) -> Self {
        Self {
            app_id: w.app_id.as_deref().unwrap_or("unknown").to_owned(),
            title: w.title.as_deref().unwrap_or_default().to_owned(),
            pid: w.pid,
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
    agent_ms: &'a mut i64,
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
    accumulated_agent_ms: i64,
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
    /// Instant of the most recent title-only session flush, for debouncing
    /// animated-title churn (see TITLE_FLUSH_DEBOUNCE_SECS).
    last_title_flush: Instant,
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
            accumulated_agent_ms: 0,
            last_idle_check: now_instant,
            last_flush: now_instant,
            is_locked: false,
            current_state: ActivityState::Active,
            last_wall_time: Utc::now(),
            last_loop_instant: now_instant,
            input_baseline_ms,
            session_start_mono_ms,
            logind_warned: false,
            // The watcher deliberately starts a fresh session as Active.
            // Persist that decision so report reconstruction has a defensible
            // presence point even when the first real input arrives later.
            input_offsets: vec![0],
            last_seen_input_ms: input_baseline_ms,
            last_heartbeat_value: 0,
            last_heartbeat_check: now_instant,
            input_thread_warned: false,
            last_schedule_check: now_instant,
            last_title_flush: now_instant,
        }
    }

    fn make_accum(&mut self) -> SessionAccum<'_> {
        SessionAccum {
            focus_start: &mut self.focus_start,
            active_ms: &mut self.accumulated_active_ms,
            passive_ms: &mut self.accumulated_passive_ms,
            idle_ms: &mut self.accumulated_idle_ms,
            agent_ms: &mut self.accumulated_agent_ms,
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
        self.accumulated_agent_ms = 0;
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
        self.input_offsets.push(0);
    }

    /// Begin a new persisted event segment without changing human-presence
    /// state. Used for title-only rollovers: a title is metadata, not input.
    fn start_new_event_segment(
        &mut self,
        new_focus_start: chrono::DateTime<chrono::Utc>,
        now_instant: Instant,
        session_start_mono_ms: u64,
    ) {
        self.focus_start = new_focus_start;
        self.reset_accumulators();
        self.last_flush = now_instant;
        self.session_start_mono_ms = session_start_mono_ms;
        self.input_offsets.clear();
    }

    fn resume_after_away(
        &mut self,
        now_instant: Instant,
        input_baseline_ms: u64,
        session_start_mono_ms: u64,
    ) {
        self.focus_start = Utc::now();
        self.reset_accumulators();
        self.last_idle_check = now_instant;
        self.last_flush = now_instant;
        self.input_baseline_ms = input_baseline_ms;
        self.session_start_mono_ms = session_start_mono_ms;
        self.last_seen_input_ms = input_baseline_ms;
        self.input_offsets.clear();
        self.input_offsets.push(0);
    }
}

/// Result of shutdown handler: true = should exit, false = continue loop.
fn handle_shutdown(
    shutdown: &AtomicBool,
    state: &mut WatchState,
    flush_ctx: &FlushContext<'_>,
    input_stats: &crate::input::InputStats,
    agent_monitor: &mut AgentMonitor,
    monitor_start: Instant,
    now_instant: Instant,
) -> Result<bool, Error> {
    if !shutdown.load(Ordering::SeqCst) {
        return Ok(false);
    }

    tracing::info!("Shutdown signal received, flushing current session...");
    let agent_active = detect_agent_activity(state, input_stats, agent_monitor, monitor_start);
    charge_boundary_elapsed(state, now_instant, agent_active);
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
    agent_monitor: &mut AgentMonitor,
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

    let agent_active = detect_agent_activity(state, input_stats, agent_monitor, monitor_start);
    charge_boundary_elapsed(state, now_instant, agent_active);

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
    agent_monitor: &mut AgentMonitor,
    monitor_start: Instant,
    now_instant: Instant,
    quiet: bool,
) -> Result<bool, Error> {
    let locked_now = logind.is_locked();

    let logind_degraded = logind.is_degraded();
    if logind_degraded && !state.logind_warned {
        tracing::warn!("logind lock monitoring degraded; fail-closed locked state is active");
        state.logind_warned = true;
    } else if !logind_degraded && state.logind_warned {
        tracing::info!("logind lock monitoring recovered");
        state.logind_warned = false;
    }

    // Transition: unlocked → locked
    if locked_now && !state.is_locked {
        let agent_active = detect_agent_activity(state, input_stats, agent_monitor, monitor_start);
        charge_boundary_elapsed(state, now_instant, agent_active);
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
        }
        // End the pre-lock segment even when there is no focused window.
        // The boundary interval has already been charged, and carrying it
        // through unlock would either replay or misattribute it.
        state.reset_accumulators();
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

    // Both values share the monitor-wide monotonic clock. Session/focus/title
    // boundaries are metadata changes, not human input, and must never reset
    // the presence clock (an animated title otherwise prevents Away
    // indefinitely).
    let idle_duration_ms = now_ms.saturating_sub(last_input_ms);

    let focused_title = state
        .focused_id
        .and_then(|id| state.windows.get(&id))
        .map(|info| info.title.as_str());
    let agent_active = agent_monitor.is_active(idle_duration_ms, focused_title);
    let new_state = compute_activity_state(idle_duration_ms, agent_active, thresholds);

    if new_state == ActivityState::Away && state.current_state != ActivityState::Away {
        handle_enter_away(
            state,
            flush_ctx,
            input_stats,
            now_instant,
            idle_duration_ms,
            agent_active,
            quiet,
        )?;
    }

    if state.current_state == ActivityState::Away && new_state != ActivityState::Away {
        handle_exit_away(state, input_stats, now_instant, now_ms, quiet);
    }

    if new_state != state.current_state && !quiet {
        let from = color_state(state.current_state);
        let to = color_state(new_state);
        tracing::debug!("{} {} → {}", "[STATE]".dimmed(), from, to);
    }
    accumulate_state_time(state, now_instant, agent_active);

    state.current_state = new_state;

    if should_periodic_flush(state, now_instant) {
        handle_periodic_flush(state, flush_ctx, input_stats, now_ms, now_instant, quiet)?;
    }

    Ok(())
}

fn compute_activity_state(
    idle_duration_ms: u64,
    agent_active: bool,
    thresholds: &IdleThresholds,
) -> ActivityState {
    // Human presence takes priority: once the user has been away past the
    // threshold, agent activity alone must NOT keep the session "present".
    // Agent work is still recorded separately as `agent_ms`; it just cannot
    // hold the human-presence state past away_ms of no human input. This
    // prevents an overnight coding agent from logging the machine as active
    // while the user is asleep (see docs/SLEEP_DETECTION.lean, bug B1).
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
    agent_active: bool,
    quiet: bool,
) -> Result<(), Error> {
    charge_boundary_elapsed(state, now_instant, agent_active);
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
    state.resume_after_away(now_instant, input_stats.last_activity_ms(), now_ms);
}

fn detect_agent_activity(
    state: &WatchState,
    input_stats: &crate::input::InputStats,
    agent_monitor: &mut AgentMonitor,
    monitor_start: Instant,
) -> bool {
    let now_ms = millis_u64(monitor_start.elapsed());
    let idle_duration_ms = now_ms.saturating_sub(input_stats.last_activity_ms());
    let focused_title = state
        .focused_id
        .and_then(|id| state.windows.get(&id))
        .map(|info| info.title.as_str());
    agent_monitor.is_active(idle_duration_ms, focused_title)
}

/// Close the monotonic accounting interval immediately before a boundary
/// flush/reset. Updating `last_idle_check` makes subsequent maintenance in the
/// same loop iteration a zero-length interval, so the boundary is conserved
/// exactly once.
fn charge_boundary_elapsed(state: &mut WatchState, now_instant: Instant, agent_active: bool) {
    accumulate_state_time(state, now_instant, agent_active);
}

fn accumulate_state_time(state: &mut WatchState, now_instant: Instant, agent_active: bool) {
    let elapsed_since_last_check = i64::try_from(
        now_instant
            .duration_since(state.last_idle_check)
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    if agent_active {
        state.accumulated_agent_ms = state
            .accumulated_agent_ms
            .saturating_add(elapsed_since_last_check);
    }
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
    now_ms: u64,
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
        state.session_start_mono_ms = now_ms;
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
            state.input_offsets.push(0);
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

/// Kind of metadata change on the focused window, used to decide whether to
/// close the current session (write an events row) and start a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataChange {
    /// No change to the focused window's app_id or title.
    None,
    /// The focused app changed (real context switch) — always a session
    /// boundary.
    AppChanged,
    /// Only the window title changed (e.g. browser page nav, or a terminal
    /// updating its title). Honor at most once per debounce interval so an
    /// animated title (spinner/clock/progress) cannot create a session — and a
    /// DB row — every tick. This was the production "flush storm" root cause
    /// (see docs/SLEEP_DETECTION.lean bug B1 notes).
    TitleChanged,
}

fn classify_metadata_change(state: &WatchState, id: u64, next: &WindowInfo) -> MetadataChange {
    if state.focused_id != Some(id) {
        return MetadataChange::None;
    }
    match state.windows.get(&id) {
        Some(current) if current.app_id != next.app_id => MetadataChange::AppChanged,
        Some(current) if current.title != next.title => MetadataChange::TitleChanged,
        _ => MetadataChange::None,
    }
}

/// Action to take after receiving updated metadata for a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataChangeDecision {
    CacheOnly,
    RolloverApp,
    RolloverTitle,
}

/// Decide whether updated metadata ends the current session.
///
/// Title changes while locked or away only update the window cache because no
/// live session exists to roll over. Active title changes remain debounced.
fn decide_metadata_change(
    state: &WatchState,
    change: MetadataChange,
    now_instant: Instant,
) -> MetadataChangeDecision {
    match change {
        MetadataChange::None => MetadataChangeDecision::CacheOnly,
        MetadataChange::AppChanged => MetadataChangeDecision::RolloverApp,
        MetadataChange::TitleChanged
            if state.is_locked || state.current_state == ActivityState::Away =>
        {
            MetadataChangeDecision::CacheOnly
        }
        MetadataChange::TitleChanged
            if now_instant.duration_since(state.last_title_flush)
                < Duration::from_secs(TITLE_FLUSH_DEBOUNCE_SECS) =>
        {
            MetadataChangeDecision::CacheOnly
        }
        MetadataChange::TitleChanged => MetadataChangeDecision::RolloverTitle,
    }
}

fn handle_title_changed(
    state: &mut WatchState,
    ctx: &mut NiriEventContext<'_>,
) -> Result<(), Error> {
    if state.is_locked {
        return Ok(());
    }

    tracing::debug!(target: "input_debug", "SNAPSHOT caller: handle_title_changed");
    let input = ctx.input_stats.snapshot();
    let jiggler = ctx.input_stats.jiggler_detected();
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
        print_focus_change_status(info, &flushed, &input, jiggler, state.focus_start, ctx);
    }

    state.start_new_event_segment(
        ctx.now,
        ctx.now_instant,
        millis_u64(ctx.monitor_start.elapsed()),
    );
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
    state.input_offsets.push(0);

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
/// Maximum age of the pwd file (written by shell hooks) before we consider it
/// stale.
const PWD_FILE_FRESHNESS_SECS: u64 = 30;

/// App IDs whose focused window can safely consume the global shell-hook state.
const TERMINAL_APP_IDS: &[&str] = &[
    "Alacritty",
    "kitty",
    "foot",
    "org.wezfurlong.wezterm",
    "wezterm",
    "alacritty",
    "org.codeberg.dnkl.foot",
];

fn is_terminal_app(app_id: &str) -> bool {
    TERMINAL_APP_IDS.contains(&app_id)
}

fn terminal_fallback<T>(app_id: &str, fallback: impl FnOnce() -> Option<T>) -> Option<T> {
    is_terminal_app(app_id).then(fallback).flatten()
}

/// Detect the current project by checking /proc/PID/cwd first, then the shell
/// pwd file for terminals, then window title parsing. Applies project aliases
/// from config.
fn detect_project(config: &Config, app_id: &str, title: &str, pid: Option<i32>) -> Option<String> {
    // 1. Try /proc/PID/cwd (most reliable — no shell hook needed, always
    //    current)
    if let Some(project) = proc_cwd_project(pid) {
        return Some(apply_alias(config, project));
    }

    // 2. Only terminals may consume process-global shell-hook state. It has no
    // association with browser or IDE windows and could otherwise leak the
    // most recent terminal project into an unrelated event.
    if let Some(project) = terminal_fallback(app_id, pwd_file_project) {
        return Some(apply_alias(config, project));
    }

    // 3. Title-based detection (terminal patterns)
    if is_terminal_app(app_id) {
        if let Some(name) = project::detect_project_from_title(title) {
            // OC sessions and compound names always pass through
            if name.contains(':') || name.contains(' ') {
                return Some(apply_alias(config, name));
            }
            // Simple basenames: try to validate via search_dirs if configured
            if !config.project_search_dirs.is_empty() {
                if let Some(resolved) =
                    project::resolve_project_in_search_dirs(&name, &config.project_search_dirs)
                {
                    return Some(apply_alias(config, resolved));
                }
            }
            // Accept the name — it came from detect_project_from_path (full
            // path titles) or is a legitimate basename from a shell
            // prompt
            return Some(apply_alias(config, name));
        }
    }

    // 4. Try app-specific title parsing (IDEs, editors)
    if let Some(name) = project::detect_project_from_app_title(app_id, title) {
        return Some(apply_alias(config, name));
    }

    None
}

/// Apply project alias mapping from config, falling back to the original name.
fn apply_alias(config: &Config, name: String) -> String {
    config.project_aliases.get(&name).cloned().unwrap_or(name)
}

/// Try to detect a project from /proc/PID/cwd of the terminal's child shell.
/// Walks the process tree to find the deepest child with a project-bearing cwd.
fn proc_cwd_project(pid: Option<i32>) -> Option<String> {
    let pid = pid?;

    // Find the deepest child process (terminal → shell → maybe inner process)
    let leaf_pid = find_leaf_child(pid);

    // Read the child's cwd via /proc
    let cwd = fs::read_link(format!("/proc/{}/cwd", leaf_pid)).ok()?;
    project::detect_project_from_path(&cwd)
}

/// Walk the process tree to find the deepest (leaf) child.
/// Stops at depth 5 to avoid infinite loops with process forks.
fn find_leaf_child(pid: i32) -> i32 {
    let mut current = pid;
    for _ in 0..5 {
        let children_path = format!("/proc/{}/task/{}/children", current, current);
        let children = match fs::read_to_string(&children_path) {
            Ok(s) => s,
            Err(_) => break,
        };
        // Take the first child PID
        match children
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<i32>().ok())
        {
            Some(child) => current = child,
            None => break,
        }
    }
    current
}

/// Attempt to detect a project from the shell hook's pwd file.
/// Returns None if the file is missing, stale (>30s), or detection fails.
fn pwd_file_project() -> Option<String> {
    let data_dir = get_data_dir().ok()?;
    let pwd_path = data_dir.join("current_pwd");

    // Check file freshness
    let metadata = fs::metadata(&pwd_path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::from_secs(u64::MAX));
    if age.as_secs() > PWD_FILE_FRESHNESS_SECS {
        return None;
    }

    let contents = fs::read_to_string(&pwd_path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }

    project::detect_project_from_path(Path::new(trimmed))
}

/// Detect the git branch from the shell hook's pwd file path.
/// Uses the same pwd file as project detection for consistency.
fn detect_branch_from_pwd(app_id: &str, pid: Option<i32>) -> Option<String> {
    // 1. Try /proc/PID/cwd first
    if let Some(pid) = pid {
        let leaf = find_leaf_child(pid);
        if let Ok(cwd) = fs::read_link(format!("/proc/{}/cwd", leaf)) {
            if let Some(branch) = project::detect_git_branch(&cwd) {
                return Some(branch);
            }
        }
    }

    // 2. Fall back to global shell state only for a terminal window.
    terminal_fallback(app_id, || Some(()))?;
    let data_dir = get_data_dir().ok()?;
    let pwd_path = data_dir.join("current_pwd");

    // Check file freshness (same threshold as project detection)
    let metadata = fs::metadata(&pwd_path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::from_secs(u64::MAX));
    if age.as_secs() > PWD_FILE_FRESHNESS_SECS {
        return None;
    }

    let contents = fs::read_to_string(&pwd_path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }

    project::detect_git_branch(Path::new(trimmed))
}

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

        let detected_project = detect_project(ctx.config, &info.app_id, &info.title, info.pid);
        let git_branch = detect_branch_from_pwd(&info.app_id, info.pid);

        insert_event(
            ctx.conn,
            SessionSnapshot {
                window: info.clone(),
                config: ctx.config,
                focus_start: *accum.focus_start,
                active_ms: *accum.active_ms,
                passive_ms: *accum.passive_ms,
                idle_ms: *accum.idle_ms,
                agent_ms: *accum.agent_ms,
                input: *input,
                jiggler_detected: jiggler,
                input_offsets: std::mem::take(accum.input_offsets),
                project: detected_project,
                git_branch,
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
            *accum.agent_ms = 0;
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
            *accum.agent_ms = 0;
            *accum.input_baseline_ms = new_baseline_ms;
            *accum.session_start_mono_ms = new_session_mono_ms;
            *accum.last_seen_input_ms = new_baseline_ms;
            accum.input_offsets.clear();
            accum.input_offsets.push(0);
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

    let mut config = load_config()?;
    // A page the user has not visited before is absent from history, so live
    // classification lags by one visit; reports re-resolve it afterwards.
    config.load_browser_history();
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
    {
        let _linkscope_db_init = linkscope::phase("watch.db_init");
        init_db(&conn)?;
        run_migrations(&mut conn, &config)?;
        reclassify_all(&mut conn, &config)?;
        match crate::db::heal_missing_agent_ms(&mut conn) {
            Ok(0) => {}
            Ok(n) => println!("Agent-time measurement settled for {n} unmeasured events"),
            // Healing is a repair, not a prerequisite: a corrupt agent log
            // must not stop the watcher from recording activity.
            Err(e) => tracing::warn!("Could not reconstruct agent time: {e}"),
        }
    }

    // Report generation and SMTP live on a dedicated bounded worker. The
    // watcher only performs non-blocking queue pokes.
    let mut scheduler = Scheduler::start(quiet)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            shutdown_clone.store(true, Ordering::SeqCst);
        }
    });

    let monitor_start = Instant::now();
    let input_stats = {
        let _linkscope_input_start = linkscope::phase("watch.input_start");
        start_idle_monitor(
            monitor_start,
            config.jiggler.clone(),
            config.mouse_idle_threshold,
        )
    };
    let logind = {
        let _linkscope_logind_start = linkscope::phase("watch.logind_start");
        start_logind_monitor()?
    };
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

    let mut socket = {
        let _linkscope_niri_connect = linkscope::phase("watch.niri_connect");
        connect_to_niri()?
    };
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

    let linkscope_report_interval = crate::profiling::report_interval();
    let mut last_linkscope_report = Instant::now();

    loop {
        let _linkscope_watch_loop = linkscope::phase("watch.loop");
        linkscope::record_items("watch.loop", 1);
        let now_instant = Instant::now();
        if handle_shutdown(
            &shutdown,
            &mut state,
            &flush_ctx,
            &input_stats,
            &mut agent_monitor,
            monitor_start,
            now_instant,
        )? {
            scheduler.shutdown();
            return Ok(());
        }

        {
            let _linkscope_watch_maintenance = linkscope::phase("watch.maintenance");
            handle_suspend_resume(
                &mut state,
                &logind,
                &flush_ctx,
                &input_stats,
                &mut agent_monitor,
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
                &mut agent_monitor,
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

            // Check scheduled reports every 5 minutes (piggybacks on flush
            // interval)
            if now_instant.duration_since(state.last_schedule_check)
                >= Duration::from_secs(FLUSH_INTERVAL_SECS)
            {
                state.last_schedule_check = now_instant;
                check_scheduled_reports(&scheduler, &config);
            }
        }

        crate::profiling::report_periodically(
            &mut last_linkscope_report,
            linkscope_report_interval,
        );

        let event = {
            let _linkscope_watch_recv = linkscope::phase("watch.recv_timeout");
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ev) => {
                    linkscope::record_items("watch.events", 1);
                    Some(ev)
                }
                Err(RecvTimeoutError::Timeout) => {
                    linkscope::record_items("watch.recv_timeout", 1);
                    None
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if shutdown.load(Ordering::SeqCst) {
                        continue;
                    }
                    flush_on_disconnect(&mut state, &flush_ctx, &input_stats);
                    return Err(Error::NiriEventStreamClosed);
                }
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
                let _linkscope_event = linkscope::phase("watch.event.windows_changed");
                handle_windows_changed(&mut state, &niri_ctx, win_list)?;
            }
            Event::WindowOpenedOrChanged { window } => {
                let _linkscope_event = linkscope::phase("watch.event.window_changed");
                let info = WindowInfo::from(&window);
                let change = classify_metadata_change(&state, window.id, &info);
                match decide_metadata_change(&state, change, now_instant) {
                    MetadataChangeDecision::CacheOnly => {}
                    MetadataChangeDecision::RolloverApp => {
                        handle_focus_changed(&mut state, &mut niri_ctx, Some(window.id))?;
                    }
                    MetadataChangeDecision::RolloverTitle => {
                        handle_title_changed(&mut state, &mut niri_ctx)?;
                        state.last_title_flush = now_instant;
                    }
                }
                state.windows.insert(window.id, info);
            }
            Event::WindowClosed { id } => {
                let _linkscope_event = linkscope::phase("watch.event.window_closed");
                handle_window_closed(&mut state, &niri_ctx, id)?;
            }
            Event::WindowFocusChanged { id: new_focus_id } => {
                let _linkscope_event = linkscope::phase("watch.event.focus_changed");
                handle_focus_changed(&mut state, &mut niri_ctx, new_focus_id)?;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn false_active_snapshot() -> InputSnapshot {
        InputSnapshot::default()
    }

    fn reclassified_with(input: InputSnapshot) -> (i64, i64) {
        let mut active_ms = 1_000;
        let mut passive_ms = 0;
        reclassify_false_active(&mut active_ms, &mut passive_ms, &input, 1_000, true);
        (active_ms, passive_ms)
    }

    #[test]
    fn scroll_only_input_is_not_reclassified_as_passive() {
        let mut input = false_active_snapshot();
        input.scroll_events = 1;
        assert_eq!(reclassified_with(input), (1_000, 0));
    }

    #[test]
    fn qualifying_mouse_motion_is_not_reclassified_as_passive() {
        let mut input = false_active_snapshot();
        input.qualifying_mouse_movements = 1;
        input.mouse_distance = 50;
        assert_eq!(reclassified_with(input), (1_000, 0));
    }

    #[test]
    fn sub_threshold_mouse_noise_is_reclassified_as_passive() {
        let mut input = false_active_snapshot();
        input.mouse_distance = 1;
        assert_eq!(reclassified_with(input), (0, 1_000));
    }

    #[test]
    fn global_shell_fallback_is_terminal_only() {
        let project = Some("niri-activity-rs".to_string());
        assert_eq!(terminal_fallback("foot", || project.clone()), project);
        assert_eq!(terminal_fallback("kitty", || project.clone()), project);
        assert_eq!(terminal_fallback("zen", || project.clone()), None);
        assert_eq!(terminal_fallback("code", || project.clone()), None);
        assert_eq!(terminal_fallback("jetbrains-rustrover", || project), None);
    }

    #[test]
    fn ide_title_detection_remains_independent_of_shell_fallback() {
        assert_eq!(
            terminal_fallback("code", || Some("wrong-shell-project".to_string())),
            None
        );
        assert_eq!(
            project::detect_project_from_app_title(
                "code",
                "watcher.rs - niri-activity-rs - Visual Studio Code",
            ),
            Some("niri-activity-rs".to_string())
        );
    }

    fn thresholds() -> IdleThresholds {
        IdleThresholds {
            idle_ms: 120_000,
            deep_idle_ms: 300_000,
            away_ms: 1_800_000,
        }
    }

    #[test]
    fn compute_activity_state_enters_away_without_agent_activity() {
        assert_eq!(
            compute_activity_state(1_800_001, false, &thresholds()),
            ActivityState::Away
        );
    }

    #[test]
    fn compute_activity_state_counts_agent_work_while_user_is_briefly_idle() {
        // Agent activity keeps the session Active while the user is only
        // briefly idle (below the away threshold) — e.g. reading while an
        // agent works.
        assert_eq!(
            compute_activity_state(60_000, true, &thresholds()),
            ActivityState::Active
        );
    }

    #[test]
    fn compute_activity_state_goes_away_despite_agent_when_human_is_gone() {
        // Once the human has been idle past away_ms, agent activity must NOT
        // keep the session present (docs/SLEEP_DETECTION.lean bug B1).
        assert_eq!(
            compute_activity_state(3_600_000, true, &thresholds()),
            ActivityState::Away
        );
    }

    #[test]
    fn startup_and_true_session_reset_persist_presence_anchor() {
        let mut state = WatchState::new(100, 0);
        assert_eq!(state.input_offsets, vec![0]);

        state.input_offsets.push(60_000);
        state.reset_session(Instant::now(), 60_100, 60_000);

        assert_eq!(state.input_baseline_ms, 60_100);
        assert_eq!(state.session_start_mono_ms, 60_000);
        assert_eq!(state.last_seen_input_ms, 60_100);
        assert_eq!(state.input_offsets, vec![0]);
    }

    #[test]
    fn resuming_from_away_rebases_offsets_on_the_waking_input() {
        let mut state = WatchState::new(100, 0);
        state.input_offsets.push(9_000_000);

        state.resume_after_away(Instant::now(), 9_500_000, 9_500_000);

        assert_eq!(state.input_baseline_ms, 9_500_000);
        assert_eq!(state.session_start_mono_ms, 9_500_000);
        assert_eq!(state.last_seen_input_ms, 9_500_000);
        assert_eq!(state.input_offsets, vec![0]);
    }

    #[test]
    fn title_rollover_preserves_human_presence_state() {
        let mut state = WatchState::new(10_000, 5_000);
        state.current_state = ActivityState::Idle;
        state.accumulated_active_ms = 10;
        state.accumulated_passive_ms = 20;
        state.accumulated_idle_ms = 30;
        state.accumulated_agent_ms = 40;
        state.input_offsets = vec![100, 200];
        let presence_check = state.last_idle_check;
        let first = Instant::now();

        state.start_new_event_segment(Utc::now(), first, 60_000);
        state.start_new_event_segment(Utc::now(), first, 120_000);

        assert_eq!(state.current_state, ActivityState::Idle);
        assert_eq!(state.last_idle_check, presence_check);
        assert_eq!(state.input_baseline_ms, 10_000);
        assert_eq!(state.last_seen_input_ms, 10_000);
        assert_eq!(state.session_start_mono_ms, 120_000);
        assert_eq!(state.accumulated_active_ms, 0);
        assert_eq!(state.accumulated_passive_ms, 0);
        assert_eq!(state.accumulated_idle_ms, 0);
        assert_eq!(state.accumulated_agent_ms, 0);
        assert_eq!(state.input_offsets, Vec::<u32>::new());
    }

    #[test]
    fn title_rollovers_do_not_delay_away_without_input() {
        let last_input_ms = 1_000;
        let mut state = WatchState::new(last_input_ms, last_input_ms);
        for now_ms in [60_000, 121_001, 301_001, 1_801_001] {
            state.start_new_event_segment(Utc::now(), Instant::now(), now_ms);
            let idle_ms = now_ms.saturating_sub(last_input_ms);
            let expected = if idle_ms > thresholds().away_ms {
                ActivityState::Away
            } else if idle_ms > thresholds().deep_idle_ms {
                ActivityState::Idle
            } else if idle_ms > thresholds().idle_ms {
                ActivityState::Passive
            } else {
                ActivityState::Active
            };
            assert_eq!(
                compute_activity_state(idle_ms, false, &thresholds()),
                expected
            );
        }
        assert_eq!(
            compute_activity_state(1_800_001, true, &thresholds()),
            ActivityState::Away
        );
    }

    fn advanced_state(current: ActivityState, agent_active: bool) -> WatchState {
        let mut state = WatchState::new(0, 0);
        state.current_state = current;
        state.last_idle_check = Instant::now()
            .checked_sub(Duration::from_millis(1000))
            .expect("instant predates process start");
        accumulate_state_time(&mut state, Instant::now(), agent_active);
        state
    }

    fn boundary_state(current: ActivityState) -> (WatchState, Instant) {
        let boundary = Instant::now();
        let mut state = WatchState::new(0, 0);
        state.current_state = current;
        state.last_idle_check = boundary
            .checked_sub(Duration::from_millis(1_250))
            .expect("instant predates process start");
        (state, boundary)
    }

    fn assert_boundary_charge(current: ActivityState, expected: (i64, i64, i64), agent: bool) {
        let (mut state, boundary) = boundary_state(current);
        charge_boundary_elapsed(&mut state, boundary, agent);
        assert_eq!(
            (
                state.accumulated_active_ms,
                state.accumulated_passive_ms,
                state.accumulated_idle_ms
            ),
            expected,
        );
        assert_eq!(state.accumulated_agent_ms, i64::from(agent) * 1_250);
        assert_eq!(state.last_idle_check, boundary);

        accumulate_state_time(&mut state, boundary, agent);
        assert_eq!(
            state.accumulated_active_ms + state.accumulated_passive_ms + state.accumulated_idle_ms,
            1_250,
        );
        assert_eq!(state.accumulated_agent_ms, i64::from(agent) * 1_250);
    }

    #[test]
    fn lock_boundary_charges_pre_lock_state_once() {
        assert_boundary_charge(ActivityState::Active, (1_250, 0, 0), true);
    }

    #[test]
    fn suspend_boundary_charges_pre_suspend_state_once() {
        assert_boundary_charge(ActivityState::Passive, (0, 1_250, 0), false);
    }

    #[test]
    fn enter_away_boundary_charges_pre_away_state_once() {
        assert_boundary_charge(ActivityState::Idle, (0, 0, 1_250), true);
    }

    #[test]
    fn shutdown_boundary_charges_pre_shutdown_state_once() {
        assert_boundary_charge(ActivityState::Active, (1_250, 0, 0), false);
    }

    #[test]
    fn boundary_reset_conserves_elapsed_time_without_resume_replay() {
        let (mut state, boundary) = boundary_state(ActivityState::Passive);
        state.accumulated_active_ms = 500;
        charge_boundary_elapsed(&mut state, boundary, true);
        let persisted_human_ms =
            state.accumulated_active_ms + state.accumulated_passive_ms + state.accumulated_idle_ms;
        let persisted_agent_ms = state.accumulated_agent_ms;
        assert_eq!((persisted_human_ms, persisted_agent_ms), (1_750, 1_250));

        state.reset_session(boundary, 10_000, 10_000);
        accumulate_state_time(&mut state, boundary, true);
        assert_eq!(
            state.accumulated_active_ms + state.accumulated_passive_ms + state.accumulated_idle_ms,
            0,
        );
        assert_eq!(state.accumulated_agent_ms, 0);
    }

    #[test]
    fn agent_time_accrues_independently_of_activity_state() {
        // Agent time overlaps the state counters rather than partitioning
        // them, so an idle user with a working agent accrues both.
        let state = advanced_state(ActivityState::Idle, true);
        assert!(state.accumulated_agent_ms >= 1000);
        assert!(state.accumulated_idle_ms >= 1000);
        assert_eq!(state.accumulated_active_ms, 0);
    }

    #[test]
    fn agent_time_accrues_while_the_user_is_also_active() {
        let state = advanced_state(ActivityState::Active, true);
        assert!(state.accumulated_agent_ms >= 1000);
        assert!(state.accumulated_active_ms >= 1000);
    }

    #[test]
    fn no_agent_means_no_agent_time() {
        let state = advanced_state(ActivityState::Active, false);
        assert_eq!(state.accumulated_agent_ms, 0);
        assert!(state.accumulated_active_ms >= 1000);
    }

    #[test]
    fn resetting_accumulators_clears_agent_time() {
        // A leak here would carry one session's agent time into the next.
        let mut state = advanced_state(ActivityState::Active, true);
        assert!(state.accumulated_agent_ms > 0);
        state.reset_accumulators();
        assert_eq!(state.accumulated_agent_ms, 0);
    }

    #[test]
    fn classify_metadata_change_distinguishes_app_title_and_noise() {
        let mut state = WatchState::new(0, 0);
        state.focused_id = Some(7);
        state.windows.insert(
            7,
            WindowInfo {
                app_id: "zen".to_string(),
                title: "GitHub".to_string(),
                pid: Some(10),
            },
        );

        let title_changed = WindowInfo {
            app_id: "zen".to_string(),
            title: "YouTube".to_string(),
            pid: Some(10),
        };
        let app_changed = WindowInfo {
            app_id: "foot".to_string(),
            title: "GitHub".to_string(),
            pid: Some(10),
        };
        let pid_only = WindowInfo {
            app_id: "zen".to_string(),
            title: "GitHub".to_string(),
            pid: Some(11),
        };

        assert_eq!(
            classify_metadata_change(&state, 7, &title_changed),
            MetadataChange::TitleChanged
        );
        assert_eq!(
            classify_metadata_change(&state, 7, &app_changed),
            MetadataChange::AppChanged
        );
        assert_eq!(
            classify_metadata_change(&state, 7, &pid_only),
            MetadataChange::None
        );
        // Not the focused window -> no change.
        assert_eq!(
            classify_metadata_change(&state, 8, &title_changed),
            MetadataChange::None
        );
    }

    fn title_change_state(current_state: ActivityState) -> WatchState {
        let mut state = WatchState::new(0, 0);
        state.focused_id = Some(7);
        state.current_state = current_state;
        state.windows.insert(
            7,
            WindowInfo {
                app_id: "zen".to_string(),
                title: "Before".to_string(),
                pid: Some(10),
            },
        );
        state
    }

    fn changed_title() -> WindowInfo {
        WindowInfo {
            app_id: "zen".to_string(),
            title: "After".to_string(),
            pid: Some(10),
        }
    }

    #[test]
    fn title_change_while_away_updates_cache_without_rollover() {
        let mut state = title_change_state(ActivityState::Away);
        let previous_flush = state.last_title_flush;
        let now = previous_flush + Duration::from_secs(TITLE_FLUSH_DEBOUNCE_SECS + 1);
        let next = changed_title();
        let change = classify_metadata_change(&state, 7, &next);

        assert_eq!(
            decide_metadata_change(&state, change, now),
            MetadataChangeDecision::CacheOnly
        );
        state.windows.insert(7, next);
        assert_eq!(state.windows[&7].title, "After");
        assert_eq!(state.last_title_flush, previous_flush);
    }

    #[test]
    fn title_change_while_locked_updates_cache_without_rollover() {
        let mut state = title_change_state(ActivityState::Active);
        state.is_locked = true;
        let previous_flush = state.last_title_flush;
        let now = previous_flush + Duration::from_secs(TITLE_FLUSH_DEBOUNCE_SECS + 1);
        let next = changed_title();
        let change = classify_metadata_change(&state, 7, &next);

        assert_eq!(
            decide_metadata_change(&state, change, now),
            MetadataChangeDecision::CacheOnly
        );
        state.windows.insert(7, next);
        assert_eq!(state.windows[&7].title, "After");
        assert_eq!(state.last_title_flush, previous_flush);
    }

    #[test]
    fn title_change_debounce_does_not_advance_last_flush() {
        let state = title_change_state(ActivityState::Active);
        let previous_flush = state.last_title_flush;
        let change = classify_metadata_change(&state, 7, &changed_title());

        assert_eq!(
            decide_metadata_change(&state, change, previous_flush),
            MetadataChangeDecision::CacheOnly
        );
        assert_eq!(state.last_title_flush, previous_flush);
    }

    #[test]
    fn cached_away_title_does_not_roll_over_on_resume_but_next_title_does() {
        let mut state = title_change_state(ActivityState::Away);
        let previous_flush = state.last_title_flush;
        let now = previous_flush + Duration::from_secs(TITLE_FLUSH_DEBOUNCE_SECS + 1);
        let away_title = changed_title();
        let change = classify_metadata_change(&state, 7, &away_title);
        assert_eq!(
            decide_metadata_change(&state, change, now),
            MetadataChangeDecision::CacheOnly
        );
        state.windows.insert(7, away_title.clone());

        state.current_state = ActivityState::Active;
        let unchanged = classify_metadata_change(&state, 7, &away_title);
        assert_eq!(
            decide_metadata_change(&state, unchanged, now),
            MetadataChangeDecision::CacheOnly
        );

        let after_resume = WindowInfo {
            title: "After resume".to_string(),
            ..away_title
        };
        let changed = classify_metadata_change(&state, 7, &after_resume);
        assert_eq!(
            decide_metadata_change(&state, changed, now),
            MetadataChangeDecision::RolloverTitle
        );
        assert_eq!(state.last_title_flush, previous_flush);
    }

    #[test]
    fn app_change_always_rolls_over() {
        let state = title_change_state(ActivityState::Active);
        assert_eq!(
            decide_metadata_change(&state, MetadataChange::AppChanged, state.last_title_flush),
            MetadataChangeDecision::RolloverApp
        );
    }
}
