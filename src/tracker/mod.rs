//! Compositor-neutral, synchronous window tracking.
//!
//! A connected source has one consumer and must yield an authoritative
//! [`Snapshot`] before any incremental update. Window identifiers are opaque
//! and scoped to that source connection; consumers must not persist or compare
//! them across reconnects.

#[cfg(feature = "gnome")]
mod gnome;
#[cfg(feature = "kde")]
mod kde;
#[cfg(feature = "niri")]
mod niri;
mod select;
#[cfg(feature = "wlr-toplevel")]
mod wlr;

use std::error::Error;
use std::fmt;
use std::time::Duration;

pub use select::{Backend, select_runtime};

/// Opaque identity of an open window within one tracker connection.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct WindowId(Box<str>);

#[cfg_attr(
    not(any(
        feature = "niri",
        feature = "wlr-toplevel",
        feature = "gnome",
        feature = "kde"
    )),
    allow(dead_code)
)]
impl WindowId {
    pub(crate) fn new(value: impl Into<Box<str>>) -> Self {
        Self(value.into())
    }

    #[cfg(test)]
    pub(crate) fn test(value: impl Into<Box<str>>) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for WindowId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("WindowId").field(&self.0).finish()
    }
}

/// Complete normalized metadata for one open window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub app_id: String,
    pub title: String,
    pub pid: Option<i32>,
}

/// Authoritative replacement for the complete window and focus state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub windows: Vec<WindowInfo>,
    pub focused: Option<WindowId>,
}

/// One normalized update from a compositor backend.
/// Shared event vocabulary; focused-only adapters do not construct every
/// variant.
#[cfg_attr(
    not(any(feature = "niri", feature = "wlr-toplevel", feature = "kde")),
    allow(dead_code)
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// Mandatory first update and authoritative resynchronization barrier.
    Snapshot(Snapshot),
    /// Full metadata for a new or changed window.
    ///
    /// `focused` is explicit because some protocols fold a focus transition
    /// into an upsert and do not follow it with a separate focus event.
    Upsert {
        window: WindowInfo,
        focused: bool,
    },
    Closed(WindowId),
    Focused(Option<WindowId>),
}

/// Broad error class used for backend selection and failure policy.
#[cfg_attr(
    not(any(
        feature = "niri",
        feature = "wlr-toplevel",
        feature = "gnome",
        feature = "kde"
    )),
    allow(dead_code)
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerErrorKind {
    Unavailable,
    Disconnected,
    Protocol,
    Io,
}

/// Source-preserving tracker failure.
#[derive(Debug)]
pub struct TrackerError {
    kind: TrackerErrorKind,
    message: Box<str>,
    source: Option<Box<dyn Error + Send + Sync>>,
}

#[cfg_attr(
    not(any(
        feature = "niri",
        feature = "wlr-toplevel",
        feature = "gnome",
        feature = "kde"
    )),
    allow(dead_code)
)]
impl TrackerError {
    pub fn new(kind: TrackerErrorKind, message: impl Into<Box<str>>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(
        kind: TrackerErrorKind,
        message: impl Into<Box<str>>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    pub fn kind(&self) -> TrackerErrorKind {
        self.kind
    }
}

impl fmt::Display for TrackerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl Error for TrackerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Reusable factory for a compositor connection.
pub trait WindowTracker: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn connect(&self) -> Result<Box<dyn WindowEventSource>, TrackerError>;
}

/// Ordered, single-consumer window update source.
pub trait WindowEventSource: Send {
    /// Wait for one update until `timeout` expires.
    ///
    /// `Ok(None)` means only that the deadline elapsed. EOF, lag disconnect,
    /// parse failure, and I/O failure return an error.
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Update>, TrackerError>;
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::VecDeque;
    use std::time::Duration;

    use super::{TrackerError, Update, WindowEventSource};

    pub enum Step {
        Update(Update),
        Timeout,
        Error(TrackerError),
    }

    pub struct ScriptedSource {
        steps: VecDeque<Step>,
        pub requested_timeouts: Vec<Duration>,
    }

    impl ScriptedSource {
        pub fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                requested_timeouts: Vec::new(),
            }
        }
    }

    impl WindowEventSource for ScriptedSource {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Update>, TrackerError> {
            self.requested_timeouts.push(timeout);
            match self.steps.pop_front().unwrap_or(Step::Timeout) {
                Step::Update(update) => Ok(Some(update)),
                Step::Timeout => Ok(None),
                Step::Error(error) => Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{ScriptedSource, Step};
    use super::*;

    fn id(value: &str) -> WindowId {
        WindowId::test(value)
    }

    fn window(value: &str) -> WindowInfo {
        WindowInfo {
            id: id(value),
            app_id: "app".to_owned(),
            title: value.to_owned(),
            pid: None,
        }
    }

    #[test]
    fn scripted_source_preserves_snapshot_upsert_focus_close_order() {
        let updates = [
            Update::Snapshot(Snapshot {
                windows: vec![window("one")],
                focused: Some(id("one")),
            }),
            Update::Upsert {
                window: window("two"),
                focused: true,
            },
            Update::Focused(Some(id("one"))),
            Update::Closed(id("two")),
        ];
        let mut source = ScriptedSource::new(updates.clone().map(Step::Update));
        for expected in updates {
            assert_eq!(
                source.recv_timeout(Duration::from_millis(10)).unwrap(),
                Some(expected)
            );
        }
    }

    #[test]
    fn scripted_source_distinguishes_timeout_update_and_disconnect() {
        let update = Update::Focused(None);
        let mut source = ScriptedSource::new([
            Step::Timeout,
            Step::Update(update.clone()),
            Step::Error(TrackerError::new(TrackerErrorKind::Disconnected, "closed")),
        ]);
        let timeout = Duration::from_secs(1);

        assert_eq!(source.recv_timeout(timeout).unwrap(), None);
        assert_eq!(source.recv_timeout(timeout).unwrap(), Some(update));
        assert_eq!(
            source.recv_timeout(timeout).unwrap_err().kind(),
            TrackerErrorKind::Disconnected
        );
        assert_eq!(source.requested_timeouts, [timeout, timeout, timeout]);
    }
}
