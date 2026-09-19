//! GNOME Shell extension D-Bus adapter.
//!
//! This module talks only to the narrow bridge in
//! `contrib/gnome-shell-extension`. It deliberately does not use Shell's `Eval`
//! or introspection APIs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use zbus::MatchRule;
use zbus::blocking::connection::Builder;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::OwnedValue;

use super::{
    Snapshot, TrackerError, TrackerErrorKind, Update, WindowEventSource, WindowId, WindowInfo,
    WindowTracker,
};

const BUS_NAME: &str = "io.github.coleleavitt.NiriActivity.Gnome";
const OBJECT_PATH: &str = "/io/github/coleleavitt/NiriActivity/Gnome";
const INTERFACE: &str = "io.github.coleleavitt.NiriActivity.Gnome1";
const PROTOCOL_VERSION: u32 = 1;
const CHANNEL_CAPACITY: usize = 32;
const MAX_FIELDS: usize = 8;
const MAX_TITLE_CHARS: usize = 4096;
const MAX_IDENTITY_CHARS: usize = 512;

/// Factory for the GNOME Shell extension bridge.
#[derive(Debug, Default, Clone, Copy)]
pub struct GnomeTracker;

impl WindowTracker for GnomeTracker {
    fn backend_name(&self) -> &'static str {
        "gnome"
    }

    fn connect(&self) -> Result<Box<dyn WindowEventSource>, TrackerError> {
        let connection = Builder::session()
            .map_err(|error| {
                TrackerError::with_source(
                    TrackerErrorKind::Unavailable,
                    "failed to configure the session D-Bus connection",
                    error,
                )
            })?
            .method_timeout(Duration::from_secs(5))
            .build()
            .map_err(|error| {
                TrackerError::with_source(
                    TrackerErrorKind::Unavailable,
                    "failed to connect to the session D-Bus",
                    error,
                )
            })?;
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));

        // Install both matches before resolving the owner or requesting a
        // snapshot. Start their blocking reader threads only after the
        // handshake succeeds, so failed auto-selection leaves no orphaned
        // D-Bus readers while preserving startup race closure.
        let active_messages = active_messages(&connection)?;
        let owner_messages = owner_messages(&connection)?;
        let owner = get_owner(&connection)?;
        check_protocol(&connection, &owner)?;
        let (generation, payload) = get_active_window(&connection, &owner)?;
        let window = parse_window(payload)?;
        let active_reader =
            spawn_active_reader(active_messages, sender.clone(), Arc::clone(&overflowed));
        let owner_reader = spawn_owner_reader(owner_messages, sender, Arc::clone(&overflowed));

        Ok(Box::new(GnomeEventSource {
            connection: Some(connection),
            readers: vec![active_reader, owner_reader],
            receiver,
            overflowed,
            state: Reducer::new(owner.to_string(), generation, window.clone()),
            initial: Some(Update::Snapshot(snapshot(window))),
        }))
    }
}

#[derive(Debug)]
enum BusEvent {
    Active {
        sender: String,
        generation: u64,
        payload: HashMap<String, OwnedValue>,
    },
    OwnerChanged {
        old_owner: String,
        new_owner: String,
    },
    Failed(TrackerError),
}

fn active_rule() -> Result<MatchRule<'static>, TrackerError> {
    MatchRule::builder()
        .msg_type(Type::Signal)
        .path(OBJECT_PATH)
        .and_then(|builder| builder.interface(INTERFACE))
        .and_then(|builder| builder.member("ActiveWindowChanged"))
        .map(|builder| builder.build().to_owned())
        .map_err(|error| {
            TrackerError::with_source(
                TrackerErrorKind::Protocol,
                "invalid GNOME bridge signal rule",
                error,
            )
        })
}

fn owner_rule() -> Result<MatchRule<'static>, TrackerError> {
    MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")
        .and_then(|builder| builder.interface("org.freedesktop.DBus"))
        .and_then(|builder| builder.member("NameOwnerChanged"))
        .and_then(|builder| builder.add_arg(BUS_NAME))
        .map(|builder| builder.build().to_owned())
        .map_err(|error| {
            TrackerError::with_source(
                TrackerErrorKind::Protocol,
                "invalid D-Bus owner signal rule",
                error,
            )
        })
}

fn send_event(
    sender: &mpsc::SyncSender<BusEvent>,
    overflowed: &AtomicBool,
    event: BusEvent,
) -> bool {
    match sender.try_send(event) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(_)) => {
            overflowed.store(true, Ordering::Release);
            false
        }
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    }
}

fn active_messages(connection: &Connection) -> Result<MessageIterator, TrackerError> {
    MessageIterator::for_match_rule(active_rule()?, connection, Some(CHANNEL_CAPACITY)).map_err(
        |error| {
            TrackerError::with_source(
                TrackerErrorKind::Io,
                "failed to subscribe to GNOME bridge signals",
                error,
            )
        },
    )
}

fn spawn_active_reader(
    messages: MessageIterator,
    sender: mpsc::SyncSender<BusEvent>,
    overflowed: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for message in messages {
            let result = (|| {
                let message = message.map_err(|error| {
                    TrackerError::with_source(
                        TrackerErrorKind::Disconnected,
                        "GNOME bridge signal stream failed",
                        error,
                    )
                })?;
                let header = message.header();
                let sender = header.sender().ok_or_else(|| {
                    TrackerError::new(
                        TrackerErrorKind::Protocol,
                        "GNOME bridge signal has no sender",
                    )
                })?;
                let (generation, payload) = message
                    .body()
                    .deserialize::<(u64, HashMap<String, OwnedValue>)>()
                    .map_err(|error| {
                        TrackerError::with_source(
                            TrackerErrorKind::Protocol,
                            "malformed GNOME bridge signal",
                            error,
                        )
                    })?;
                Ok(BusEvent::Active {
                    sender: sender.to_string(),
                    generation,
                    payload,
                })
            })();
            let event = result.unwrap_or_else(BusEvent::Failed);
            if !send_event(&sender, &overflowed, event) {
                return;
            }
        }
        send_event(
            &sender,
            &overflowed,
            BusEvent::Failed(TrackerError::new(
                TrackerErrorKind::Disconnected,
                "GNOME bridge signal reader stopped",
            )),
        );
    })
}

fn owner_messages(connection: &Connection) -> Result<MessageIterator, TrackerError> {
    MessageIterator::for_match_rule(owner_rule()?, connection, Some(4)).map_err(|error| {
        TrackerError::with_source(
            TrackerErrorKind::Io,
            "failed to watch GNOME bridge ownership",
            error,
        )
    })
}

fn spawn_owner_reader(
    messages: MessageIterator,
    sender: mpsc::SyncSender<BusEvent>,
    overflowed: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for message in messages {
            let result = message
                .map_err(|error| {
                    TrackerError::with_source(
                        TrackerErrorKind::Disconnected,
                        "D-Bus owner stream failed",
                        error,
                    )
                })
                .and_then(|message| {
                    message
                        .body()
                        .deserialize::<(String, String, String)>()
                        .map_err(|error| {
                            TrackerError::with_source(
                                TrackerErrorKind::Protocol,
                                "malformed D-Bus owner notification",
                                error,
                            )
                        })
                })
                .and_then(|(name, old_owner, new_owner)| {
                    if name != BUS_NAME {
                        return Err(protocol("unexpected D-Bus owner notification"));
                    }
                    Ok(BusEvent::OwnerChanged {
                        old_owner,
                        new_owner,
                    })
                });
            let event = result.unwrap_or_else(BusEvent::Failed);
            if !send_event(&sender, &overflowed, event) {
                return;
            }
        }
        send_event(
            &sender,
            &overflowed,
            BusEvent::Failed(TrackerError::new(
                TrackerErrorKind::Disconnected,
                "D-Bus owner reader stopped",
            )),
        );
    })
}

fn get_owner(connection: &Connection) -> Result<OwnedUniqueName, TrackerError> {
    let proxy = Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .map_err(|error| {
        TrackerError::with_source(TrackerErrorKind::Io, "failed to create D-Bus proxy", error)
    })?;
    proxy.call("GetNameOwner", &(BUS_NAME,)).map_err(|error| {
        TrackerError::with_source(
            TrackerErrorKind::Unavailable,
            "GNOME Shell extension bridge is not available",
            error,
        )
    })
}

fn bridge_proxy<'a>(
    connection: &Connection,
    owner: &OwnedUniqueName,
) -> Result<Proxy<'a>, TrackerError> {
    Proxy::new_owned(connection.clone(), owner.clone(), OBJECT_PATH, INTERFACE).map_err(|error| {
        TrackerError::with_source(
            TrackerErrorKind::Io,
            "failed to create GNOME bridge proxy",
            error,
        )
    })
}

fn check_protocol(connection: &Connection, owner: &OwnedUniqueName) -> Result<(), TrackerError> {
    let version: u32 = bridge_proxy(connection, owner)?
        .get_property("ProtocolVersion")
        .map_err(|error| {
            TrackerError::with_source(
                TrackerErrorKind::Protocol,
                "failed to read GNOME bridge protocol version",
                error,
            )
        })?;
    if version != PROTOCOL_VERSION {
        return Err(TrackerError::new(
            TrackerErrorKind::Protocol,
            format!("unsupported GNOME bridge protocol version {version}"),
        ));
    }
    Ok(())
}

fn get_active_window(
    connection: &Connection,
    owner: &OwnedUniqueName,
) -> Result<(u64, HashMap<String, OwnedValue>), TrackerError> {
    bridge_proxy(connection, owner)?
        .call("GetActiveWindow", &())
        .map_err(|error| {
            TrackerError::with_source(
                TrackerErrorKind::Protocol,
                "failed to get GNOME active window",
                error,
            )
        })
}

fn parse_window(
    mut fields: HashMap<String, OwnedValue>,
) -> Result<Option<WindowInfo>, TrackerError> {
    if fields.is_empty() {
        return Ok(None);
    }
    if fields.len() > MAX_FIELDS {
        return Err(protocol("GNOME active-window payload has too many fields"));
    }
    let id = take::<u64>(&mut fields, "window-id")?;
    let title = take::<String>(&mut fields, "title")?;
    let app_id = take::<String>(&mut fields, "app-id")?;
    let identity_source = take::<String>(&mut fields, "identity-source")?;
    check_string(&title, MAX_TITLE_CHARS, "title")?;
    check_string(&app_id, MAX_IDENTITY_CHARS, "app-id")?;
    check_string(&identity_source, MAX_IDENTITY_CHARS, "identity-source")?;
    if app_id.is_empty() || identity_source.is_empty() {
        return Err(protocol("GNOME active-window identity is empty"));
    }
    for key in ["wm-class", "sandboxed-app-id", "gtk-application-id"] {
        if let Some(value) = fields.remove(key) {
            let value = String::try_from(value)
                .map_err(|_| protocol("GNOME active-window optional field has wrong type"))?;
            check_string(&value, MAX_IDENTITY_CHARS, key)?;
        }
    }
    if let Some(value) = fields.remove("client-type") {
        u32::try_from(value)
            .map_err(|_| protocol("GNOME active-window client-type has wrong type"))?;
    }
    if !fields.is_empty() {
        return Err(protocol(
            "GNOME active-window payload contains an unknown field",
        ));
    }
    Ok(Some(WindowInfo {
        id: WindowId::new(format!("gnome:{id}")),
        app_id,
        title,
        pid: None,
    }))
}

fn take<T>(fields: &mut HashMap<String, OwnedValue>, key: &str) -> Result<T, TrackerError>
where
    T: TryFrom<OwnedValue>,
{
    fields
        .remove(key)
        .ok_or_else(|| protocol(format!("GNOME active-window payload is missing {key}")))?
        .try_into()
        .map_err(|_| protocol(format!("GNOME active-window {key} has wrong type")))
}

fn check_string(value: &str, max_chars: usize, field: &str) -> Result<(), TrackerError> {
    if value.chars().count() > max_chars || value.chars().any(char::is_control) {
        return Err(protocol(format!("GNOME active-window {field} is invalid")));
    }
    Ok(())
}

fn snapshot(window: Option<WindowInfo>) -> Snapshot {
    match window {
        Some(window) => Snapshot {
            focused: Some(window.id.clone()),
            windows: vec![window],
        },
        None => Snapshot {
            focused: None,
            windows: Vec::new(),
        },
    }
}

fn protocol(message: impl Into<Box<str>>) -> TrackerError {
    TrackerError::new(TrackerErrorKind::Protocol, message)
}

struct Reducer {
    owner: String,
    generation: u64,
    window: Option<WindowInfo>,
}

impl Reducer {
    fn new(owner: String, generation: u64, window: Option<WindowInfo>) -> Self {
        Self {
            owner,
            generation,
            window,
        }
    }

    fn apply(&mut self, event: BusEvent) -> Result<Option<Update>, TrackerError> {
        match event {
            BusEvent::Failed(error) => Err(error),
            BusEvent::OwnerChanged {
                old_owner,
                new_owner,
            } => {
                if old_owner == self.owner || new_owner != self.owner {
                    Err(TrackerError::new(
                        TrackerErrorKind::Disconnected,
                        "GNOME bridge owner changed",
                    ))
                } else {
                    Ok(None)
                }
            }
            BusEvent::Active {
                sender,
                generation,
                payload,
            } => {
                if sender != self.owner {
                    return Err(protocol("GNOME bridge signal came from the wrong owner"));
                }
                if !is_newer(generation, self.generation) {
                    return Ok(None);
                }
                let window = parse_window(payload)?;
                self.generation = generation;
                self.window.clone_from(&window);
                Ok(Some(match window {
                    Some(window) => Update::Upsert {
                        window,
                        focused: true,
                    },
                    None => Update::Focused(None),
                }))
            }
        }
    }
}

fn is_newer(candidate: u64, current: u64) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1_u64 << 63)
}

struct GnomeEventSource {
    connection: Option<Connection>,
    readers: Vec<thread::JoinHandle<()>>,
    receiver: mpsc::Receiver<BusEvent>,
    overflowed: Arc<AtomicBool>,
    state: Reducer,
    initial: Option<Update>,
}

impl Drop for GnomeEventSource {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = connection.close();
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

impl WindowEventSource for GnomeEventSource {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Update>, TrackerError> {
        if self.overflowed.load(Ordering::Acquire) {
            return Err(TrackerError::new(
                TrackerErrorKind::Disconnected,
                "GNOME bridge event queue overflowed",
            ));
        }
        if let Some(initial) = self.initial.take() {
            return Ok(Some(initial));
        }
        loop {
            let event = match self.receiver.recv_timeout(timeout) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(TrackerError::new(
                        TrackerErrorKind::Disconnected,
                        "GNOME bridge readers stopped",
                    ));
                }
            };
            if let Some(update) = self.state.apply(event)? {
                return Ok(Some(update));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use zbus::zvariant::Value;

    use super::*;

    fn payload(id: u64, title: &str) -> HashMap<String, OwnedValue> {
        HashMap::from([
            ("window-id".to_owned(), OwnedValue::from(id)),
            (
                "title".to_owned(),
                OwnedValue::try_from(Value::from(title)).unwrap(),
            ),
            (
                "app-id".to_owned(),
                OwnedValue::try_from(Value::from("org.example.App")).unwrap(),
            ),
            (
                "identity-source".to_owned(),
                OwnedValue::try_from(Value::from("shell-app-id")).unwrap(),
            ),
        ])
    }

    #[test]
    fn parser_normalizes_focused_and_empty_snapshots() {
        let window = parse_window(payload(7, "Document")).unwrap().unwrap();
        assert_eq!(window.id, WindowId::test("gnome:7"));
        assert_eq!(window.title, "Document");
        assert_eq!(snapshot(Some(window)).windows.len(), 1);
        assert_eq!(
            snapshot(parse_window(HashMap::new()).unwrap()).focused,
            None
        );
    }

    #[test]
    fn parser_rejects_missing_wrong_unknown_and_oversized_fields() {
        let mut missing = payload(1, "title");
        missing.remove("app-id");
        assert_eq!(
            parse_window(missing).unwrap_err().kind(),
            TrackerErrorKind::Protocol
        );

        let mut wrong = payload(1, "title");
        wrong.insert("window-id".to_owned(), OwnedValue::from(4_u32));
        assert!(parse_window(wrong).is_err());

        let mut unknown = payload(1, "title");
        unknown.insert("secret".to_owned(), OwnedValue::from(1_u32));
        assert!(parse_window(unknown).is_err());
        assert!(parse_window(payload(1, &"x".repeat(MAX_TITLE_CHARS + 1))).is_err());
    }

    #[test]
    fn generation_order_accepts_forward_and_wrap_but_drops_stale() {
        assert!(is_newer(11, 10));
        assert!(!is_newer(10, 10));
        assert!(!is_newer(9, 10));
        assert!(is_newer(0, u64::MAX));
        assert!(!is_newer(1_u64 << 63, 0));
    }

    #[test]
    fn reducer_verifies_owner_and_emits_full_updates() {
        let mut reducer = Reducer::new(":1.2".to_owned(), 3, None);
        assert!(
            reducer
                .apply(BusEvent::Active {
                    sender: ":1.9".to_owned(),
                    generation: 4,
                    payload: payload(2, "bad")
                })
                .is_err()
        );
        let update = reducer
            .apply(BusEvent::Active {
                sender: ":1.2".to_owned(),
                generation: 4,
                payload: payload(2, "good"),
            })
            .unwrap();
        assert!(matches!(update, Some(Update::Upsert { focused: true, .. })));
        assert!(
            reducer
                .apply(BusEvent::Active {
                    sender: ":1.2".to_owned(),
                    generation: 4,
                    payload: payload(2, "duplicate")
                })
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reader_failure_reaches_reducer_and_full_queue_sets_overflow() {
        let mut reducer = Reducer::new(":1.2".to_owned(), 0, None);
        let error = reducer
            .apply(BusEvent::Failed(TrackerError::new(
                TrackerErrorKind::Disconnected,
                "reader failed",
            )))
            .unwrap_err();
        assert_eq!(error.kind(), TrackerErrorKind::Disconnected);

        let (sender, receiver) = mpsc::sync_channel(1);
        let overflowed = AtomicBool::new(false);
        assert!(send_event(
            &sender,
            &overflowed,
            BusEvent::OwnerChanged {
                old_owner: String::new(),
                new_owner: String::new(),
            },
        ));
        assert!(!send_event(
            &sender,
            &overflowed,
            BusEvent::Failed(protocol("overflow")),
        ));
        assert!(overflowed.load(Ordering::Acquire));
        drop(receiver);
    }

    #[test]
    fn reducer_fails_closed_on_owner_loss_or_replacement() {
        for new_owner in ["", ":1.3"] {
            let mut reducer = Reducer::new(":1.2".to_owned(), 0, None);
            let error = reducer
                .apply(BusEvent::OwnerChanged {
                    old_owner: ":1.2".to_owned(),
                    new_owner: new_owner.to_owned(),
                })
                .unwrap_err();
            assert_eq!(error.kind(), TrackerErrorKind::Disconnected);
        }
    }
}
