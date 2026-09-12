//! Niri IPC adapter. This is the only module that imports `niri_ipc`.

use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use backon::{BlockingRetryable, ExponentialBuilder};
#[cfg(test)]
use niri_ipc::WindowLayout;
use niri_ipc::{Event, Reply, Request, Response, Window};

use super::{
    Snapshot, TrackerError, TrackerErrorKind, Update, WindowEventSource, WindowId, WindowInfo,
    WindowTracker,
};

const BACKOFF_MAX_INTERVAL: Duration = Duration::from_secs(300);
const CONNECT_ATTEMPTS: usize = 20;
const INITIAL_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Factory for niri's blocking event-stream protocol.
#[derive(Debug, Clone, Copy)]
pub struct NiriTracker {
    attempts: usize,
}

impl NiriTracker {
    /// One-shot factory for runtime capability probing.
    pub const fn probe() -> Self {
        Self { attempts: 1 }
    }
}

impl Default for NiriTracker {
    fn default() -> Self {
        Self {
            attempts: CONNECT_ATTEMPTS,
        }
    }
}

impl WindowTracker for NiriTracker {
    fn backend_name(&self) -> &'static str {
        "niri"
    }

    fn connect(&self) -> Result<Box<dyn WindowEventSource>, TrackerError> {
        let backoff = ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(100))
            .with_max_delay(BACKOFF_MAX_INTERVAL)
            .with_max_times(self.attempts);
        let source = (|| connect_and_subscribe(INITIAL_SNAPSHOT_TIMEOUT))
            .retry(backoff)
            .sleep(thread::sleep)
            .call()
            .map_err(|error| {
                TrackerError::with_source(
                    TrackerErrorKind::Unavailable,
                    "failed to connect, subscribe, and receive niri's initial snapshot",
                    error,
                )
            })?;
        Ok(Box::new(source))
    }
}

fn connect_and_subscribe(timeout: Duration) -> io::Result<NiriEventSource> {
    let path = std::env::var_os("NIRI_SOCKET")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "NIRI_SOCKET is not set"))?;
    connect_and_subscribe_to(Path::new(&path), timeout)
}

fn connect_and_subscribe_to(path: &Path, timeout: Duration) -> io::Result<NiriEventSource> {
    let deadline = std::time::Instant::now() + timeout;
    let mut stream = connect_unix_socket(path, remaining(deadline)?)?;
    stream.set_write_timeout(Some(remaining(deadline)?))?;
    let mut request = serde_json::to_vec(&Request::EventStream).map_err(io::Error::from)?;
    request.push(b'\n');
    stream.write_all(&request)?;

    stream.set_read_timeout(Some(remaining(deadline)?))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "niri closed the subscription handshake",
        ));
    }
    let reply: Reply = serde_json::from_str(&line).map_err(io::Error::from)?;
    match reply {
        Ok(Response::Handled) => {}
        Ok(response) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected niri event-stream response: {response:?}"),
            ));
        }
        Err(message) => return Err(io::Error::other(message)),
    }

    reader
        .get_mut()
        .set_read_timeout(Some(remaining(deadline)?))?;
    let mut source = NiriEventSource::spawn(reader)?;
    let initial = source
        .recv_timeout(remaining(deadline)?)
        .map_err(|error| io::Error::other(error.to_string()))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for niri's initial window snapshot",
            )
        })?;
    source.pending = Some(initial);
    Ok(source)
}

fn remaining(deadline: std::time::Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "niri startup deadline elapsed",
        ))
    } else {
        Ok(remaining)
    }
}

fn connect_unix_socket(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::io::Errno;
    use rustix::net::sockopt::socket_error;
    use rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect, socket_with,
    };

    let fd = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .map_err(io::Error::from)?;
    let address = SocketAddrUnix::new(path).map_err(io::Error::from)?;
    match connect(&fd, &address) {
        Ok(()) => {}
        Err(error) if error == Errno::INPROGRESS || error == Errno::AGAIN => {
            let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
            let timeout = Timespec::try_from(timeout).unwrap_or(Timespec {
                tv_sec: i64::MAX,
                tv_nsec: 0,
            });
            if poll(&mut fds, Some(&timeout)).map_err(io::Error::from)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "niri socket connection timed out",
                ));
            }
            socket_error(&fd)
                .map_err(io::Error::from)?
                .map_err(io::Error::from)?;
        }
        Err(error) => return Err(io::Error::from(error)),
    }
    let mut flags = fcntl_getfl(&fd).map_err(io::Error::from)?;
    flags.remove(OFlags::NONBLOCK);
    fcntl_setfl(&fd, flags).map_err(io::Error::from)?;
    Ok(UnixStream::from(fd))
}

struct NiriEventSource {
    receiver: Receiver<Result<Update, TrackerError>>,
    received_initial_snapshot: bool,
    pending: Option<Update>,
    shutdown: UnixStream,
    worker: Option<thread::JoinHandle<()>>,
}

impl NiriEventSource {
    fn spawn(reader: BufReader<UnixStream>) -> io::Result<Self> {
        let shutdown = reader.get_ref().try_clone()?;
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("niri-events".to_owned())
            .spawn(move || {
                let mut reader = reader;
                let mut line = String::new();
                let mut first_update = true;
                loop {
                    line.clear();
                    let event = match reader.read_line(&mut line) {
                        Ok(0) => Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "niri event stream reached EOF",
                        )),
                        Ok(_) => serde_json::from_str::<Event>(&line).map_err(io::Error::from),
                        Err(error) => Err(error),
                    };
                    match event {
                        Ok(event) => match normalize_event(event) {
                            Ok(Some(update)) => {
                                if first_update {
                                    if !matches!(update, Update::Snapshot(_)) {
                                        let _ = sender.send(Err(TrackerError::new(
                                            TrackerErrorKind::Protocol,
                                            "niri sent an incremental update before its initial snapshot",
                                        )));
                                        return;
                                    }
                                    if let Err(error) = reader.get_mut().set_read_timeout(None) {
                                        let _ = sender.send(Err(TrackerError::with_source(
                                            TrackerErrorKind::Io,
                                            "failed to clear niri snapshot deadline",
                                            error,
                                        )));
                                        return;
                                    }
                                    first_update = false;
                                }
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
            })?;
        Ok(Self {
            receiver,
            received_initial_snapshot: false,
            pending: None,
            shutdown,
            worker: Some(worker),
        })
    }
}

impl Drop for NiriEventSource {
    fn drop(&mut self) {
        let _ = self.shutdown.shutdown(Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl WindowEventSource for NiriEventSource {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Update>, TrackerError> {
        if let Some(update) = self.pending.take() {
            return Ok(Some(update));
        }
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

    #[test]
    fn subscription_handshake_yields_and_replays_initial_snapshot() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("niri.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(matches!(
                serde_json::from_str::<Request>(&request).unwrap(),
                Request::EventStream
            ));
            let mut writer = stream;
            serde_json::to_writer(&mut writer, &Ok::<Response, String>(Response::Handled)).unwrap();
            writer.write_all(b"\n").unwrap();
            serde_json::to_writer(
                &mut writer,
                &Event::WindowsChanged {
                    windows: vec![window(7, true)],
                },
            )
            .unwrap();
            writer.write_all(b"\n").unwrap();
        });

        let mut source = connect_and_subscribe_to(&path, Duration::from_secs(1)).unwrap();
        let update = source.recv_timeout(Duration::from_secs(1)).unwrap();
        let Some(Update::Snapshot(snapshot)) = update else {
            panic!("expected initial snapshot");
        };
        assert_eq!(snapshot.focused, Some(normalize_id(7)));
        drop(source);
        server.join().unwrap();
    }
}
