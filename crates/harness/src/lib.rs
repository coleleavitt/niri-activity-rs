//! Detect when an AI coding agent is working on your behalf.
//!
//! A window-focus tracker sees no keystrokes while a model streams a response
//! and concludes the user went idle. The user is in fact working — waiting on
//! an agent is the modern equivalent of waiting on a compile — so agent
//! activity has to be measured some other way.
//!
//! Agents write continuously while they work: to a SQLite session store, to an
//! append-only transcript, or both. A recent write is therefore a reliable
//! proxy for "the model is producing output right now".
//!
//! ```no_run
//! use std::time::Duration;
//!
//! if harness::any_active(Duration::from_secs(30)) {
//!     // The user is waiting on an agent, not idle.
//! }
//! ```

mod detect;
mod harness;
mod history;
mod process;
mod tokens;

use std::time::{Duration, SystemTime};

pub use detect::{expand_tilde, resolve};
pub use harness::{Harness, Signal};
pub use history::{BusyMinutes, busy_minutes};
pub use process::{is_running, running};
pub use tokens::{TokenSource, TokenUsage, recent_usage, recent_usage_all};

/// Whether any agent generated tokens in the last `window_ms` milliseconds.
///
/// Stronger evidence than [`any_active`]: a file timestamp only shows that
/// something was written, while a non-zero generated count shows a model
/// actually produced output.
pub fn any_generating(window_ms: i64) -> bool {
    tokens::recent_usage_all(window_ms).generated() > 0
}

/// Whether a specific agent has written to any of its stores within `window`.
pub fn is_active(harness: Harness, window: Duration) -> bool {
    harness
        .signals()
        .iter()
        .any(|signal| detect::signal_active(*signal, window))
}

/// Whether any installed agent is currently working.
pub fn any_active(window: Duration) -> bool {
    Harness::ALL
        .iter()
        .any(|harness| is_active(*harness, window))
}

/// Every agent that has written within `window`.
pub fn active(window: Duration) -> Vec<Harness> {
    Harness::ALL
        .iter()
        .copied()
        .filter(|h| is_active(*h, window))
        .collect()
}

/// Most recent write across all of an agent's stores.
///
/// `None` means the agent left no trace on this machine, which is the normal
/// result for one that is installed but has never run.
pub fn last_activity(harness: Harness) -> Option<SystemTime> {
    harness
        .signals()
        .iter()
        .filter_map(|signal| detect::signal_last_write(*signal))
        .max()
}

/// Agents that have left any trace on this machine.
pub fn installed() -> Vec<Harness> {
    Harness::ALL
        .iter()
        .copied()
        .filter(|h| last_activity(*h).is_some())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probing_every_harness_is_infallible() {
        // None of these paths are guaranteed to exist; the point is that a
        // missing store reads as inactive rather than panicking.
        for h in Harness::ALL {
            let _ = is_active(*h, Duration::from_secs(30));
            let _ = last_activity(*h);
        }
    }

    #[test]
    fn a_zero_window_reports_nothing_active() {
        assert!(active(Duration::ZERO).is_empty());
    }

    #[test]
    fn active_agents_are_a_subset_of_installed_ones() {
        let installed = installed();
        for h in active(Duration::from_secs(3600)) {
            assert!(
                installed.contains(&h),
                "{h} reported active but left no trace"
            );
        }
    }
}
