//! Niri IPC adapter. This is the only module that imports `niri_ipc`.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use backon::{BlockingRetryable, ExponentialBuilder};
#[cfg(test)]
use niri_ipc::WindowLayout;
use niri_ipc::socket::Socket;
use niri_ipc::{Event, Request, Response, Window};

use super::{
    Snapshot, TrackerError, TrackerErrorKind, Update, WindowEventSource, WindowId, WindowInfo,
    WindowTracker,
};

const BACKOFF_MAX_INTERVAL: Duration = Duration::from_secs(300);
const CONNECT_ATTEMPTS: usize = 20;

/// Factory for niri's blocking event-stream protocol.
#[derive(Debug, Default, Clone, Copy)]
pub struct NiriTracker;

impl WindowTracker for NiriTracker {
    fn backend_name(&self) -> &'static str {
        "niri"
    }

    fn connect(&self) -> Result<Box<dyn WindowEventSource>, TrackerError> {
        let backoff = ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(100))
            .with_max_delay(BACKOFF_MAX_INTERVAL)
            .with_max_times(CONNECT_ATTEMPTS);
        let mut socket = (|| {
            Socket::connect().inspect_err(|error| {
                tracing::warn!("Connection to niri failed: {error}. Retrying...");
            })
        })
        .retry(backoff)
        .sleep(thread::sleep)
        .call()
        .map_err(|error| {
            TrackerError::with_source(
                TrackerErrorKind::Unavailable,
                "failed to connect to niri IPC socket",
                error,
            )
        })?;

        let reply = socket.send(Request::EventStream).map_err(|error| {
            TrackerError::with_source(
                TrackerErrorKind::Io,
                "failed to subscribe to niri event stream",
                error,
            )
        })?;
        match reply {
            Ok(Response::Handled) => {}
            Ok(response) => {
                return Err(TrackerError::new(
                    TrackerErrorKind::Protocol,
                    format!("unexpected niri event-stream response: {response:?}"),
                ));
            }
            Err(message) => {
                return Err(TrackerError::new(
                    TrackerErrorKind::Protocol,
                    format!("niri rejected event-stream subscription: {message}"),
                ));
            }
        }

        Ok(Box::new(NiriEventSource::spawn(socket)))
    }
}

struct NiriEventSource {
    receiver: Receiver<Result<Update, TrackerError>>,
    received_initial_snapshot: bool,
}

impl NiriEventSource {
    fn spawn(socket: Socket) -> Self {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut read_event = socket.read_events();
            loop {
                match read_event() {
                    Ok(event) => match normalize_event(event) {
                        Ok(Some(update)) => {
                            if sender.send(Ok(update)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            return;
                        }
                    },
                    Err(error) => {
                        let _ = sender.send(Err(TrackerError::with_source(
                            TrackerErrorKind::Disconnected,
                            "niri event stream closed",
                            error,
                        )));
                        return;
                    }
                }
            }
        });
        Self {
            receiver,
            received_initial_snapshot: false,
        }
    }
}

impl WindowEventSource for NiriEventSource {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Update>, TrackerError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(update)) => {
                if !self.received_initial_snapshot {
                    if !matches!(update, Update::Snapshot(_)) {
                        return Err(TrackerError::new(
                            TrackerErrorKind::Protocol,
                            "niri sent an incremental window update before its initial snapshot",
                        ));
                    }
                    self.received_initial_snapshot = true;
                }
                Ok(Some(update))
            }
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(TrackerError::new(
                TrackerErrorKind::Disconnected,
                "niri event reader stopped unexpectedly",
            )),
        }
    }
}

fn normalize_event(event: Event) -> Result<Option<Update>, TrackerError> {
    let update = match event {
        Event::WindowsChanged { windows } => Update::Snapshot(normalize_snapshot(windows)?),
        Event::WindowOpenedOrChanged { window } => {
            let focused = window.is_focused;
            Update::Upsert {
                window: normalize_window(window),
                focused,
            }
        }
        Event::WindowClosed { id } => Update::Closed(normalize_id(id)),
        Event::WindowFocusChanged { id } => Update::Focused(id.map(normalize_id)),
        _ => return Ok(None),
    };
    Ok(Some(update))
}

fn normalize_snapshot(windows: Vec<Window>) -> Result<Snapshot, TrackerError> {
    let focused = windows
        .iter()
        .filter(|window| window.is_focused)
        .map(|window| normalize_id(window.id))
        .collect::<Vec<_>>();
    let focused = match focused.as_slice() {
        [] => None,
        [id] => Some(id.clone()),
        _ => {
            return Err(TrackerError::new(
                TrackerErrorKind::Protocol,
                "niri snapshot contains more than one focused window",
            ));
        }
    };
    Ok(Snapshot {
        windows: windows.into_iter().map(normalize_window).collect(),
        focused,
    })
}

fn normalize_window(window: Window) -> WindowInfo {
    WindowInfo {
        id: normalize_id(window.id),
        app_id: window.app_id.unwrap_or_else(|| "unknown".to_owned()),
        title: window.title.unwrap_or_default(),
        pid: window.pid,
    }
}

fn normalize_id(id: u64) -> WindowId {
    WindowId::new(format!("niri:{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64, focused: bool) -> Window {
        Window {
            id,
            title: Some(format!("window {id}")),
            app_id: Some("app".to_owned()),
            pid: Some(42),
            workspace_id: None,
            is_focused: focused,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: None,
                tile_size: (0.0, 0.0),
                window_size: (0, 0),
                tile_pos_in_workspace_view: None,
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    #[test]
    fn snapshot_normalizes_zero_or_one_focus() {
        let none = normalize_snapshot(vec![window(1, false)]).unwrap();
        assert_eq!(none.focused, None);

        let one = normalize_snapshot(vec![window(1, true)]).unwrap();
        assert_eq!(one.focused, Some(normalize_id(1)));
    }

    #[test]
    fn snapshot_rejects_multiple_focused_windows() {
        let error = normalize_snapshot(vec![window(1, true), window(2, true)]).unwrap_err();
        assert_eq!(error.kind(), TrackerErrorKind::Protocol);
    }

    #[test]
    fn upsert_preserves_focus_and_normalizes_absent_metadata() {
        let mut native = window(u64::MAX, true);
        native.title = None;
        native.app_id = None;
        native.pid = None;
        let Some(Update::Upsert { window, focused }) =
            normalize_event(Event::WindowOpenedOrChanged { window: native }).unwrap()
        else {
            panic!("expected upsert");
        };
        assert!(focused);
        assert_eq!(window.id, normalize_id(u64::MAX));
        assert_eq!(window.app_id, "unknown");
        assert_eq!(window.title, "");
        assert_eq!(window.pid, None);
    }
}
