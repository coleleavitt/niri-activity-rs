use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread;
use std::time::Duration;

use logind_zbus::manager::ManagerProxyBlocking;
use logind_zbus::session::SessionProxyBlocking;
use zbus::blocking::Connection;

use crate::error::Error;

/// Monitors logind D-Bus signals for screen lock/unlock and suspend/resume
/// events.
///
/// Replaces the old `is_session_locked()` approach that spawned two `loginctl`
/// subprocesses every loop iteration. Instead, background threads subscribe to
/// D-Bus signals and flip `AtomicBool` flags that the main loop reads cheaply.
///
/// Pattern matches `start_idle_monitor()` in `input.rs` — background threads
/// with atomics, no locks.
pub struct LogindMonitor {
    lock_state: Arc<AtomicU8>,
    suspend_resumed: Arc<AtomicBool>,
}

impl LogindMonitor {
    /// Read the current lock state. Non-blocking (atomic load).
    pub fn is_locked(&self) -> bool {
        self.lock_state.load(Ordering::Acquire) != LOCK_STATE_HEALTHY_UNLOCKED
    }

    /// Check and clear the suspend-resumed flag. Returns `true` exactly once
    /// after each resume from suspend, then resets to `false`.
    pub fn take_suspend_resumed(&self) -> bool {
        self.suspend_resumed.swap(false, Ordering::AcqRel)
    }

    /// Whether lock monitoring is currently fail-closed due to an unavailable
    /// or unreadable `LockedHint` stream.
    pub fn is_lock_degraded(&self) -> bool {
        self.lock_state.load(Ordering::Acquire) == LOCK_STATE_DEGRADED
    }
}

/// Find our graphical session's D-Bus object path via logind.
///
/// Strategy: prefer the current user's UID, then fall back to uid >= 1000
/// heuristic.
fn find_user_session_path(
    manager: &ManagerProxyBlocking<'_>,
) -> Result<zbus::zvariant::OwnedObjectPath, Error> {
    let sessions = manager
        .list_sessions()
        .map_err(|e| Error::Logind(format!("failed to list sessions: {e}")))?;

    if sessions.is_empty() {
        return Err(Error::LogindSessionNotFound);
    }

    // SAFETY: libc::getuid() is always safe to call
    #[allow(unsafe_code)]
    let current_uid = unsafe { libc::getuid() };

    // First, try to find a session matching the current user's UID
    for session_info in &sessions {
        if session_info.uid() == current_uid {
            return Ok(session_info.path().clone());
        }
    }

    // Fall back to uid >= 1000 heuristic for multi-user systems
    for session_info in &sessions {
        if session_info.uid() >= 1000 {
            tracing::debug!(
                "no session for current uid {}, using session with uid {}",
                current_uid,
                session_info.uid()
            );
            return Ok(session_info.path().clone());
        }
    }

    tracing::warn!("no session with uid >= 1000, using first session");
    Ok(sessions[0].path().clone())
}

/// Maximum delay between attempts to restore a failed logind listener.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
const LOCK_STATE_DEGRADED: u8 = 0;
const LOCK_STATE_HEALTHY_LOCKED: u8 = 1;
const LOCK_STATE_HEALTHY_UNLOCKED: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockMonitorEvent {
    Observed(bool),
    Failed,
}

/// Apply one ordered lock-monitor event.
///
/// Only the lock listener calls this in production. A failure is itself an
/// authoritative fail-closed update: consumers must never keep using the last
/// observed unlocked value while monitoring is degraded.
fn apply_lock_event(event: LockMonitorEvent, lock_state: &AtomicU8) {
    let state = match event {
        LockMonitorEvent::Observed(true) => LOCK_STATE_HEALTHY_LOCKED,
        LockMonitorEvent::Observed(false) => LOCK_STATE_HEALTHY_UNLOCKED,
        LockMonitorEvent::Failed => LOCK_STATE_DEGRADED,
    };
    lock_state.store(state, Ordering::Release);
}

#[derive(Debug, Clone, Copy)]
struct ReconnectBackoff {
    next: Duration,
}

impl ReconnectBackoff {
    const fn new() -> Self {
        Self {
            next: Duration::from_secs(1),
        }
    }

    fn take(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(RECONNECT_MAX_DELAY);
        delay
    }

    const fn reset(&mut self) {
        self.next = Duration::from_secs(1);
    }
}

/// Run one lock-listener connection until its stream ends or an update fails.
///
/// Subscribing before the initial read closes the startup/reconnect window: a
/// transition is either reflected by the read or queued in the ordered stream.
fn listen_for_locked_hint(lock_state: &AtomicU8) -> Result<(), Error> {
    let connection = Connection::system()
        .map_err(|e| Error::Logind(format!("failed to connect lock-state listener: {e}")))?;
    let manager = ManagerProxyBlocking::new(&connection)
        .map_err(|e| Error::Logind(format!("failed to create manager proxy: {e}")))?;
    let session_path = find_user_session_path(&manager)?;
    let session = SessionProxyBlocking::builder(&connection)
        .path(session_path)
        .map_err(|e| Error::Logind(format!("invalid session path: {e}")))?
        .build()
        .map_err(|e| Error::Logind(format!("failed to build session proxy: {e}")))?;

    let mut changes = session.receive_locked_hint_changed();
    let locked = session
        .locked_hint()
        .map_err(|e| Error::Logind(format!("failed to read LockedHint: {e}")))?;
    apply_lock_event(LockMonitorEvent::Observed(locked), lock_state);

    for change in &mut changes {
        let locked = change
            .get()
            .map_err(|e| Error::Logind(format!("failed to read changed LockedHint: {e}")))?;
        apply_lock_event(LockMonitorEvent::Observed(locked), lock_state);
        tracing::debug!(locked, "LockedHint changed");
    }

    Err(Error::Logind(
        "LockedHint signal stream ended unexpectedly".to_owned(),
    ))
}

fn run_lock_listener(lock_state: &AtomicU8) -> ! {
    let mut backoff = ReconnectBackoff::new();
    loop {
        match listen_for_locked_hint(lock_state) {
            Ok(()) => unreachable!("lock listener always reports an ended stream as an error"),
            Err(error) => {
                let was_healthy = lock_state.load(Ordering::Acquire) != LOCK_STATE_DEGRADED;
                apply_lock_event(LockMonitorEvent::Failed, lock_state);
                if was_healthy {
                    backoff.reset();
                }
                let delay = backoff.take();
                tracing::warn!(
                    %error,
                    retry_delay_secs = delay.as_secs(),
                    "logind lock monitoring degraded; publishing locked and reconnecting"
                );
                thread::sleep(delay);
            }
        }
    }
}

fn listen_for_sleep(
    suspend_resumed: &AtomicBool,
    sleep_degraded: &AtomicBool,
) -> Result<(), Error> {
    let connection = Connection::system()
        .map_err(|e| Error::Logind(format!("failed to connect sleep listener: {e}")))?;
    let manager = ManagerProxyBlocking::new(&connection)
        .map_err(|e| Error::Logind(format!("failed to create sleep manager proxy: {e}")))?;
    let signals = manager
        .receive_prepare_for_sleep()
        .map_err(|e| Error::Logind(format!("failed to subscribe to PrepareForSleep: {e}")))?;
    sleep_degraded.store(false, Ordering::Release);

    for signal in signals {
        let args = signal
            .args()
            .map_err(|e| Error::Logind(format!("failed to parse PrepareForSleep args: {e}")))?;
        if args.start {
            tracing::debug!("PrepareForSleep: suspending");
        } else {
            suspend_resumed.store(true, Ordering::Release);
            tracing::debug!("PrepareForSleep: resumed");
        }
    }

    Err(Error::Logind(
        "PrepareForSleep signal stream ended unexpectedly".to_owned(),
    ))
}

fn run_sleep_listener(suspend_resumed: &AtomicBool, sleep_degraded: &AtomicBool) -> ! {
    let mut backoff = ReconnectBackoff::new();
    loop {
        if let Err(error) = listen_for_sleep(suspend_resumed, sleep_degraded) {
            let was_healthy = !sleep_degraded.swap(true, Ordering::AcqRel);
            if was_healthy {
                backoff.reset();
            }
            let delay = backoff.take();
            tracing::warn!(
                %error,
                retry_delay_secs = delay.as_secs(),
                "logind sleep monitoring degraded; reconnecting"
            );
            thread::sleep(delay);
        }
    }
}

/// Start the logind D-Bus monitor.
///
/// Lock state starts fail-closed. The background listener is the sole ordered
/// writer and only publishes unlocked after it has subscribed and completed a
/// fresh `LockedHint` read. Connection, proxy, read, and stream failures all
/// publish locked and retry forever with bounded exponential backoff.
pub fn start_logind_monitor() -> Result<LogindMonitor, Error> {
    let lock_state = Arc::new(AtomicU8::new(LOCK_STATE_DEGRADED));
    let suspend_resumed = Arc::new(AtomicBool::new(false));
    let sleep_degraded = Arc::new(AtomicBool::new(true));

    {
        let lock_state = Arc::clone(&lock_state);
        thread::Builder::new()
            .name("logind-lock".into())
            .spawn(move || run_lock_listener(&lock_state))
            .map_err(|e| Error::Logind(format!("failed to spawn lock-state thread: {e}")))?;
    }

    {
        let suspend_resumed = Arc::clone(&suspend_resumed);
        let sleep_degraded = Arc::clone(&sleep_degraded);
        thread::Builder::new()
            .name("logind-sleep".into())
            .spawn(move || run_sleep_listener(&suspend_resumed, &sleep_degraded))
            .map_err(|e| Error::Logind(format!("failed to spawn sleep thread: {e}")))?;
    }

    Ok(LogindMonitor {
        lock_state,
        suspend_resumed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_fail_closed_and_degraded() {
        let monitor = LogindMonitor {
            lock_state: Arc::new(AtomicU8::new(LOCK_STATE_DEGRADED)),
            suspend_resumed: Arc::new(AtomicBool::new(false)),
        };

        assert!(monitor.is_locked());
        assert!(monitor.is_lock_degraded());
    }

    #[test]
    fn lock_health_is_independent_of_sleep_listener_health() {
        let sleep_degraded = AtomicBool::new(true);
        let monitor = LogindMonitor {
            lock_state: Arc::new(AtomicU8::new(LOCK_STATE_HEALTHY_UNLOCKED)),
            suspend_resumed: Arc::new(AtomicBool::new(false)),
        };

        assert!(sleep_degraded.load(Ordering::Acquire));
        assert!(!monitor.is_locked());
        assert!(!monitor.is_lock_degraded());
    }

    #[test]
    fn successful_read_publishes_observation_and_health() {
        let state = AtomicU8::new(LOCK_STATE_DEGRADED);
        apply_lock_event(LockMonitorEvent::Observed(false), &state);

        assert_eq!(state.load(Ordering::Acquire), LOCK_STATE_HEALTHY_UNLOCKED);
    }

    #[test]
    fn failure_atomically_overrides_stale_unlocked_state() {
        let state = AtomicU8::new(LOCK_STATE_HEALTHY_UNLOCKED);
        apply_lock_event(LockMonitorEvent::Failed, &state);

        assert_eq!(state.load(Ordering::Acquire), LOCK_STATE_DEGRADED);
    }

    #[test]
    fn reconnect_observation_recovers_from_degraded_state() {
        let state = AtomicU8::new(LOCK_STATE_DEGRADED);
        apply_lock_event(LockMonitorEvent::Observed(false), &state);

        assert_eq!(state.load(Ordering::Acquire), LOCK_STATE_HEALTHY_UNLOCKED);
    }

    #[test]
    fn reconnect_backoff_is_bounded_and_resettable() {
        let mut backoff = ReconnectBackoff::new();
        let delays: Vec<_> = (0..7).map(|_| backoff.take()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30].map(Duration::from_secs));

        backoff.reset();
        assert_eq!(backoff.take(), Duration::from_secs(1));
    }
}
