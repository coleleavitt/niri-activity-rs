use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use inotify::{Inotify, WatchMask};

pub const BTN_MOUSE_RANGE: RangeInclusive<u16> = 272..=279;
pub const REL_X: u16 = 0;
pub const REL_Y: u16 = 1;
pub const REL_HWHEEL: u16 = 8;
pub const REL_WHEEL_HI_RES: u16 = 11;

#[derive(Debug, Clone, Copy)]
pub struct InputSnapshot {
    pub keystrokes: u64,
    pub mouse_clicks: u64,
    pub scroll_events: u64,
    pub mouse_distance: u64,
}

pub struct InputStats {
    last_activity_ms: Arc<AtomicU64>,
    keystrokes: Arc<AtomicU64>,
    mouse_clicks: Arc<AtomicU64>,
    scroll_events: Arc<AtomicU64>,
    mouse_distance: Arc<AtomicU64>,
}

impl InputStats {
    pub fn snapshot(&self) -> InputSnapshot {
        let keystrokes = self.keystrokes.swap(0, Ordering::Relaxed);
        let mouse_clicks = self.mouse_clicks.swap(0, Ordering::Relaxed);
        let scroll_events = self.scroll_events.swap(0, Ordering::Relaxed);
        let mouse_distance = self.mouse_distance.swap(0, Ordering::Relaxed);
        InputSnapshot {
            keystrokes,
            mouse_clicks,
            scroll_events,
            mouse_distance,
        }
    }

    pub fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms.load(Ordering::Relaxed)
    }
}

pub fn enumerate_input_devices() -> Vec<evdev::Device> {
    evdev::enumerate()
        .filter_map(|(path, device)| {
            let supported = device.supported_events();
            let has_keys = supported.contains(evdev::EventType::KEY);
            let has_relative = supported.contains(evdev::EventType::RELATIVE);
            let has_absolute = supported.contains(evdev::EventType::ABSOLUTE);

            if has_keys || has_relative || has_absolute {
                match evdev::Device::open(&path) {
                    Ok(dev) => {
                        let _ = dev.set_nonblocking(true);
                        Some(dev)
                    }
                    Err(_) => None,
                }
            } else {
                None
            }
        })
        .collect()
}

pub fn start_idle_monitor(start: Instant) -> InputStats {
    let stats = InputStats {
        last_activity_ms: Arc::new(AtomicU64::new(0)),
        keystrokes: Arc::new(AtomicU64::new(0)),
        mouse_clicks: Arc::new(AtomicU64::new(0)),
        scroll_events: Arc::new(AtomicU64::new(0)),
        mouse_distance: Arc::new(AtomicU64::new(0)),
    };

    let last_activity = Arc::clone(&stats.last_activity_ms);
    let keystrokes = Arc::clone(&stats.keystrokes);
    let mouse_clicks = Arc::clone(&stats.mouse_clicks);
    let scroll_events = Arc::clone(&stats.scroll_events);
    let mouse_distance = Arc::clone(&stats.mouse_distance);

    let devices_changed = Arc::new(AtomicBool::new(false));
    let devices_changed_clone = Arc::clone(&devices_changed);

    thread::spawn(move || {
        let mut inotify = match Inotify::init() {
            Ok(i) => i,
            Err(e) => {
                eprintln!(
                    "Warning: Failed to init inotify: {}. Device hotplug disabled.",
                    e
                );
                return;
            }
        };

        if let Err(e) = inotify
            .watches()
            .add("/dev/input", WatchMask::CREATE | WatchMask::DELETE)
        {
            eprintln!(
                "Warning: Failed to watch /dev/input: {}. Device hotplug disabled.",
                e
            );
            return;
        }

        let mut buffer = [0; 1024];
        loop {
            match inotify.read_events_blocking(&mut buffer) {
                Ok(events) => {
                    for event in events {
                        if let Some(name) = event.name {
                            let name_str = name.to_string_lossy();
                            if name_str.starts_with("event") {
                                eprintln!("Input device changed: {:?}", name);
                                devices_changed_clone.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("inotify error: {}", e);
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    });

    thread::spawn(move || {
        let mut devices = enumerate_input_devices();

        if devices.is_empty() {
            eprintln!("Warning: No input devices found. Idle detection disabled.");
            eprintln!("  (May need to add user to 'input' group: sudo usermod -aG input $USER)");
            return;
        }

        eprintln!("Monitoring {} input device(s) for activity", devices.len());

        let mut last_reenumerate = Instant::now();
        let mut last_mouse_event = Instant::now();
        let mut last_keyboard_event = Instant::now();

        const REENUMERATE_INTERVAL: Duration = Duration::from_secs(60);
        const STALE_MOUSE_THRESHOLD: Duration = Duration::from_secs(30);
        const REENUMERATE_COOLDOWN: Duration = Duration::from_secs(10);

        loop {
            let loop_now = Instant::now();

            if devices_changed.swap(false, Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
                let new_devices = enumerate_input_devices();
                eprintln!(
                    "Re-enumerated (hotplug): {} -> {} devices",
                    devices.len(),
                    new_devices.len()
                );
                devices = new_devices;
                last_reenumerate = loop_now;
                last_mouse_event = loop_now;
            }

            if loop_now.duration_since(last_reenumerate) >= REENUMERATE_INTERVAL {
                let new_devices = enumerate_input_devices();
                if new_devices.len() != devices.len() {
                    eprintln!(
                        "Re-enumerated (periodic): {} -> {} devices",
                        devices.len(),
                        new_devices.len()
                    );
                }
                devices = new_devices;
                last_reenumerate = loop_now;
            }

            if loop_now.duration_since(last_keyboard_event) < Duration::from_secs(5)
                && loop_now.duration_since(last_mouse_event) >= STALE_MOUSE_THRESHOLD
                && loop_now.duration_since(last_reenumerate) >= REENUMERATE_COOLDOWN
            {
                eprintln!(
                    "Stale mouse detected (no mouse events for {}s, keyboard active). Re-enumerating...",
                    loop_now.duration_since(last_mouse_event).as_secs()
                );
                devices = enumerate_input_devices();
                last_reenumerate = loop_now;
                last_mouse_event = loop_now;
            }

            for device in &mut devices {
                if let Ok(events) = device.fetch_events() {
                    for ev in events {
                        let now = start.elapsed().as_millis() as u64;

                        match ev.event_type() {
                            evdev::EventType::KEY => {
                                if ev.value() == 1 {
                                    let code = ev.code();
                                    if BTN_MOUSE_RANGE.contains(&code) {
                                        mouse_clicks.fetch_add(1, Ordering::Relaxed);
                                        last_activity.store(now, Ordering::Relaxed);
                                        last_mouse_event = Instant::now();
                                    } else {
                                        keystrokes.fetch_add(1, Ordering::Relaxed);
                                        last_activity.store(now, Ordering::Relaxed);
                                        last_keyboard_event = Instant::now();
                                    }
                                }
                            }
                            evdev::EventType::RELATIVE => {
                                let code = ev.code();
                                if code == REL_X || code == REL_Y {
                                    mouse_distance.fetch_add(
                                        ev.value().unsigned_abs() as u64,
                                        Ordering::Relaxed,
                                    );
                                    last_activity.store(now, Ordering::Relaxed);
                                    last_mouse_event = Instant::now();
                                } else if code == REL_HWHEEL || code == REL_WHEEL_HI_RES {
                                    scroll_events.fetch_add(1, Ordering::Relaxed);
                                    last_activity.store(now, Ordering::Relaxed);
                                    last_mouse_event = Instant::now();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    });

    stats
}
