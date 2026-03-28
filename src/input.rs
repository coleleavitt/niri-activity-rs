use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::ops::RangeInclusive;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use inotify::{Inotify, WatchMask};
use input::event::pointer::{Axis, PointerScrollEvent};
use input::event::{Event, PointerEvent};
use input::{Libinput, LibinputInterface};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};

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

pub const BTN_TOUCH: u16 = 330;
pub const BTN_TOOL_FINGER: u16 = 325;
pub const BTN_LEFT: u16 = 272;
pub const BTN_RIGHT: u16 = 273;
pub const BTN_MIDDLE: u16 = 274;

pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_DELETE: u16 = 111;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

struct LibinputInterfaceImpl;

impl LibinputInterface for LibinputInterfaceImpl {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_ACCMODE == O_RDONLY) || (flags & O_ACCMODE == O_RDWR))
            .write((flags & O_ACCMODE == O_WRONLY) || (flags & O_ACCMODE == O_RDWR))
            .open(path)
            .map(Into::into)
            .map_err(|err| err.raw_os_error().unwrap_or(-1))
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InputSnapshot {
    pub keystrokes: u64,
    pub mouse_clicks: u64,
    pub scroll_events: u64,
    pub mouse_distance: u64,
    pub backspace_count: u64,
    pub modifier_count: u64,
    pub left_clicks: u64,
    pub right_clicks: u64,
    pub middle_clicks: u64,
    pub scroll_up: u64,
    pub scroll_down: u64,
    pub scroll_horizontal: u64,
}

pub struct InputStats {
    last_activity_ms: Arc<AtomicU64>,
    keystrokes: Arc<AtomicU64>,
    mouse_clicks: Arc<AtomicU64>,
    scroll_events: Arc<AtomicU64>,
    mouse_distance: Arc<AtomicU64>,
    backspace_count: Arc<AtomicU64>,
    modifier_count: Arc<AtomicU64>,
    left_clicks: Arc<AtomicU64>,
    right_clicks: Arc<AtomicU64>,
    middle_clicks: Arc<AtomicU64>,
    scroll_up: Arc<AtomicU64>,
    scroll_down: Arc<AtomicU64>,
    scroll_horizontal: Arc<AtomicU64>,
    jiggler_pattern: Arc<AtomicBool>,
    jiggler_process: Arc<AtomicBool>,
    last_keyboard_ms: Arc<AtomicU64>,
    last_meaningful_input_ms: Arc<AtomicU64>,
    heartbeat: Arc<AtomicU64>,
}

impl InputStats {
    pub fn snapshot(&self) -> InputSnapshot {
        InputSnapshot {
            keystrokes: self.keystrokes.swap(0, Ordering::AcqRel),
            mouse_clicks: self.mouse_clicks.swap(0, Ordering::AcqRel),
            scroll_events: self.scroll_events.swap(0, Ordering::AcqRel),
            mouse_distance: self.mouse_distance.swap(0, Ordering::AcqRel),
            backspace_count: self.backspace_count.swap(0, Ordering::AcqRel),
            modifier_count: self.modifier_count.swap(0, Ordering::AcqRel),
            left_clicks: self.left_clicks.swap(0, Ordering::AcqRel),
            right_clicks: self.right_clicks.swap(0, Ordering::AcqRel),
            middle_clicks: self.middle_clicks.swap(0, Ordering::AcqRel),
            scroll_up: self.scroll_up.swap(0, Ordering::AcqRel),
            scroll_down: self.scroll_down.swap(0, Ordering::AcqRel),
            scroll_horizontal: self.scroll_horizontal.swap(0, Ordering::AcqRel),
        }
    }

    pub fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms.load(Ordering::Acquire)
    }

    pub fn jiggler_detected(&self) -> bool {
        self.jiggler_pattern.load(Ordering::Acquire) || self.jiggler_process.load(Ordering::Acquire)
    }

    pub fn heartbeat(&self) -> u64 {
        self.heartbeat.load(Ordering::Acquire)
    }
}

struct IntervalTracker {
    timestamps_ms: VecDeque<u64>,
    window_ms: u64,
    min_events: usize,
    variance_threshold_ms: u64,
    /// Hard cap on deque size to prevent unbounded growth
    max_capacity: usize,
}

impl IntervalTracker {
    fn new(config: &JigglerConfig) -> Self {
        // Hard cap: even at 1 event/ms for a 5-min window, 300k entries is extreme.
        // 10_000 is generous for jiggler detection (typically ~1/sec over 2 min = 120).
        const MAX_TRACKER_CAPACITY: usize = 10_000;
        Self {
            timestamps_ms: VecDeque::with_capacity(256),
            window_ms: config.window_secs.saturating_mul(1000),
            min_events: config.min_events,
            variance_threshold_ms: config.variance_threshold_ms,
            max_capacity: MAX_TRACKER_CAPACITY,
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
        // Hard cap: drop oldest entries if deque exceeds max_capacity
        while self.timestamps_ms.len() > self.max_capacity {
            self.timestamps_ms.pop_front();
        }
    }

    fn is_artificial(&self) -> bool {
        if self.timestamps_ms.len() < self.min_events {
            return false;
        }

        let first = match self.timestamps_ms.front().copied() {
            Some(v) => v,
            None => return false,
        };
        let last = match self.timestamps_ms.back().copied() {
            Some(v) => v,
            None => return false,
        };
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

fn libinput_scroll_poll(scroll_events: &AtomicU64, last_activity: &AtomicU64, start: Instant) {
    let mut libinput = Libinput::new_with_udev(LibinputInterfaceImpl);

    if let Err(e) = libinput.udev_assign_seat("seat0") {
        eprintln!(
            "Warning: Failed to assign libinput seat: {:?}. Trackpad scroll disabled.",
            e
        );
        return;
    }

    eprintln!("Monitoring libinput for trackpad scroll events");

    let mut v_accum: f64 = 0.0;
    let mut h_accum: f64 = 0.0;
    const SCROLL_THRESHOLD: f64 = 15.0;

    fn extract_scroll_deltas(scroll: &impl PointerScrollEvent) -> (f64, f64) {
        let v = if scroll.has_axis(Axis::Vertical) {
            scroll.scroll_value(Axis::Vertical).abs()
        } else {
            0.0
        };
        let h = if scroll.has_axis(Axis::Horizontal) {
            scroll.scroll_value(Axis::Horizontal).abs()
        } else {
            0.0
        };
        (v, h)
    }

    loop {
        if let Err(e) = libinput.dispatch() {
            eprintln!("libinput dispatch error: {:?}", e);
            thread::sleep(Duration::from_secs(1));
            continue;
        }

        for event in &mut libinput {
            let (v_delta, h_delta) = match &event {
                Event::Pointer(PointerEvent::ScrollFinger(s)) => extract_scroll_deltas(s),
                Event::Pointer(PointerEvent::ScrollWheel(s)) => extract_scroll_deltas(s),
                Event::Pointer(PointerEvent::ScrollContinuous(s)) => extract_scroll_deltas(s),
                _ => continue,
            };

            v_accum += v_delta;
            h_accum += h_delta;

            let v_notches = (v_accum / SCROLL_THRESHOLD) as u64;
            let h_notches = (h_accum / SCROLL_THRESHOLD) as u64;

            if v_notches > 0 || h_notches > 0 {
                let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                scroll_events.fetch_add(v_notches + h_notches, Ordering::Release);
                v_accum = (v_notches as f64).mul_add(-SCROLL_THRESHOLD, v_accum);
                h_accum = (h_notches as f64).mul_add(-SCROLL_THRESHOLD, h_accum);
                last_activity.store(now, Ordering::Release);
            }
        }

        thread::sleep(Duration::from_millis(10));
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
                        if let Err(e) = dev.set_nonblocking(true) {
                            eprintln!(
                                "Warning: failed to set nonblocking on {}: {}, skipping device",
                                path.display(),
                                e
                            );
                            return None;
                        }
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

#[allow(clippy::too_many_arguments)]
fn input_poll_inner(
    start: Instant,
    mouse_idle_threshold: u64,
    jiggler_enabled: bool,
    jiggler_config: &JigglerConfig,
    devices_changed: &AtomicBool,
    last_activity: &AtomicU64,
    keystrokes: &AtomicU64,
    mouse_clicks: &AtomicU64,
    scroll_events: &AtomicU64,
    mouse_distance: &AtomicU64,
    backspace_count: &AtomicU64,
    modifier_count: &AtomicU64,
    left_clicks: &AtomicU64,
    right_clicks: &AtomicU64,
    middle_clicks: &AtomicU64,
    scroll_up: &AtomicU64,
    scroll_down: &AtomicU64,
    scroll_horizontal: &AtomicU64,
    jiggler_pattern_flag: &AtomicBool,
    last_keyboard_ms: &AtomicU64,
    last_meaningful: &AtomicU64,
    heartbeat: &AtomicU64,
) {
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

    let mut kb_tracker = IntervalTracker::new(jiggler_config);
    let mut mouse_tracker = IntervalTracker::new(jiggler_config);
    let mut last_mouse_tracker_ms: u64 = 0;
    let mut last_jiggler_check = Instant::now();

    let mut motion_dx: i64 = 0;
    let mut motion_dy: i64 = 0;
    let mut motion_window_start_ms: u64 = 0;
    const MOTION_WINDOW_MS: u64 = 2000;

    let mut last_mt_x: Option<i32> = None;
    let mut last_mt_y: Option<i32> = None;

    // Hi-res scroll accumulators (120 hi-res units = 1 notch).
    // Kernel drivers (hid-input.c, hid-logitech-hidpp.c) may emit only
    // REL_WHEEL_HI_RES without a corresponding REL_WHEEL when the
    // accumulated value hasn't reached one full notch (120 units).
    let mut hires_wheel_accum: i64 = 0;
    let mut hires_hwheel_accum: i64 = 0;
    let mut seen_hires_wheel = false;
    let mut seen_hires_hwheel = false;

    const REENUMERATE_INTERVAL: Duration = Duration::from_mins(1);
    const STALE_MOUSE_THRESHOLD: Duration = Duration::from_secs(30);
    const REENUMERATE_COOLDOWN: Duration = Duration::from_secs(10);
    const JIGGLER_CHECK_INTERVAL: Duration = Duration::from_secs(10);
    const HIRES_SCROLL_DIVISOR: i64 = 120;

    loop {
        let loop_now = Instant::now();
        heartbeat.fetch_add(1, Ordering::Release);

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
                    let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

                    match ev.event_type() {
                        evdev::EventType::KEY if ev.value() == 1 => {
                            let code = ev.code();
                            if BTN_MOUSE_RANGE.contains(&code) || code == BTN_TOUCH {
                                mouse_clicks.fetch_add(1, Ordering::Release);
                                match code {
                                    BTN_LEFT => left_clicks.fetch_add(1, Ordering::Release),
                                    BTN_RIGHT => right_clicks.fetch_add(1, Ordering::Release),
                                    BTN_MIDDLE => middle_clicks.fetch_add(1, Ordering::Release),
                                    _ => 0,
                                };
                                last_activity.store(now, Ordering::Release);
                                last_meaningful.store(now, Ordering::Release);
                                last_mouse_event = Instant::now();
                                if jiggler_enabled {
                                    mouse_tracker.record(now);
                                }
                            } else if code != BTN_TOOL_FINGER {
                                keystrokes.fetch_add(1, Ordering::Release);
                                if code == KEY_BACKSPACE || code == KEY_DELETE {
                                    backspace_count.fetch_add(1, Ordering::Release);
                                }
                                if code == KEY_LEFTCTRL
                                    || code == KEY_RIGHTCTRL
                                    || code == KEY_LEFTSHIFT
                                    || code == KEY_RIGHTSHIFT
                                    || code == KEY_LEFTALT
                                    || code == KEY_RIGHTALT
                                    || code == KEY_LEFTMETA
                                    || code == KEY_RIGHTMETA
                                {
                                    modifier_count.fetch_add(1, Ordering::Release);
                                }
                                last_activity.store(now, Ordering::Release);
                                last_meaningful.store(now, Ordering::Release);
                                last_keyboard_event = Instant::now();
                                last_keyboard_ms.store(now, Ordering::Release);
                                if jiggler_enabled {
                                    kb_tracker.record(now);
                                }
                            }
                        }
                        evdev::EventType::KEY if ev.value() == 0 && ev.code() == BTN_TOUCH => {
                            last_mt_x = None;
                            last_mt_y = None;
                        }
                        evdev::EventType::RELATIVE => {
                            let code = ev.code();
                            if code == REL_X || code == REL_Y {
                                let delta = ev.value();
                                mouse_distance
                                    .fetch_add(delta.unsigned_abs() as u64, Ordering::Release);

                                if code == REL_X {
                                    motion_dx = motion_dx.saturating_add(delta as i64);
                                } else {
                                    motion_dy = motion_dy.saturating_add(delta as i64);
                                }

                                let window_expired =
                                    now.saturating_sub(motion_window_start_ms) > MOTION_WINDOW_MS;

                                let net_sq = (motion_dx.saturating_mul(motion_dx))
                                    .saturating_add(motion_dy.saturating_mul(motion_dy));
                                let threshold_i64 =
                                    i64::try_from(mouse_idle_threshold).unwrap_or(i64::MAX);
                                let threshold_sq = threshold_i64.saturating_mul(threshold_i64);
                                let above_threshold = net_sq >= threshold_sq;

                                if window_expired || above_threshold {
                                    if above_threshold {
                                        last_activity.store(now, Ordering::Release);
                                    }
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
                            } else if code == REL_WHEEL_HI_RES {
                                seen_hires_wheel = true;
                                let raw_value = ev.value();
                                hires_wheel_accum += raw_value.unsigned_abs() as i64;
                                let notches = hires_wheel_accum / HIRES_SCROLL_DIVISOR;
                                if notches > 0 {
                                    scroll_events.fetch_add(notches as u64, Ordering::Release);
                                    if raw_value > 0 {
                                        scroll_up.fetch_add(notches as u64, Ordering::Release);
                                    } else {
                                        scroll_down.fetch_add(notches as u64, Ordering::Release);
                                    }
                                    hires_wheel_accum -= notches * HIRES_SCROLL_DIVISOR;
                                }
                                last_activity.store(now, Ordering::Release);
                                last_mouse_event = Instant::now();
                            } else if code == REL_HWHEEL_HI_RES {
                                seen_hires_hwheel = true;
                                hires_hwheel_accum += ev.value().unsigned_abs() as i64;
                                let notches = hires_hwheel_accum / HIRES_SCROLL_DIVISOR;
                                if notches > 0 {
                                    scroll_events.fetch_add(notches as u64, Ordering::Release);
                                    scroll_horizontal.fetch_add(notches as u64, Ordering::Release);
                                    hires_hwheel_accum -= notches * HIRES_SCROLL_DIVISOR;
                                }
                                last_activity.store(now, Ordering::Release);
                                last_mouse_event = Instant::now();
                            } else if (code == REL_WHEEL && !seen_hires_wheel)
                                || (code == REL_HWHEEL && !seen_hires_hwheel)
                            {
                                scroll_events.fetch_add(1, Ordering::Release);
                                if code == REL_WHEEL {
                                    if ev.value() > 0 {
                                        scroll_up.fetch_add(1, Ordering::Release);
                                    } else {
                                        scroll_down.fetch_add(1, Ordering::Release);
                                    }
                                } else {
                                    scroll_horizontal.fetch_add(1, Ordering::Release);
                                }
                                last_activity.store(now, Ordering::Release);
                                last_mouse_event = Instant::now();
                            } else if code == REL_WHEEL || code == REL_HWHEEL {
                                last_activity.store(now, Ordering::Release);
                                last_mouse_event = Instant::now();
                            }
                        }
                        evdev::EventType::ABSOLUTE => {
                            let code = ev.code();
                            let val = ev.value();
                            if code == ABS_MT_POSITION_X {
                                if let Some(prev) = last_mt_x {
                                    let delta = u64::from((val - prev).unsigned_abs());
                                    mouse_distance.fetch_add(delta, Ordering::Release);
                                }
                                last_mt_x = Some(val);
                                last_activity.store(now, Ordering::Release);
                                last_mouse_event = Instant::now();
                            } else if code == ABS_MT_POSITION_Y {
                                if let Some(prev) = last_mt_y {
                                    let delta = u64::from((val - prev).unsigned_abs());
                                    mouse_distance.fetch_add(delta, Ordering::Release);
                                }
                                last_mt_y = Some(val);
                                last_activity.store(now, Ordering::Release);
                                last_mouse_event = Instant::now();
                            } else if code == ABS_X || code == ABS_Y {
                                last_activity.store(now, Ordering::Release);
                                last_mouse_event = Instant::now();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if jiggler_enabled && loop_now.duration_since(last_jiggler_check) >= JIGGLER_CHECK_INTERVAL
        {
            let now_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            let kb_ms = last_keyboard_ms.load(Ordering::Acquire);
            let kb_age_ms = now_ms.saturating_sub(kb_ms);
            let window_ms = jiggler_config.window_secs.saturating_mul(1000);

            let mouse_artificial = mouse_tracker.is_artificial() && kb_age_ms >= window_ms;
            let artificial = kb_tracker.is_artificial() || mouse_artificial;
            jiggler_pattern_flag.store(artificial, Ordering::Release);
            last_jiggler_check = loop_now;
        }

        thread::sleep(Duration::from_millis(10));
    }
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
        backspace_count: Arc::new(AtomicU64::new(0)),
        modifier_count: Arc::new(AtomicU64::new(0)),
        left_clicks: Arc::new(AtomicU64::new(0)),
        right_clicks: Arc::new(AtomicU64::new(0)),
        middle_clicks: Arc::new(AtomicU64::new(0)),
        scroll_up: Arc::new(AtomicU64::new(0)),
        scroll_down: Arc::new(AtomicU64::new(0)),
        scroll_horizontal: Arc::new(AtomicU64::new(0)),
        jiggler_pattern: Arc::new(AtomicBool::new(false)),
        jiggler_process: Arc::new(AtomicBool::new(false)),
        last_keyboard_ms: Arc::new(AtomicU64::new(0)),
        last_meaningful_input_ms: Arc::new(AtomicU64::new(0)),
        heartbeat: Arc::new(AtomicU64::new(0)),
    };

    let last_activity = Arc::clone(&stats.last_activity_ms);
    let keystrokes = Arc::clone(&stats.keystrokes);
    let mouse_clicks = Arc::clone(&stats.mouse_clicks);
    let scroll_events = Arc::clone(&stats.scroll_events);
    let mouse_distance = Arc::clone(&stats.mouse_distance);
    let backspace_count = Arc::clone(&stats.backspace_count);
    let modifier_count = Arc::clone(&stats.modifier_count);
    let left_clicks = Arc::clone(&stats.left_clicks);
    let right_clicks = Arc::clone(&stats.right_clicks);
    let middle_clicks = Arc::clone(&stats.middle_clicks);
    let scroll_up = Arc::clone(&stats.scroll_up);
    let scroll_down = Arc::clone(&stats.scroll_down);
    let scroll_horizontal = Arc::clone(&stats.scroll_horizontal);
    let jiggler_pattern_flag = Arc::clone(&stats.jiggler_pattern);
    let last_keyboard_ms = Arc::clone(&stats.last_keyboard_ms);
    let last_meaningful = Arc::clone(&stats.last_meaningful_input_ms);
    let heartbeat = Arc::clone(&stats.heartbeat);

    let devices_changed = Arc::new(AtomicBool::new(false));
    let devices_changed_clone = Arc::clone(&devices_changed);

    // Detached thread: runs for process lifetime monitoring /dev/input hotplug.
    // JoinHandle intentionally dropped — thread terminates with process exit.
    if let Err(e) = thread::Builder::new()
        .name("input-hotplug".into())
        .spawn(move || {
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
            // JPL-R11: bounded by process lifetime — inotify::read_events_blocking
            // blocks on kernel I/O and terminates when the process exits.
            let mut inotify_iterations: u64 = 0;
            const MAX_INOTIFY_ITERATIONS: u64 = u64::MAX;
            loop {
                inotify_iterations = inotify_iterations.saturating_add(1);
                if inotify_iterations == MAX_INOTIFY_ITERATIONS {
                    eprintln!("inotify loop reached iteration limit, exiting");
                    break;
                }
                match inotify.read_events_blocking(&mut buffer) {
                    Ok(events) => {
                        for event in events {
                            if let Some(name) = event.name {
                                let name_str = name.to_string_lossy();
                                if name_str.starts_with("event") {
                                    eprintln!("Input device changed: {}", name.display());
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
        })
    {
        eprintln!(
            "Warning: Failed to spawn input-hotplug thread: {}. Device hotplug disabled.",
            e
        );
    }

    if jiggler_config.enabled {
        let jiggler_process_flag = Arc::clone(&stats.jiggler_process);
        let blacklist = jiggler_config.process_blacklist.clone();
        // Detached thread: runs for process lifetime scanning for jiggler processes.
        // JoinHandle intentionally dropped — thread terminates with process exit.
        if let Err(e) = thread::Builder::new()
            .name("jiggler-scan".into())
            .spawn(move || {
                // JPL-R11: bounded by process lifetime — sleeps 30s between iterations.
                let mut jiggler_iterations: u64 = 0;
                const MAX_JIGGLER_ITERATIONS: u64 = u64::MAX;
                loop {
                    jiggler_iterations = jiggler_iterations.saturating_add(1);
                    if jiggler_iterations == MAX_JIGGLER_ITERATIONS {
                        eprintln!("jiggler scan loop reached iteration limit, exiting");
                        break;
                    }
                    let found = scan_jiggler_processes(&blacklist);
                    jiggler_process_flag.store(found, Ordering::Release);
                    thread::sleep(Duration::from_secs(30));
                }
            })
        {
            eprintln!(
                "Warning: Failed to spawn jiggler-scan thread: {}. Jiggler detection disabled.",
                e
            );
        }
    }

    {
        let scroll_events_clone = Arc::clone(&stats.scroll_events);
        let last_activity_clone = Arc::clone(&stats.last_activity_ms);
        if let Err(e) = thread::Builder::new()
            .name("libinput-scroll".into())
            .spawn(move || {
                libinput_scroll_poll(&scroll_events_clone, &last_activity_clone, start);
            })
        {
            eprintln!(
                "Warning: Failed to spawn libinput-scroll thread: {}. Trackpad scroll disabled.",
                e
            );
        }
    }

    let jiggler_enabled = jiggler_config.enabled;

    if let Err(e) = thread::Builder::new()
        .name("input-poll".into())
        .spawn(move || {
            const MAX_RESPAWN_ATTEMPTS: u32 = 10;
            const MIN_RESPAWN_INTERVAL: Duration = Duration::from_secs(5);
            const RESPAWN_COUNT_RESET: Duration = Duration::from_mins(5);

            let mut respawn_count: u32 = 0;
            let mut last_respawn = Instant::now();

            loop {
                let devices_changed_ref = Arc::clone(&devices_changed);
                let last_activity_ref = Arc::clone(&last_activity);
                let keystrokes_ref = Arc::clone(&keystrokes);
                let mouse_clicks_ref = Arc::clone(&mouse_clicks);
                let scroll_events_ref = Arc::clone(&scroll_events);
                let mouse_distance_ref = Arc::clone(&mouse_distance);
                let backspace_count_ref = Arc::clone(&backspace_count);
                let modifier_count_ref = Arc::clone(&modifier_count);
                let left_clicks_ref = Arc::clone(&left_clicks);
                let right_clicks_ref = Arc::clone(&right_clicks);
                let middle_clicks_ref = Arc::clone(&middle_clicks);
                let scroll_up_ref = Arc::clone(&scroll_up);
                let scroll_down_ref = Arc::clone(&scroll_down);
                let scroll_horizontal_ref = Arc::clone(&scroll_horizontal);
                let jiggler_pattern_ref = Arc::clone(&jiggler_pattern_flag);
                let last_keyboard_ref = Arc::clone(&last_keyboard_ms);
                let last_meaningful_ref = Arc::clone(&last_meaningful);
                let heartbeat_ref = Arc::clone(&heartbeat);
                let jiggler_cfg = jiggler_config.clone();

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    input_poll_inner(
                        start,
                        mouse_idle_threshold,
                        jiggler_enabled,
                        &jiggler_cfg,
                        &devices_changed_ref,
                        &last_activity_ref,
                        &keystrokes_ref,
                        &mouse_clicks_ref,
                        &scroll_events_ref,
                        &mouse_distance_ref,
                        &backspace_count_ref,
                        &modifier_count_ref,
                        &left_clicks_ref,
                        &right_clicks_ref,
                        &middle_clicks_ref,
                        &scroll_up_ref,
                        &scroll_down_ref,
                        &scroll_horizontal_ref,
                        &jiggler_pattern_ref,
                        &last_keyboard_ref,
                        &last_meaningful_ref,
                        &heartbeat_ref,
                    );
                }));

                match result {
                    Ok(()) => break,
                    Err(panic_info) => {
                        let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                            (*s).to_string()
                        } else if let Some(s) = panic_info.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_string()
                        };
                        eprintln!(
                            "CRITICAL: input-poll thread panicked (attempt {}/{}): {}",
                            respawn_count + 1,
                            MAX_RESPAWN_ATTEMPTS,
                            msg,
                        );

                        if Instant::now().duration_since(last_respawn) >= RESPAWN_COUNT_RESET {
                            respawn_count = 0;
                        }
                        respawn_count += 1;
                        if respawn_count >= MAX_RESPAWN_ATTEMPTS {
                            eprintln!("input-poll thread exceeded max respawn attempts, giving up");
                            break;
                        }

                        let since_last = Instant::now().duration_since(last_respawn);
                        if let Some(remaining) = MIN_RESPAWN_INTERVAL.checked_sub(since_last) {
                            thread::sleep(remaining);
                        }
                        last_respawn = Instant::now();

                        eprintln!("Respawning input-poll thread...");
                    }
                }
            }
        })
    {
        eprintln!(
            "CRITICAL: Failed to spawn input-poll thread: {}. Idle detection will not work!",
            e
        );
    }

    stats
}
