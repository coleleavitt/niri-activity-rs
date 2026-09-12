//! KDE Plasma 6 adapter for the installed KWin metadata bridge.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use serde::Deserialize;
use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::names::{BusName, OwnedUniqueName};

use super::{
    Snapshot, TrackerError, TrackerErrorKind, Update, WindowEventSource, WindowId, WindowInfo,
    WindowTracker,
};

const KWIN_SERVICE: &str = "org.kde.KWin";
const RECEIVER_SERVICE: &str = "io.github.coleleavitt.NiriActivityRs.KWin";
const RECEIVER_PATH: &str = "/io/github/coleleavitt/NiriActivityRs/KWin";
const PROTOCOL: u32 = 1;
const QUEUE_CAPACITY: usize = 256;
const MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
const MAX_WINDOWS: usize = 2048;
const MAX_GENERATION_BYTES: usize = 128;
const MAX_ID_BYTES: usize = 1024;
const MAX_TITLE_BYTES: usize = 4096;
const INITIAL_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);

/// Factory for the installed KDE Plasma 6 KWin script bridge.
#[derive(Debug, Default, Clone, Copy)]
pub struct KdeTracker;

impl WindowTracker for KdeTracker {
    fn backend_name(&self) -> &'static str {
        "kde"
    }

    fn connect(&self) -> Result<Box<dyn WindowEventSource>, TrackerError> {
        let connection = Builder::session()
            .map_err(unavailable)?
            .method_timeout(Duration::from_secs(5))
            .build()
            .map_err(unavailable)?;
        let owner = kwin_owner(&connection)?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let failed = Arc::new(AtomicBool::new(false));
        connection
            .object_server()
            .at(
                RECEIVER_PATH,
                KdeReceiver {
                    owner: owner.clone(),
                    sender,
                    failed: Arc::clone(&failed),
                },
            )
            .map_err(|error| {
                TrackerError::with_source(
                    TrackerErrorKind::Io,
                    "failed to export KWin receiver",
                    error,
                )
            })?;
        let reply = connection
            .request_name_with_flags(RECEIVER_SERVICE, RequestNameFlags::DoNotQueue.into())
            .map_err(unavailable)?;
        if reply != RequestNameReply::PrimaryOwner && reply != RequestNameReply::AlreadyOwner {
            return Err(TrackerError::new(
                TrackerErrorKind::Unavailable,
                format!("KWin receiver D-Bus name is already owned: {reply}"),
            ));
        }

        let mut source = KdeEventSource {
            connection,
            owner,
            receiver,
            failed,
            state: ProtocolState::default(),
            pending_snapshot: None,
        };
        let deadline = Instant::now() + INITIAL_SNAPSHOT_TIMEOUT;
        let remaining = deadline.saturating_duration_since(Instant::now());
        match source.recv_timeout(remaining)? {
            Some(Update::Snapshot(snapshot)) => {
                source.pending_snapshot = Some(Update::Snapshot(snapshot));
                Ok(Box::new(source))
            }
            Some(_) => Err(protocol("KWin sent an event before its initial snapshot")),
            None => Err(TrackerError::new(
                TrackerErrorKind::Unavailable,
                "timed out waiting for a complete KWin snapshot",
            )),
        }
    }
}

fn unavailable(error: zbus::Error) -> TrackerError {
    TrackerError::with_source(
        TrackerErrorKind::Unavailable,
        "KDE KWin D-Bus bridge is unavailable",
        error,
    )
}

fn kwin_owner(connection: &Connection) -> Result<OwnedUniqueName, TrackerError> {
    let proxy = zbus::blocking::fdo::DBusProxy::new(connection).map_err(unavailable)?;
    proxy
        .get_name_owner(BusName::try_from(KWIN_SERVICE).expect("static bus name is valid"))
        .map_err(|error| {
            TrackerError::with_source(
                TrackerErrorKind::Unavailable,
                "org.kde.KWin has no current D-Bus owner",
                error,
            )
        })
}

struct KdeReceiver {
    owner: OwnedUniqueName,
    sender: SyncSender<Inbound>,
    failed: Arc<AtomicBool>,
}

#[zbus::interface(name = "io.github.coleleavitt.NiriActivityRs.KWin1")]
impl KdeReceiver {
    #[zbus(name = "Snapshot")]
    fn snapshot(&self, payload: &str, #[zbus(header)] header: Header<'_>) {
        self.accept(payload, header.sender().map(ToString::to_string), true);
    }

    #[zbus(name = "Event")]
    fn event(&self, payload: &str, #[zbus(header)] header: Header<'_>) {
        self.accept(payload, header.sender().map(ToString::to_string), false);
    }
}

impl KdeReceiver {
    fn accept(&self, payload: &str, sender: Option<String>, snapshot: bool) {
        if sender.as_deref() != Some(self.owner.as_str()) || payload.len() > MAX_PAYLOAD_BYTES {
            self.reject();
            return;
        }
        let inbound = if snapshot {
            parse_snapshot(payload)
        } else {
            parse_event(payload)
        };
        let Ok(inbound) = inbound else {
            self.reject();
            return;
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.sender.try_send(inbound)
        {
            self.failed.store(true, Ordering::Release);
        }
    }

    fn reject(&self) {
        self.failed.store(true, Ordering::Release);
        let _ = self.sender.try_send(Inbound::Failed);
    }
}

struct KdeEventSource {
    connection: Connection,
    owner: OwnedUniqueName,
    receiver: Receiver<Inbound>,
    failed: Arc<AtomicBool>,
    state: ProtocolState,
    pending_snapshot: Option<Update>,
}

impl WindowEventSource for KdeEventSource {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Update>, TrackerError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(protocol("KWin receiver rejected input or overflowed"));
        }
        if kwin_owner(&self.connection).map_or(true, |owner| owner != self.owner) {
            return Err(TrackerError::new(
                TrackerErrorKind::Disconnected,
                "KWin D-Bus owner changed or disappeared",
            ));
        }
        if let Some(update) = self.pending_snapshot.take() {
            return Ok(Some(update));
        }
        let inbound = match self.receiver.recv_timeout(timeout) {
            Ok(value) => value,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(TrackerError::new(
                    TrackerErrorKind::Disconnected,
                    "KWin receiver stopped unexpectedly",
                ));
            }
        };
        if self.failed.load(Ordering::Acquire) {
            return Err(protocol("KWin receiver queue overflowed"));
        }
        self.state.reduce(inbound).map(Some)
    }
}

#[derive(Default)]
struct ProtocolState {
    generation: Option<String>,
    next_seq: Option<u64>,
}

impl ProtocolState {
    fn reduce(&mut self, inbound: Inbound) -> Result<Update, TrackerError> {
        match inbound {
            Inbound::Snapshot(snapshot) => {
                if !snapshot.complete {
                    return Err(TrackerError::new(
                        TrackerErrorKind::Unavailable,
                        "KWin reported an incomplete window snapshot",
                    ));
                }
                self.generation = Some(snapshot.generation);
                self.next_seq = snapshot.seq.checked_add(1);
                Ok(Update::Snapshot(normalize_snapshot(
                    snapshot.windows,
                    snapshot.active_uuid,
                )?))
            }
            Inbound::Event(event) => {
                if self.generation.as_deref() != Some(&event.generation)
                    || self.next_seq != Some(event.seq)
                {
                    return Err(protocol(
                        "KWin event generation or sequence is not contiguous",
                    ));
                }
                self.next_seq = event.seq.checked_add(1);
                normalize_incremental(event)
            }
            Inbound::Failed => Err(protocol(
                "KWin receiver rejected malformed or unauthenticated input",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotPayload {
    protocol: u32,
    generation: String,
    seq: u64,
    complete: bool,
    windows: Vec<WindowRecord>,
    active_uuid: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventPayload {
    protocol: u32,
    generation: String,
    seq: u64,
    event: EventKind,
    window: Option<WindowRecord>,
    active_uuid: Option<String>,
}
#[derive(Deserialize)]
enum EventKind {
    #[serde(rename = "window_added")]
    Added,
    #[serde(rename = "window_changed")]
    Changed,
    #[serde(rename = "window_removed")]
    Removed,
    #[serde(rename = "window_activated")]
    Activated,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowRecord {
    uuid: String,
    pid: i64,
    app_id: String,
    resource_class: String,
    resource_name: String,
    title: String,
    active: bool,
}
enum Inbound {
    Snapshot(SnapshotPayload),
    Event(EventPayload),
    Failed,
}

fn parse_snapshot(payload: &str) -> Result<Inbound, ()> {
    let parsed: SnapshotPayload = serde_json::from_str(payload).map_err(|_| ())?;
    validate_common(parsed.protocol, &parsed.generation)?;
    if parsed.windows.len() > MAX_WINDOWS {
        return Err(());
    }
    validate_active(parsed.active_uuid.as_deref())?;
    for window in &parsed.windows {
        validate_window(window)?;
    }
    Ok(Inbound::Snapshot(parsed))
}
fn parse_event(payload: &str) -> Result<Inbound, ()> {
    let parsed: EventPayload = serde_json::from_str(payload).map_err(|_| ())?;
    validate_common(parsed.protocol, &parsed.generation)?;
    validate_active(parsed.active_uuid.as_deref())?;
    if let Some(window) = &parsed.window {
        validate_window(window)?;
    }
    match parsed.event {
        EventKind::Activated => {}
        _ if parsed.window.is_none() => return Err(()),
        _ => {}
    }
    Ok(Inbound::Event(parsed))
}
fn validate_common(protocol: u32, generation: &str) -> Result<(), ()> {
    if protocol != PROTOCOL
        || generation.is_empty()
        || generation.len() > MAX_GENERATION_BYTES
        || generation.chars().any(char::is_control)
    {
        Err(())
    } else {
        Ok(())
    }
}
fn validate_window(window: &WindowRecord) -> Result<(), ()> {
    if !valid_uuid(&window.uuid)
        || window.pid < 0
        || window.pid > i64::from(i32::MAX)
        || window.app_id.len() > MAX_ID_BYTES
        || window.resource_class.len() > MAX_ID_BYTES
        || window.resource_name.len() > MAX_ID_BYTES
        || window.title.len() > MAX_TITLE_BYTES
    {
        Err(())
    } else {
        Ok(())
    }
}
fn validate_active(active: Option<&str>) -> Result<(), ()> {
    if active.is_none_or(valid_uuid) {
        Ok(())
    } else {
        Err(())
    }
}
fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
fn normalize_id(uuid: &str) -> WindowId {
    WindowId::new(format!("kde:{}", uuid.to_ascii_lowercase()))
}
fn normalize_window(record: WindowRecord) -> WindowInfo {
    WindowInfo {
        id: normalize_id(&record.uuid),
        app_id: record.app_id,
        title: record.title,
        pid: (record.pid != 0).then_some(record.pid as i32),
    }
}
fn normalize_snapshot(
    windows: Vec<WindowRecord>,
    active: Option<String>,
) -> Result<Snapshot, TrackerError> {
    let mut ids = HashSet::with_capacity(windows.len());
    for window in &windows {
        if !ids.insert(window.uuid.to_ascii_lowercase()) {
            return Err(protocol("KWin snapshot contains duplicate UUIDs"));
        }
    }
    if active
        .as_ref()
        .is_some_and(|uuid| !ids.contains(&uuid.to_ascii_lowercase()))
    {
        return Err(protocol(
            "KWin snapshot active UUID is not in its window set",
        ));
    }
    for window in &windows {
        let should_be_active = active
            .as_deref()
            .is_some_and(|uuid| uuid.eq_ignore_ascii_case(&window.uuid));
        if window.active != should_be_active {
            return Err(protocol(
                "KWin snapshot active flags disagree with active UUID",
            ));
        }
    }
    Ok(Snapshot {
        windows: windows.into_iter().map(normalize_window).collect(),
        focused: active.as_deref().map(normalize_id),
    })
}
fn normalize_incremental(event: EventPayload) -> Result<Update, TrackerError> {
    match event.event {
        EventKind::Added | EventKind::Changed => {
            let window = event
                .window
                .ok_or_else(|| protocol("KWin upsert omitted its full window record"))?;
            let focused = event
                .active_uuid
                .as_deref()
                .is_some_and(|active| active.eq_ignore_ascii_case(&window.uuid));
            if window.active != focused {
                return Err(protocol(
                    "KWin upsert active flag disagrees with active UUID",
                ));
            }
            Ok(Update::Upsert {
                window: normalize_window(window),
                focused,
            })
        }
        EventKind::Removed => Ok(Update::Closed(normalize_id(
            &event.window.expect("validated full removal record").uuid,
        ))),
        EventKind::Activated => {
            if let Some(window) = event.window.as_ref() {
                if event
                    .active_uuid
                    .as_deref()
                    .is_none_or(|active| !active.eq_ignore_ascii_case(&window.uuid))
                    || !window.active
                {
                    return Err(protocol(
                        "KWin activation record disagrees with active UUID",
                    ));
                }
            } else if event.active_uuid.is_some() {
                return Err(protocol("KWin activation omitted the active window record"));
            }
            Ok(Update::Focused(
                event.active_uuid.as_deref().map(normalize_id),
            ))
        }
    }
}
fn protocol(message: impl Into<Box<str>>) -> TrackerError {
    TrackerError::new(TrackerErrorKind::Protocol, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
    fn record() -> WindowRecord {
        WindowRecord {
            uuid: UUID.into(),
            pid: 42,
            app_id: "org.kde.konsole".into(),
            resource_class: "konsole".into(),
            resource_name: "konsole".into(),
            title: "shell".into(),
            active: true,
        }
    }
    fn snapshot(seq: u64) -> SnapshotPayload {
        SnapshotPayload {
            protocol: 1,
            generation: "g".into(),
            seq,
            complete: true,
            windows: vec![record()],
            active_uuid: Some(UUID.into()),
        }
    }
    fn state() -> ProtocolState {
        ProtocolState::default()
    }
    #[test]
    fn strict_json_rejects_unknown_fields_and_bad_uuid() {
        let json = r#"{"protocol":1,"generation":"g","seq":0,"complete":true,"windows":[],"active_uuid":null,"extra":1}"#.to_string();
        assert!(parse_snapshot(&json).is_err());
        let mut bad = record();
        bad.uuid = "not-a-uuid".into();
        assert!(validate_window(&bad).is_err());
    }
    #[test]
    fn snapshot_requires_unique_windows_and_known_focus() {
        assert!(normalize_snapshot(vec![record(), record()], None).is_err());
        assert!(
            normalize_snapshot(
                vec![record()],
                Some("00000000-0000-0000-0000-000000000000".into())
            )
            .is_err()
        );
    }
    #[test]
    fn reducer_enforces_generation_and_sequence() {
        let mut reducer = state();
        assert!(matches!(
            reducer.reduce(Inbound::Snapshot(snapshot(4))).unwrap(),
            Update::Snapshot(_)
        ));
        let event = EventPayload {
            protocol: 1,
            generation: "g".into(),
            seq: 6,
            event: EventKind::Changed,
            window: Some(record()),
            active_uuid: Some(UUID.into()),
        };
        assert_eq!(
            reducer.reduce(Inbound::Event(event)).unwrap_err().kind(),
            TrackerErrorKind::Protocol
        );
    }
    #[test]
    fn complete_snapshot_and_full_events_normalize() {
        let snapshot = normalize_snapshot(vec![record()], Some(UUID.into())).unwrap();
        assert_eq!(snapshot.focused, Some(normalize_id(UUID)));
        let event = EventPayload {
            protocol: 1,
            generation: "g".into(),
            seq: 1,
            event: EventKind::Added,
            window: Some(record()),
            active_uuid: Some(UUID.into()),
        };
        assert!(matches!(
            normalize_incremental(event).unwrap(),
            Update::Upsert { focused: true, .. }
        ));
    }
    #[test]
    fn spoof_and_queue_overflow_fail_closed() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.try_send(Inbound::Snapshot(snapshot(0))).unwrap();
        let failed = Arc::new(AtomicBool::new(false));
        let receiver_object = KdeReceiver {
            owner: OwnedUniqueName::try_from(":1.7").unwrap(),
            sender,
            failed: Arc::clone(&failed),
        };
        receiver_object.accept("{}", Some(":1.8".into()), true);
        assert!(failed.load(Ordering::Acquire));
        failed.store(false, Ordering::Release);
        let payload = serde_json::json!({"protocol":1,"generation":"g","seq":0,"complete":true,"windows":[],"active_uuid":null}).to_string();
        receiver_object.accept(&payload, Some(":1.7".into()), true);
        assert!(failed.load(Ordering::Acquire));
        drop(receiver);
    }

    #[test]
    fn event_wire_names_match_the_kwin_script_contract() {
        let window = serde_json::json!({
            "uuid": "11111111-1111-4111-8111-111111111111",
            "pid": 42,
            "app_id": "org.kde.konsole",
            "resource_class": "konsole",
            "resource_name": "konsole",
            "title": "shell",
            "active": false,
        });
        let payload = |event: &str, window: serde_json::Value| {
            serde_json::json!({
                "protocol": 1,
                "generation": "g",
                "seq": 1,
                "event": event,
                "window": window,
                "active_uuid": null,
            })
            .to_string()
        };
        for event in ["window_added", "window_changed", "window_removed"] {
            assert!(
                parse_event(&payload(event, window.clone())).is_ok(),
                "wire event {event}"
            );
        }
        assert!(parse_event(&payload("window_activated", serde_json::Value::Null)).is_ok());
        for unsupported in ["added", "changed", "removed", "activated"] {
            assert!(parse_event(&payload(unsupported, window.clone())).is_err());
        }
    }
}
