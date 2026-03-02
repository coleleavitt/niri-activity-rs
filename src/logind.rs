use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use logind_zbus::manager::ManagerProxyBlocking;
use logind_zbus::session::SessionProxyBlocking;
use zbus::blocking::Connection;

use crate::error::Error;

/// Monitors logind D-Bus signals for screen lock/unlock and suspend/resume events.
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
}

/// Find our graphical session's D-Bus object path via logind.
///
/// Strategy: iterate `ListSessions`, find first session with `uid >= 1000`.
/// This matches the pattern from `logind-zbus/examples/unlock_signal.rs`.
fn find_user_session_path(
    manager: &ManagerProxyBlocking<'_>,
) -> Result<zbus::zvariant::OwnedObjectPath, Error> {
    let sessions = manager
        .list_sessions()
        .map_err(|e| Error::Logind(format!("failed to list sessions: {e}")))?;

    if sessions.is_empty() {
        return Err(Error::Logind("logind returned zero sessions".into()));
    }

    for session_info in &sessions {
        if session_info.uid() >= 1000 {
            return Ok(session_info.path().clone());
        }
    }

    // Fallback: first session (shouldn't happen on a normal desktop)
    eprintln!("[logind] Warning: no session with uid >= 1000, using first session");
    Ok(sessions[0].path().clone())
}

/// Start the logind D-Bus monitor. Spawns background threads for:
/// 1. Lock signal → sets `is_locked = true`
/// 2. Unlock signal → sets `is_locked = false`
/// 3. PrepareForSleep signal → sets `suspend_resumed = true` on resume
///
/// Also reads the initial `LockedHint` property so we start in the correct state.
pub fn start_logind_monitor() -> Result<LogindMonitor, Error> {
    let connection = Connection::system()
        .map_err(|e| Error::Logind(format!("failed to connect to system bus: {e}")))?;

    let manager = ManagerProxyBlocking::new(&connection)
        .map_err(|e| Error::Logind(format!("failed to create manager proxy: {e}")))?;

    let session_path = find_user_session_path(&manager)?;

    // Build session proxy to read initial state
    let session = SessionProxyBlocking::builder(&connection)
        .path(session_path.clone())
        .map_err(|e| Error::Logind(format!("invalid session path: {e}")))?
        .build()
        .map_err(|e| Error::Logind(format!("failed to build session proxy: {e}")))?;

    // Read initial LockedHint so we don't miss a lock that happened before we started
    let initial_locked = session.locked_hint().unwrap_or(false);

    let is_locked = Arc::new(AtomicBool::new(initial_locked));
    let suspend_resumed = Arc::new(AtomicBool::new(false));

    if initial_locked {
        eprintln!("[logind] Session already locked at startup");
    }

    eprintln!(
        "[logind] Monitoring session {} (locked={})",
        session_path.as_str(),
        initial_locked,
    );

    // Thread 1: Lock signal → is_locked = true
    {
        let is_locked = Arc::clone(&is_locked);
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
                    .and_then(|b| b.build())
                {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[logind] Lock listener failed to build proxy: {e}");
                        return;
                    }
                };

                match session.receive_lock() {
                    Ok(mut signals) => {
                        while signals.next().is_some() {
                            is_locked.store(true, Ordering::SeqCst);
                            eprintln!("[logind] Lock signal received");
                        }
                    }
                    Err(e) => {
                        eprintln!("[logind] Failed to subscribe to Lock signal: {e}");
                    }
                }
            })
            .map_err(|e| Error::Logind(format!("failed to spawn lock thread: {e}")))?;
    }

    // Thread 2: Unlock signal → is_locked = false
    {
        let is_locked = Arc::clone(&is_locked);
        let connection = Connection::system().map_err(|e| {
            Error::Logind(format!(
                "failed to connect to system bus (unlock thread): {e}"
            ))
        })?;
        let session_path = session_path.clone();

        thread::Builder::new()
            .name("logind-unlock".into())
            .spawn(move || {
                let session = match SessionProxyBlocking::builder(&connection)
                    .path(session_path)
                    .and_then(|b| b.build())
                {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[logind] Unlock listener failed to build proxy: {e}");
                        return;
                    }
                };

                match session.receive_unlock() {
                    Ok(mut signals) => {
                        while signals.next().is_some() {
                            is_locked.store(false, Ordering::SeqCst);
                            eprintln!("[logind] Unlock signal received");
                        }
                    }
                    Err(e) => {
                        eprintln!("[logind] Failed to subscribe to Unlock signal: {e}");
                    }
                }
            })
            .map_err(|e| Error::Logind(format!("failed to spawn unlock thread: {e}")))?;
    }

    // Thread 3: PrepareForSleep signal → set suspend_resumed on resume (start=false)
    {
        let suspend_resumed = Arc::clone(&suspend_resumed);
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
                        eprintln!("[logind] Sleep listener failed to build proxy: {e}");
                        return;
                    }
                };

                match manager.receive_prepare_for_sleep() {
                    Ok(signals) => {
                        for signal in signals {
                            match signal.args() {
                                Ok(args) => {
                                    if args.start {
                                        eprintln!("[logind] PrepareForSleep: suspending");
                                    } else {
                                        suspend_resumed.store(true, Ordering::SeqCst);
                                        eprintln!("[logind] PrepareForSleep: resumed");
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[logind] Failed to parse PrepareForSleep args: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[logind] Failed to subscribe to PrepareForSleep signal: {e}");
                    }
                }
            })
            .map_err(|e| Error::Logind(format!("failed to spawn sleep thread: {e}")))?;
    }

    Ok(LogindMonitor {
        is_locked,
        suspend_resumed,
    })
}
