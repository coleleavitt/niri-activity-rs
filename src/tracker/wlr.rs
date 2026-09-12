//! Native `wlr-foreign-toplevel-management` window tracker.
//!
//! The protocol is optional. Connecting probes the Wayland registry once and
//! returns `Unavailable` when the compositor does not advertise it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use std::{io, thread};

use wayland_client::globals::{self, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self, ZwlrForeignToplevelHandleV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self, ZwlrForeignToplevelManagerV1,
};

use super::{
    Snapshot, TrackerError, TrackerErrorKind, Update, WindowEventSource, WindowId, WindowInfo,
    WindowTracker,
};

const EVENT_QUEUE_CAPACITY: usize = 256;
const ACTIVATED_STATE: u32 = 2;
const INITIAL_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Factory for the optional WLR foreign-toplevel protocol.
#[derive(Debug, Default, Clone, Copy)]
pub struct WlrTracker;

impl WindowTracker for WlrTracker {
    fn backend_name(&self) -> &'static str {
        "wlr-toplevel"
    }

    fn connect(&self) -> Result<Box<dyn WindowEventSource>, TrackerError> {
        let deadline = std::time::Instant::now() + INITIAL_SNAPSHOT_TIMEOUT;
        let socket_path = wayland_socket_path(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )
        .map_err(Failure::into_error)?;
        let stream =
            connect_wayland_socket(&socket_path, INITIAL_SNAPSHOT_TIMEOUT).map_err(|error| {
                TrackerError::with_source(
                    TrackerErrorKind::Unavailable,
                    "failed to connect to Wayland display",
                    error,
                )
            })?;
        let shutdown = stream.try_clone().map_err(|error| {
            TrackerError::with_source(
                TrackerErrorKind::Io,
                "failed to create WLR shutdown handle",
                error,
            )
        })?;
        let (updates_tx, updates_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let terminal = Arc::new(Mutex::new(None));
        let worker_terminal = Arc::clone(&terminal);
        let worker = thread::Builder::new()
            .name("wlr-toplevel".to_owned())
            .spawn(move || run_worker(stream, updates_tx, startup_tx, worker_terminal))
            .map_err(|error| {
                TrackerError::with_source(
                    TrackerErrorKind::Io,
                    "failed to start WLR event reader",
                    error,
                )
            })?;

        match startup_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            Ok(Ok(())) => Ok(Box::new(WlrEventSource {
                receiver: updates_rx,
                terminal,
                received_initial_snapshot: false,
                shutdown,
                worker: Some(worker),
            })),
            Ok(Err(failure)) => {
                stop_worker(&shutdown, worker);
                Err(failure.into_error())
            }
            Err(RecvTimeoutError::Timeout) => {
                stop_worker(&shutdown, worker);
                Err(TrackerError::new(
                    TrackerErrorKind::Unavailable,
                    "timed out waiting for WLR initial window snapshot",
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                stop_worker(&shutdown, worker);
                Err(TrackerError::new(
                    TrackerErrorKind::Disconnected,
                    "WLR event reader stopped during initialization",
                ))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Failure {
    kind: TrackerErrorKind,
    message: String,
}

impl Failure {
    fn new(kind: TrackerErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn into_error(self) -> TrackerError {
        TrackerError::new(self.kind, self.message)
    }
}

struct WlrEventSource {
    receiver: Receiver<Update>,
    terminal: Arc<Mutex<Option<Failure>>>,
    received_initial_snapshot: bool,
    shutdown: UnixStream,
    worker: Option<thread::JoinHandle<()>>,
}

impl WlrEventSource {
    fn take_terminal(&self) -> Option<TrackerError> {
        self.terminal
            .lock()
            .expect("WLR terminal-state mutex was poisoned")
            .take()
            .map(Failure::into_error)
    }
}

fn stop_worker(shutdown: &UnixStream, worker: thread::JoinHandle<()>) {
    let _ = shutdown.shutdown(Shutdown::Both);
    let _ = worker.join();
}

impl Drop for WlrEventSource {
    fn drop(&mut self) {
        let _ = self.shutdown.shutdown(Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl WindowEventSource for WlrEventSource {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Update>, TrackerError> {
        if let Some(error) = self.take_terminal() {
            return Err(error);
        }

        let result = self.receiver.recv_timeout(timeout);
        if let Some(error) = self.take_terminal() {
            return Err(error);
        }

        match result {
            Ok(update) => {
                if !self.received_initial_snapshot {
                    if !matches!(update, Update::Snapshot(_)) {
                        return Err(TrackerError::new(
                            TrackerErrorKind::Protocol,
                            "WLR sent an incremental update before its initial snapshot",
                        ));
                    }
                    self.received_initial_snapshot = true;
                }
                Ok(Some(update))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(TrackerError::new(
                TrackerErrorKind::Disconnected,
                "WLR event reader stopped unexpectedly",
            )),
        }
    }
}

fn run_worker(
    stream: UnixStream,
    sender: SyncSender<Update>,
    startup: SyncSender<Result<(), Failure>>,
    terminal: Arc<Mutex<Option<Failure>>>,
) {
    let result = initialize_and_dispatch(stream, sender, Arc::clone(&terminal));
    match result {
        Ok(mut worker) => {
            if startup.send(Ok(())).is_err() {
                return;
            }
            while !worker.state.failed() {
                if let Err(error) = worker.queue.blocking_dispatch(&mut worker.state) {
                    worker.state.fail(Failure::new(
                        TrackerErrorKind::Disconnected,
                        format!("WLR Wayland transport failed: {error}"),
                    ));
                }
            }
        }
        Err(failure) => {
            let _ = startup.send(Err(failure.clone()));
            set_terminal(&terminal, failure);
        }
    }
}

struct Worker {
    queue: wayland_client::EventQueue<BackendState>,
    state: BackendState,
    _manager: ZwlrForeignToplevelManagerV1,
}

fn initialize_and_dispatch(
    stream: UnixStream,
    sender: SyncSender<Update>,
    terminal: Arc<Mutex<Option<Failure>>>,
) -> Result<Worker, Failure> {
    let connection = Connection::from_socket(stream).map_err(|error| {
        Failure::new(
            TrackerErrorKind::Unavailable,
            format!("failed to initialize Wayland connection: {error}"),
        )
    })?;
    let (globals, mut queue) =
        globals::registry_queue_init::<BackendState>(&connection).map_err(|error| {
            Failure::new(
                TrackerErrorKind::Io,
                format!("failed to read Wayland globals: {error}"),
            )
        })?;
    let mut state = BackendState::new(sender, terminal);
    state.initializing = true;
    let manager = globals
        .bind::<ZwlrForeignToplevelManagerV1, _, _>(&queue.handle(), 1..=3, ManagerData)
        .map_err(|error| match error {
            globals::BindError::NotPresent | globals::BindError::UnsupportedVersion => {
                Failure::new(
                    TrackerErrorKind::Unavailable,
                    format!("zwlr_foreign_toplevel_manager_v1 is unavailable: {error}"),
                )
            }
        })?;

    queue.roundtrip(&mut state).map_err(|error| {
        Failure::new(
            TrackerErrorKind::Disconnected,
            format!("WLR initial enumeration failed: {error}"),
        )
    })?;
    if let Some(failure) = state.failure() {
        return Err(failure);
    }
    state.emit(Update::Snapshot(state.reducer.snapshot()))?;
    state.initializing = false;

    Ok(Worker {
        queue,
        state,
        _manager: manager,
    })
}

fn wait_writable(
    fd: &impl std::os::fd::AsFd,
    timeout: Duration,
    timeout_message: &str,
) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    let mut fds = [PollFd::new(fd, PollFlags::OUT)];
    let timeout = Timespec::try_from(timeout).unwrap_or(Timespec {
        tv_sec: i64::MAX,
        tv_nsec: 0,
    });
    if poll(&mut fds, Some(&timeout)).map_err(io::Error::from)? == 0 {
        return Err(io::Error::new(io::ErrorKind::TimedOut, timeout_message));
    }
    Ok(())
}

fn connect_wayland_socket(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
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
            wait_writable(&fd, timeout, "Wayland socket connection timed out")?;
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

fn wayland_socket_path(
    inherited_socket: Option<OsString>,
    display: Option<OsString>,
    runtime_dir: Option<OsString>,
) -> Result<PathBuf, Failure> {
    if inherited_socket.is_some() {
        return Err(Failure::new(
            TrackerErrorKind::Unavailable,
            "WAYLAND_SOCKET cannot be consumed after watcher threads have started; use WAYLAND_DISPLAY",
        ));
    }
    let display = display
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Failure::new(TrackerErrorKind::Unavailable, "WAYLAND_DISPLAY is not set"))?;
    let display = PathBuf::from(display);
    if display.is_absolute() {
        return Ok(display);
    }
    if display.components().count() != 1 {
        return Err(Failure::new(
            TrackerErrorKind::Unavailable,
            "WAYLAND_DISPLAY must be an absolute path or a single socket name",
        ));
    }
    let runtime = runtime_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Failure::new(TrackerErrorKind::Unavailable, "XDG_RUNTIME_DIR is not set"))?;
    if !runtime.is_absolute() {
        return Err(Failure::new(
            TrackerErrorKind::Unavailable,
            "XDG_RUNTIME_DIR must be absolute",
        ));
    }
    if display == Path::new(".") || display == Path::new("..") {
        return Err(Failure::new(
            TrackerErrorKind::Unavailable,
            "WAYLAND_DISPLAY socket name is invalid",
        ));
    }
    Ok(runtime.join(display))
}

fn set_terminal(terminal: &Mutex<Option<Failure>>, failure: Failure) {
    let mut slot = terminal
        .lock()
        .expect("WLR terminal-state mutex was poisoned");
    if slot.is_none() {
        *slot = Some(failure);
    }
}

#[derive(Debug, Default)]
struct ManagerData;

#[derive(Debug, Default)]
struct HandleData {
    id: OnceLock<u64>,
}

struct BackendState {
    sender: SyncSender<Update>,
    terminal: Arc<Mutex<Option<Failure>>>,
    reducer: Reducer,
    initializing: bool,
}

impl BackendState {
    fn new(sender: SyncSender<Update>, terminal: Arc<Mutex<Option<Failure>>>) -> Self {
        Self {
            sender,
            terminal,
            reducer: Reducer::default(),
            initializing: false,
        }
    }

    fn id(&self, data: &HandleData) -> u64 {
        *data.id.get_or_init(|| self.reducer.allocate_id())
    }

    fn emit(&self, update: Update) -> Result<(), Failure> {
        match self.sender.try_send(update) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                let failure = Failure::new(
                    TrackerErrorKind::Protocol,
                    format!("WLR event queue overflowed its {EVENT_QUEUE_CAPACITY}-event bound"),
                );
                self.fail(failure.clone());
                Err(failure)
            }
            Err(TrySendError::Disconnected(_)) => {
                let failure = Failure::new(
                    TrackerErrorKind::Disconnected,
                    "WLR event consumer disconnected",
                );
                self.fail(failure.clone());
                Err(failure)
            }
        }
    }

    fn emit_all(&self, updates: Vec<Update>) {
        if self.initializing {
            return;
        }
        for update in updates {
            if self.emit(update).is_err() {
                break;
            }
        }
    }

    fn fail(&self, failure: Failure) {
        set_terminal(&self.terminal, failure);
    }

    fn failure(&self) -> Option<Failure> {
        self.terminal
            .lock()
            .expect("WLR terminal-state mutex was poisoned")
            .clone()
    }

    fn failed(&self) -> bool {
        self.terminal
            .lock()
            .expect("WLR terminal-state mutex was poisoned")
            .is_some()
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for BackendState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ManagerData> for BackendState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &ManagerData,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                let data = toplevel
                    .data::<HandleData>()
                    .expect("WLR child handle has the wrong user data");
                let id = state.id(data);
                state.reducer.open(id);
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => state.fail(Failure::new(
                TrackerErrorKind::Disconnected,
                "WLR foreign-toplevel manager finished",
            )),
            _ => {}
        }
    }

    wayland_client::event_created_child!(BackendState, ZwlrForeignToplevelManagerV1, [
        0 => (ZwlrForeignToplevelHandleV1, HandleData::default())
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, HandleData> for BackendState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        data: &HandleData,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if state.failed() {
            return;
        }
        let id = state.id(data);
        let updates = match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.reducer.title(id, title);
                Vec::new()
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.reducer.app_id(id, app_id);
                Vec::new()
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: bytes } => {
                match parse_activated(&bytes) {
                    Ok(activated) => state.reducer.activated(id, activated),
                    Err(failure) => state.fail(failure),
                }
                Vec::new()
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => state.reducer.done(id),
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                let updates = state.reducer.closed(id);
                proxy.destroy();
                updates
            }
            _ => Vec::new(),
        };
        state.emit_all(updates);
    }
}

#[derive(Debug, Default, Clone)]
struct Pending {
    title: Option<String>,
    app_id: Option<String>,
    activated: Option<bool>,
}

#[derive(Debug, Clone)]
struct Committed {
    title: String,
    app_id: String,
    activated: bool,
    activation_order: u64,
}

#[derive(Debug, Default)]
struct Reducer {
    next_id: AtomicU64,
    activation_clock: u64,
    pending: HashMap<u64, Pending>,
    committed: HashMap<u64, Committed>,
    focused: Option<u64>,
}

impl Reducer {
    fn allocate_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn open(&mut self, id: u64) {
        self.pending.entry(id).or_default();
    }

    fn pending(&mut self, id: u64) -> &mut Pending {
        self.pending.entry(id).or_default()
    }

    fn title(&mut self, id: u64, title: String) {
        self.pending(id).title = Some(title);
    }

    fn app_id(&mut self, id: u64, app_id: String) {
        self.pending(id).app_id = Some(app_id);
    }

    fn activated(&mut self, id: u64, activated: bool) {
        self.pending(id).activated = Some(activated);
    }

    fn done(&mut self, id: u64) -> Vec<Update> {
        let pending = self.pending.remove(&id).unwrap_or_default();
        let previous = self.committed.get(&id).cloned();
        let was_activated = previous.as_ref().is_some_and(|window| window.activated);
        let activated = pending
            .activated
            .unwrap_or_else(|| previous.as_ref().is_some_and(|window| window.activated));
        let activation_order = if activated && !was_activated {
            self.activation_clock += 1;
            self.activation_clock
        } else {
            previous
                .as_ref()
                .map_or(0, |window| window.activation_order)
        };
        let committed = Committed {
            title: pending
                .title
                .or_else(|| previous.as_ref().map(|window| window.title.clone()))
                .unwrap_or_default(),
            app_id: pending
                .app_id
                .or_else(|| previous.as_ref().map(|window| window.app_id.clone()))
                .unwrap_or_else(|| "unknown".to_owned()),
            activated,
            activation_order,
        };
        self.committed.insert(id, committed.clone());
        let old_focus = self.focused;
        self.focused = self.choose_focus();

        let mut updates = vec![Update::Upsert {
            window: window_info(id, &committed),
            focused: self.focused == Some(id),
        }];
        if old_focus != self.focused && self.focused != Some(id) {
            updates.push(Update::Focused(self.focused.map(window_id)));
        }
        updates
    }

    fn closed(&mut self, id: u64) -> Vec<Update> {
        self.pending.remove(&id);
        let existed = self.committed.remove(&id).is_some();
        let old_focus = self.focused;
        self.focused = self.choose_focus();
        if !existed {
            return Vec::new();
        }
        let mut updates = vec![Update::Closed(window_id(id))];
        if old_focus != self.focused {
            updates.push(Update::Focused(self.focused.map(window_id)));
        }
        updates
    }

    fn choose_focus(&self) -> Option<u64> {
        self.committed
            .iter()
            .filter(|(_, window)| window.activated)
            .max_by_key(|(id, window)| (window.activation_order, **id))
            .map(|(id, _)| *id)
    }

    fn snapshot(&self) -> Snapshot {
        let mut windows = self
            .committed
            .iter()
            .map(|(id, window)| (*id, window_info(*id, window)))
            .collect::<Vec<_>>();
        windows.sort_by_key(|(id, _)| *id);
        Snapshot {
            windows: windows.into_iter().map(|(_, window)| window).collect(),
            focused: self.focused.map(window_id),
        }
    }
}

fn window_id(id: u64) -> WindowId {
    WindowId::new(format!("wlr:{id}"))
}

fn window_info(id: u64, window: &Committed) -> WindowInfo {
    WindowInfo {
        id: window_id(id),
        app_id: window.app_id.clone(),
        title: window.title.clone(),
        pid: None,
    }
}

fn parse_activated(bytes: &[u8]) -> Result<bool, Failure> {
    let mut chunks = bytes.chunks_exact(size_of::<u32>());
    let activated = chunks
        .by_ref()
        .map(|chunk| u32::from_ne_bytes(chunk.try_into().expect("four-byte chunk")))
        .any(|state| state == ACTIVATED_STATE);
    if chunks.remainder().is_empty() {
        Ok(activated)
    } else {
        Err(Failure::new(
            TrackerErrorKind::Protocol,
            format!(
                "WLR state array has malformed {}-byte tail",
                chunks.remainder().len()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(states: &[u32]) -> Vec<u8> {
        states
            .iter()
            .flat_map(|state| state.to_ne_bytes())
            .collect()
    }

    #[test]
    fn state_parser_accepts_empty_known_mixed_and_unknown_values() {
        assert!(!parse_activated(&[]).unwrap());
        assert!(parse_activated(&bytes(&[2])).unwrap());
        assert!(parse_activated(&bytes(&[0, 99, 2, 3])).unwrap());
        assert!(!parse_activated(&bytes(&[0, 1, 3, u32::MAX])).unwrap());
    }

    #[test]
    fn state_parser_rejects_every_malformed_tail_length() {
        for tail in 1..=3 {
            let mut state = bytes(&[2, 99]);
            state.extend(std::iter::repeat_n(0xff, tail));
            let error = parse_activated(&state).unwrap_err();
            assert_eq!(error.kind, TrackerErrorKind::Protocol);
            assert!(error.message.contains(&format!("{tail}-byte tail")));
        }
    }

    #[test]
    fn reducer_stages_all_properties_until_done() {
        let mut reducer = Reducer::default();
        reducer.open(1);
        reducer.title(1, "Editor".to_owned());
        reducer.app_id(1, "code".to_owned());
        reducer.activated(1, true);
        assert_eq!(reducer.snapshot().windows, Vec::<WindowInfo>::new());

        let updates = reducer.done(1);
        assert_eq!(updates.len(), 1);
        let Update::Upsert { window, focused } = &updates[0] else {
            panic!("expected upsert");
        };
        assert_eq!(window.title, "Editor");
        assert_eq!(window.app_id, "code");
        assert!(focused);
    }

    #[test]
    fn done_emits_complete_metadata_not_partial_deltas() {
        let mut reducer = Reducer::default();
        reducer.title(7, "old".to_owned());
        reducer.app_id(7, "app".to_owned());
        reducer.done(7);
        reducer.title(7, "new".to_owned());

        let updates = reducer.done(7);
        let Update::Upsert { window, .. } = &updates[0] else {
            panic!("expected upsert");
        };
        assert_eq!(window.title, "new");
        assert_eq!(window.app_id, "app");
    }

    #[test]
    fn activation_removal_emits_upsert_then_explicit_focus_clear() {
        let mut reducer = Reducer::default();
        reducer.activated(1, true);
        reducer.done(1);
        reducer.activated(1, false);
        let updates = reducer.done(1);

        assert!(matches!(updates[0], Update::Upsert { focused: false, .. }));
        assert_eq!(updates[1], Update::Focused(None));
    }

    #[test]
    fn multiple_activated_handles_choose_latest_transition_deterministically() {
        let mut reducer = Reducer::default();
        reducer.activated(1, true);
        reducer.done(1);
        reducer.activated(2, true);
        reducer.done(2);
        assert_eq!(reducer.snapshot().focused, Some(window_id(2)));

        reducer.title(1, "unrelated title change".to_owned());
        let updates = reducer.done(1);
        assert!(matches!(updates[0], Update::Upsert { focused: false, .. }));
        assert_eq!(reducer.snapshot().focused, Some(window_id(2)));

        reducer.activated(2, false);
        let updates = reducer.done(2);
        assert_eq!(updates[1], Update::Focused(Some(window_id(1))));
    }

    #[test]
    fn closed_removes_pending_and_committed_state_and_updates_focus() {
        let mut reducer = Reducer::default();
        reducer.activated(1, true);
        reducer.done(1);
        reducer.activated(2, true);
        reducer.done(2);
        let updates = reducer.closed(2);
        assert_eq!(
            updates,
            [
                Update::Closed(window_id(2)),
                Update::Focused(Some(window_id(1)))
            ]
        );
        assert_eq!(reducer.snapshot().windows.len(), 1);
        assert!(!reducer.pending.contains_key(&2));
    }

    #[test]
    fn closing_unknown_handle_does_not_invent_an_update() {
        let mut reducer = Reducer::default();
        reducer.open(5);
        assert_eq!(reducer.closed(5), Vec::<Update>::new());
        assert!(reducer.pending.is_empty());
    }

    #[test]
    fn snapshot_is_authoritative_sorted_and_uses_defaults() {
        let mut reducer = Reducer::default();
        reducer.done(9);
        reducer.title(3, "three".to_owned());
        reducer.done(3);
        let snapshot = reducer.snapshot();
        assert_eq!(snapshot.windows[0].id, window_id(3));
        assert_eq!(snapshot.windows[1].id, window_id(9));
        assert_eq!(snapshot.windows[1].app_id, "unknown");
        assert_eq!(snapshot.windows[1].title, "");
        assert_eq!(snapshot.focused, None);
    }

    #[test]
    fn allocated_ids_are_monotonic_for_the_connection() {
        let reducer = Reducer::default();
        assert_eq!(reducer.allocate_id(), 1);
        assert_eq!(reducer.allocate_id(), 2);
        assert_eq!(reducer.allocate_id(), 3);
    }

    #[test]
    fn bounded_channel_overflow_sets_explicit_terminal_failure() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let terminal = Arc::new(Mutex::new(None));
        let state = BackendState::new(sender, Arc::clone(&terminal));
        state.emit(Update::Focused(None)).unwrap();
        let error = state.emit(Update::Focused(None)).unwrap_err();
        assert_eq!(error.kind, TrackerErrorKind::Protocol);
        assert!(error.message.contains("overflowed"));
        assert!(terminal.lock().unwrap().is_some());
    }

    #[test]
    fn socket_path_uses_absolute_display_directly() {
        assert_eq!(
            wayland_socket_path(None, Some("/tmp/wayland-test".into()), None).unwrap(),
            PathBuf::from("/tmp/wayland-test")
        );
    }

    #[test]
    fn socket_path_joins_single_name_to_absolute_runtime_dir() {
        assert_eq!(
            wayland_socket_path(
                None,
                Some("wayland-1".into()),
                Some("/run/user/1000".into())
            )
            .unwrap(),
            PathBuf::from("/run/user/1000/wayland-1")
        );
    }

    #[test]
    fn socket_path_rejects_late_inherited_fd_and_unsafe_paths() {
        assert!(
            wayland_socket_path(
                Some("7".into()),
                Some("wayland-1".into()),
                Some("/run/user/1000".into())
            )
            .is_err()
        );
        assert!(
            wayland_socket_path(
                None,
                Some("../wayland-1".into()),
                Some("/run/user/1000".into())
            )
            .is_err()
        );
        assert!(
            wayland_socket_path(None, Some("wayland-1".into()), Some("relative".into())).is_err()
        );
    }

    #[test]
    fn initialization_suppresses_upserts_until_snapshot_barrier() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let terminal = Arc::new(Mutex::new(None));
        let mut state = BackendState::new(sender, terminal);
        state.initializing = true;
        state.reducer.title(1, "Editor".to_owned());
        state.reducer.app_id(1, "code".to_owned());
        let updates = state.reducer.done(1);
        state.emit_all(updates);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        state
            .emit(Update::Snapshot(state.reducer.snapshot()))
            .unwrap();
        state.initializing = false;
        assert!(matches!(receiver.try_recv(), Ok(Update::Snapshot(_))));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_wait_respects_deadline() {
        use rustix::pipe::{PipeFlags, pipe_with};

        let (read_end, _write_end) = pipe_with(PipeFlags::CLOEXEC).unwrap();
        let started = std::time::Instant::now();
        let error =
            wait_writable(&read_end, Duration::from_millis(20), "test deadline").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
