use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use inotify::{Inotify, WatchMask};

use crate::config::JigglerConfig;

pub const BTN_MOUSE_RANGE: RangeInclusive<u16> = 272..=279;
pub const REL_X: u16 = 0;
pub const REL_Y: u16 = 1;
pub const REL_WHEEL: u16 = 8;
pub const REL_HWHEEL: u16 = 6;
pub const REL_WHEEL_HI_RES: u16 = 11;
pub const REL_HWHEEL_HI_RES: u16 = 12;

pub const ABS_X: u16 = 0;
pub const ABS_Y: u16 = 1;
pub const ABS_MT_POSITION_X: u16 = 53;
pub const ABS_MT_POSITION_Y: u16 = 54;

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
    jiggler_pattern: Arc<AtomicBool>,
    jiggler_process: Arc<AtomicBool>,
    last_keyboard_ms: Arc<AtomicU64>,
    last_meaningful_input_ms: Arc<AtomicU64>,
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

    #[allow(dead_code)]
    pub fn last_meaningful_input_ms(&self) -> u64 {
        self.last_meaningful_input_ms.load(Ordering::Relaxed)
    }

    pub fn jiggler_detected(&self) -> bool {
        self.jiggler_pattern.load(Ordering::Relaxed) || self.jiggler_process.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn jiggler_pattern_detected(&self) -> bool {
        self.jiggler_pattern.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn jiggler_process_detected(&self) -> bool {
        self.jiggler_process.load(Ordering::Relaxed)
    }
}

struct IntervalTracker {
    timestamps_ms: VecDeque<u64>,
    window_ms: u64,
    min_events: usize,
    variance_threshold_ms: u64,
}

impl IntervalTracker {
    fn new(config: &JigglerConfig) -> Self {
        Self {
            timestamps_ms: VecDeque::with_capacity(256),
            window_ms: config.window_secs.saturating_mul(1000),
            min_events: config.min_events,
            variance_threshold_ms: config.variance_threshold_ms,
        }
    }

    fn record(&mut self, now_ms: u64) {
        self.timestamps_ms.push_back(now_ms);
        let cutoff = now_ms.saturating_sub(self.window_ms);
        while let Some(&front) = self.timestamps_ms.front() {
            if front < cutoff {
                self.timestamps_ms.pop_front();
            } else {
                break;
            }
        }
    }

    fn is_artificial(&self) -> bool {
        if self.timestamps_ms.len() < self.min_events {
            return false;
        }

        let first = self.timestamps_ms.front().copied().unwrap_or(0);
        let last = self.timestamps_ms.back().copied().unwrap_or(0);
        let span_ms = last.saturating_sub(first);
        // Require events spread across at least 60 seconds — bursts after idle recovery
        // or rapid clicking shouldn't trigger jiggler detection.
        const MIN_SPAN_MS: u64 = 60_000;
        if span_ms < MIN_SPAN_MS {
            return false;
        }

        let mut min_interval = u64::MAX;
        let mut max_interval = 0u64;
        let mut has_intervals = false;

        let ts: Vec<u64> = self.timestamps_ms.iter().copied().collect();
        for pair in ts.windows(2) {
            let interval = pair[1].saturating_sub(pair[0]);
            if interval == 0 {
                continue;
            }
            has_intervals = true;
            if interval < min_interval {
                min_interval = interval;
            }
            if interval > max_interval {
                max_interval = interval;
            }
        }

        if !has_intervals {
            return false;
        }

        max_interval.saturating_sub(min_interval) < self.variance_threshold_ms
    }
}

pub fn scan_jiggler_processes(blacklist: &[String]) -> bool {
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let comm_path = entry.path().join("comm");
        if let Ok(comm) = std::fs::read_to_string(&comm_path) {
            let comm = comm.trim();
            if blacklist.iter().any(|b| comm.eq_ignore_ascii_case(b)) {
                return true;
            }
        }
    }
    false
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

pub fn start_idle_monitor(
    start: Instant,
    jiggler_config: JigglerConfig,
    mouse_idle_threshold: u64,
) -> InputStats {
    let stats = InputStats {
        last_activity_ms: Arc::new(AtomicU64::new(0)),
        keystrokes: Arc::new(AtomicU64::new(0)),
        mouse_clicks: Arc::new(AtomicU64::new(0)),
        scroll_events: Arc::new(AtomicU64::new(0)),
        mouse_distance: Arc::new(AtomicU64::new(0)),
        jiggler_pattern: Arc::new(AtomicBool::new(false)),
        jiggler_process: Arc::new(AtomicBool::new(false)),
        last_keyboard_ms: Arc::new(AtomicU64::new(0)),
        last_meaningful_input_ms: Arc::new(AtomicU64::new(0)),
    };

    let last_activity = Arc::clone(&stats.last_activity_ms);
    let keystrokes = Arc::clone(&stats.keystrokes);
    let mouse_clicks = Arc::clone(&stats.mouse_clicks);
    let scroll_events = Arc::clone(&stats.scroll_events);
    let mouse_distance = Arc::clone(&stats.mouse_distance);
    let jiggler_pattern_flag = Arc::clone(&stats.jiggler_pattern);
    let last_keyboard_ms = Arc::clone(&stats.last_keyboard_ms);
    let last_meaningful = Arc::clone(&stats.last_meaningful_input_ms);

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

    if jiggler_config.enabled {
        let jiggler_process_flag = Arc::clone(&stats.jiggler_process);
        let blacklist = jiggler_config.process_blacklist.clone();
        thread::spawn(move || {
            loop {
                let found = scan_jiggler_processes(&blacklist);
                jiggler_process_flag.store(found, Ordering::Relaxed);
                thread::sleep(Duration::from_secs(30));
            }
        });
    }

    let jiggler_enabled = jiggler_config.enabled;

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

        let mut kb_tracker = IntervalTracker::new(&jiggler_config);
        let mut mouse_tracker = IntervalTracker::new(&jiggler_config);
        let mut last_mouse_tracker_ms: u64 = 0;
        let mut last_jiggler_check = Instant::now();

        // Track signed displacement per axis so oscillatory tremor
        // (which cancels out) is distinguished from intentional motion.
        let mut motion_dx: i64 = 0;
        let mut motion_dy: i64 = 0;
        let mut motion_window_start_ms: u64 = 0;
        const MOTION_WINDOW_MS: u64 = 2000;

        const REENUMERATE_INTERVAL: Duration = Duration::from_secs(60);
        const STALE_MOUSE_THRESHOLD: Duration = Duration::from_secs(30);
        const REENUMERATE_COOLDOWN: Duration = Duration::from_secs(10);
        const JIGGLER_CHECK_INTERVAL: Duration = Duration::from_secs(10);

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
                                        last_meaningful.store(now, Ordering::Relaxed);
                                        last_mouse_event = Instant::now();
                                        if jiggler_enabled {
                                            mouse_tracker.record(now);
                                        }
                                    } else {
                                        keystrokes.fetch_add(1, Ordering::Relaxed);
                                        last_activity.store(now, Ordering::Relaxed);
                                        last_meaningful.store(now, Ordering::Relaxed);
                                        last_keyboard_event = Instant::now();
                                        last_keyboard_ms.store(now, Ordering::Relaxed);
                                        if jiggler_enabled {
                                            kb_tracker.record(now);
                                        }
                                    }
                                }
                            }
                            evdev::EventType::RELATIVE => {
                                let code = ev.code();
                                if code == REL_X || code == REL_Y {
                                    let delta = ev.value();
                                    mouse_distance
                                        .fetch_add(delta.unsigned_abs() as u64, Ordering::Relaxed);

                                    if code == REL_X {
                                        motion_dx = motion_dx.saturating_add(delta as i64);
                                    } else {
                                        motion_dy = motion_dy.saturating_add(delta as i64);
                                    }

                                    let window_expired = now.saturating_sub(motion_window_start_ms)
                                        > MOTION_WINDOW_MS;

                                    let net_sq = (motion_dx.saturating_mul(motion_dx))
                                        .saturating_add(motion_dy.saturating_mul(motion_dy));
                                    let threshold_sq = (mouse_idle_threshold as i64)
                                        .saturating_mul(mouse_idle_threshold as i64);
                                    let above_threshold = net_sq >= threshold_sq;

                                    if window_expired {
                                        if above_threshold {
                                            last_activity.store(now, Ordering::Relaxed);
                                        }
                                        motion_dx = 0;
                                        motion_dy = 0;
                                        motion_window_start_ms = now;
                                    } else if above_threshold {
                                        last_activity.store(now, Ordering::Relaxed);
                                        motion_dx = 0;
                                        motion_dy = 0;
                                        motion_window_start_ms = now;
                                    }

                                    last_mouse_event = Instant::now();
                                    if jiggler_enabled
                                        && now.saturating_sub(last_mouse_tracker_ms) >= 1000
                                    {
                                        mouse_tracker.record(now);
                                        last_mouse_tracker_ms = now;
                                    }
                                } else if code == REL_WHEEL || code == REL_HWHEEL {
                                    // Only count low-res wheel events; high-res
                                    // (REL_WHEEL_HI_RES / REL_HWHEEL_HI_RES) duplicate
                                    // the same physical scroll and would double-count.
                                    scroll_events.fetch_add(1, Ordering::Relaxed);
                                    last_activity.store(now, Ordering::Relaxed);
                                    last_mouse_event = Instant::now();
                                } else if code == REL_WHEEL_HI_RES
                                    || code == REL_HWHEEL_HI_RES
                                {
                                    // Still update activity timestamp for idle detection,
                                    // but don't increment scroll_events counter.
                                    last_activity.store(now, Ordering::Relaxed);
                                    last_mouse_event = Instant::now();
                                }
                            }
                            evdev::EventType::ABSOLUTE => {
                                let code = ev.code();
                                if code == ABS_X
                                    || code == ABS_Y
                                    || code == ABS_MT_POSITION_X
                                    || code == ABS_MT_POSITION_Y
                                {
                                    last_activity.store(now, Ordering::Relaxed);
                                    last_mouse_event = Instant::now();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            if jiggler_enabled
                && loop_now.duration_since(last_jiggler_check) >= JIGGLER_CHECK_INTERVAL
            {
                let now_ms = start.elapsed().as_millis() as u64;
                let kb_ms = last_keyboard_ms.load(Ordering::Relaxed);
                let kb_age_ms = now_ms.saturating_sub(kb_ms);
                let window_ms = jiggler_config.window_secs.saturating_mul(1000);

                let mouse_artificial = mouse_tracker.is_artificial() && kb_age_ms >= window_ms;
                let artificial = kb_tracker.is_artificial() || mouse_artificial;
                jiggler_pattern_flag.store(artificial, Ordering::Relaxed);
                last_jiggler_check = loop_now;
            }

            thread::sleep(Duration::from_millis(10));
        }
    });

    stats
}
