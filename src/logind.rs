use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

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
    is_locked: Arc<AtomicBool>,
    suspend_resumed: Arc<AtomicBool>,
    /// Tracks if any listener thread has died unexpectedly
    thread_error: Arc<AtomicBool>,
}

impl LogindMonitor {
    /// Read the current lock state. Non-blocking (atomic load).
    pub fn is_locked(&self) -> bool {
        self.is_locked.load(Ordering::Acquire)
    }

    /// Check and clear the suspend-resumed flag. Returns `true` exactly once
    /// after each resume from suspend, then resets to `false`.
    pub fn take_suspend_resumed(&self) -> bool {
        self.suspend_resumed.swap(false, Ordering::AcqRel)
    }

    /// Check if any listener thread has died. If true, logind monitoring may be
    /// degraded.
    pub fn has_thread_error(&self) -> bool {
        self.thread_error.load(Ordering::Acquire)
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

/// Start the logind D-Bus monitor. Spawns background threads for:
/// 1. Lock signal → sets `is_locked = true`
/// 2. Unlock signal → sets `is_locked = false`
/// 3. PrepareForSleep signal → sets `suspend_resumed = true` on resume
///
/// Also reads the initial `LockedHint` property so we start in the correct
/// state.
/// Start D-Bus monitor for screen lock/unlock and suspend/resume events.
pub fn start_logind_monitor() -> Result<LogindMonitor, Error> {
    let connection = Connection::system().map_err(|_| Error::LogindConnectionFailed)?;

    let manager = ManagerProxyBlocking::new(&connection)
        .map_err(|e| Error::Logind(format!("failed to create manager proxy: {e}")))?;

    let session_path = find_user_session_path(&manager)?;

    // Build session proxy to read initial state
    let session = SessionProxyBlocking::builder(&connection)
        .path(session_path.clone())
        .map_err(|e| Error::Logind(format!("invalid session path: {e}")))?
        .build()
        .map_err(|e| Error::Logind(format!("failed to build session proxy: {e}")))?;

    // Read initial LockedHint so we don't miss a lock that happened before we
    // started
    let initial_locked = session.locked_hint().unwrap_or_else(|e| {
        tracing::warn!("failed to read initial LockedHint: {e}, assuming unlocked");
        false
    });

    let is_locked = Arc::new(AtomicBool::new(initial_locked));
    let suspend_resumed = Arc::new(AtomicBool::new(false));
    let thread_error = Arc::new(AtomicBool::new(false));

    if initial_locked {
        tracing::info!("Session already locked at startup");
    }

    tracing::info!(
        "Monitoring session {} (locked={})",
        session_path.as_str(),
        initial_locked,
    );

    // Thread 1: Lock signal → is_locked = true
    {
        let is_locked = Arc::clone(&is_locked);
        let thread_error = Arc::clone(&thread_error);
        let connection = Connection::system().map_err(|e| {
            Error::Logind(format!(
                "failed to connect to system bus (lock thread): {e}"
            ))
        })?;
        let session_path = session_path.clone();

        thread::Builder::new()
            .name("logind-lock".into())
            .spawn(move || {
                let session = match SessionProxyBlocking::builder(&connection)
                    .path(session_path)
                    .and_then(zbus::blocking::proxy::Builder::build)
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("Lock listener failed to build proxy: {e}");
                        thread_error.store(true, Ordering::Release);
                        return;
                    }
                };

                match session.receive_lock() {
                    Ok(mut signals) => {
                        if session.locked_hint().unwrap_or(false) {
                            is_locked.store(true, Ordering::Release);
                        }
                        let mut iterations: u64 = 0;
                        while signals.next().is_some() {
                            iterations = iterations.saturating_add(1);
                            if iterations == u64::MAX {
                                tracing::warn!("Lock signal loop reached iteration limit");
                                break;
                            }
                            is_locked.store(true, Ordering::Release);
                            tracing::debug!("Lock signal received");
                        }
                        tracing::warn!("Lock signal iterator ended unexpectedly");
                        thread_error.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to subscribe to Lock signal: {e}");
                        thread_error.store(true, Ordering::Release);
                    }
                }
            })
            .map_err(|e| Error::Logind(format!("failed to spawn lock thread: {e}")))?;
    }

    // Thread 2: Unlock signal → is_locked = false
    {
        let is_locked = Arc::clone(&is_locked);
        let thread_error = Arc::clone(&thread_error);
        let connection = Connection::system().map_err(|e| {
            Error::Logind(format!(
                "failed to connect to system bus (unlock thread): {e}"
            ))
        })?;
        thread::Builder::new()
            .name("logind-unlock".into())
            .spawn(move || {
                let session = match SessionProxyBlocking::builder(&connection)
                    .path(session_path)
                    .and_then(zbus::blocking::proxy::Builder::build)
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("Unlock listener failed to build proxy: {e}");
                        thread_error.store(true, Ordering::Release);
                        return;
                    }
                };

                match session.receive_unlock() {
                    Ok(mut signals) => {
                        if !session.locked_hint().unwrap_or(true) {
                            is_locked.store(false, Ordering::Release);
                        }
                        let mut iterations: u64 = 0;
                        while signals.next().is_some() {
                            iterations = iterations.saturating_add(1);
                            if iterations == u64::MAX {
                                tracing::warn!("Unlock signal loop reached iteration limit");
                                break;
                            }
                            is_locked.store(false, Ordering::Release);
                            tracing::debug!("Unlock signal received");
                        }
                        tracing::warn!("Unlock signal iterator ended unexpectedly");
                        thread_error.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to subscribe to Unlock signal: {e}");
                        thread_error.store(true, Ordering::Release);
                    }
                }
            })
            .map_err(|e| Error::Logind(format!("failed to spawn unlock thread: {e}")))?;
    }

    // Thread 3: PrepareForSleep signal → set suspend_resumed on resume
    // (start=false)
    {
        let suspend_resumed = Arc::clone(&suspend_resumed);
        let thread_error = Arc::clone(&thread_error);
        let connection = Connection::system().map_err(|e| {
            Error::Logind(format!(
                "failed to connect to system bus (sleep thread): {e}"
            ))
        })?;

        thread::Builder::new()
            .name("logind-sleep".into())
            .spawn(move || {
                let manager = match ManagerProxyBlocking::new(&connection) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("Sleep listener failed to build proxy: {e}");
                        thread_error.store(true, Ordering::Release);
                        return;
                    }
                };

                match manager.receive_prepare_for_sleep() {
                    Ok(signals) => {
                        let mut iterations: u64 = 0;
                        for signal in signals {
                            iterations = iterations.saturating_add(1);
                            if iterations == u64::MAX {
                                tracing::warn!("Sleep signal loop reached iteration limit");
                                break;
                            }
                            match signal.args() {
                                Ok(args) => {
                                    if args.start {
                                        tracing::debug!("PrepareForSleep: suspending");
                                    } else {
                                        suspend_resumed.store(true, Ordering::Release);
                                        tracing::debug!("PrepareForSleep: resumed");
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse PrepareForSleep args: {e}");
                                }
                            }
                        }
                        tracing::warn!("Sleep signal iterator ended unexpectedly");
                        thread_error.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to subscribe to PrepareForSleep signal: {e}");
                        thread_error.store(true, Ordering::Release);
                    }
                }
            })
            .map_err(|e| Error::Logind(format!("failed to spawn sleep thread: {e}")))?;
    }

    Ok(LogindMonitor {
        is_locked,
        suspend_resumed,
        thread_error,
    })
}
