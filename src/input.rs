use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::ops::RangeInclusive;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use input::event::device::DeviceEvent;
use input::event::keyboard::{KeyState, KeyboardEventTrait};
use input::event::pointer::{Axis, ButtonState, PointerScrollEvent};
use input::event::{Event, KeyboardEvent, PointerEvent};
use input::{Libinput, LibinputInterface};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use crate::config::JigglerConfig;

pub const BTN_MOUSE_RANGE: RangeInclusive<u16> = 272..=279;
pub const BTN_TOUCH: u16 = 330;
pub const BTN_TOOL_FINGER: u16 = 325;
pub const BTN_LEFT: u16 = 272;
pub const BTN_RIGHT: u16 = 273;
pub const BTN_MIDDLE: u16 = 274;

const SCROLL_NOTCH_THRESHOLD: f64 = 15.0;

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

/// All atomic counters for input tracking, grouped for efficient sharing.
pub struct InputCounters {
    pub last_activity_ms: AtomicU64,
    pub keystrokes: AtomicU64,
    pub mouse_clicks: AtomicU64,
    pub scroll_events: AtomicU64,
    pub mouse_distance: AtomicU64,
    pub backspace_count: AtomicU64,
    pub modifier_count: AtomicU64,
    pub left_clicks: AtomicU64,
    pub right_clicks: AtomicU64,
    pub middle_clicks: AtomicU64,
    pub scroll_up: AtomicU64,
    pub scroll_down: AtomicU64,
    pub scroll_horizontal: AtomicU64,
    pub jiggler_pattern: AtomicBool,
    pub jiggler_process: AtomicBool,
    pub last_keyboard_ms: AtomicU64,
    pub heartbeat: AtomicU64,
}

impl InputCounters {
    pub fn new() -> Self {
        Self {
            last_activity_ms: AtomicU64::new(0),
            keystrokes: AtomicU64::new(0),
            mouse_clicks: AtomicU64::new(0),
            scroll_events: AtomicU64::new(0),
            mouse_distance: AtomicU64::new(0),
            backspace_count: AtomicU64::new(0),
            modifier_count: AtomicU64::new(0),
            left_clicks: AtomicU64::new(0),
            right_clicks: AtomicU64::new(0),
            middle_clicks: AtomicU64::new(0),
            scroll_up: AtomicU64::new(0),
            scroll_down: AtomicU64::new(0),
            scroll_horizontal: AtomicU64::new(0),
            jiggler_pattern: AtomicBool::new(false),
            jiggler_process: AtomicBool::new(false),
            last_keyboard_ms: AtomicU64::new(0),
            heartbeat: AtomicU64::new(0),
        }
    }
}

impl Default for InputCounters {
    fn default() -> Self {
        Self::new()
    }
}

pub struct InputStats {
    counters: Arc<InputCounters>,
}

impl InputStats {
    #[allow(dead_code)]
    pub fn counters(&self) -> &Arc<InputCounters> {
        &self.counters
    }

    pub fn snapshot(&self) -> InputSnapshot {
        let snap = InputSnapshot {
            keystrokes: self.counters.keystrokes.swap(0, Ordering::AcqRel),
            mouse_clicks: self.counters.mouse_clicks.swap(0, Ordering::AcqRel),
            scroll_events: self.counters.scroll_events.swap(0, Ordering::AcqRel),
            mouse_distance: self.counters.mouse_distance.swap(0, Ordering::AcqRel),
            backspace_count: self.counters.backspace_count.swap(0, Ordering::AcqRel),
            modifier_count: self.counters.modifier_count.swap(0, Ordering::AcqRel),
            left_clicks: self.counters.left_clicks.swap(0, Ordering::AcqRel),
            right_clicks: self.counters.right_clicks.swap(0, Ordering::AcqRel),
            middle_clicks: self.counters.middle_clicks.swap(0, Ordering::AcqRel),
            scroll_up: self.counters.scroll_up.swap(0, Ordering::AcqRel),
            scroll_down: self.counters.scroll_down.swap(0, Ordering::AcqRel),
            scroll_horizontal: self.counters.scroll_horizontal.swap(0, Ordering::AcqRel),
        };
        // Debug: trace every snapshot call with caller location
        if snap.keystrokes > 0 || snap.mouse_clicks > 0 {
            tracing::trace!(
                target: "input_debug",
                keystrokes = snap.keystrokes,
                clicks = snap.mouse_clicks,
                mouse_px = snap.mouse_distance,
                "SNAPSHOT consumed"
            );
        }
        snap
    }

    pub fn last_activity_ms(&self) -> u64 {
        self.counters.last_activity_ms.load(Ordering::Acquire)
    }

    pub fn jiggler_detected(&self) -> bool {
        self.counters.jiggler_pattern.load(Ordering::Acquire)
            || self.counters.jiggler_process.load(Ordering::Acquire)
    }

    pub fn heartbeat(&self) -> u64 {
        self.counters.heartbeat.load(Ordering::Acquire)
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

/// Detect if any jiggler/artificial input software is currently running.
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

fn handle_finger_scroll(
    s: &input::event::pointer::PointerScrollFingerEvent,
    v_accum: &mut f64,
    h_accum: &mut f64,
    counters: &InputCounters,
    now: u64,
) {
    if s.has_axis(Axis::Vertical) {
        let v_raw = s.scroll_value(Axis::Vertical);
        if v_raw.is_finite() && v_raw != 0.0 {
            *v_accum += v_raw;
        }
    }
    if s.has_axis(Axis::Horizontal) {
        let h_raw = s.scroll_value(Axis::Horizontal);
        if h_raw.is_finite() && h_raw != 0.0 {
            *h_accum += h_raw;
        }
    }

    process_scroll_accumulators(v_accum, h_accum, counters, now);
}

fn handle_continuous_scroll(
    s: &input::event::pointer::PointerScrollContinuousEvent,
    v_accum: &mut f64,
    h_accum: &mut f64,
    counters: &InputCounters,
    now: u64,
) {
    if s.has_axis(Axis::Vertical) {
        let v_raw = s.scroll_value(Axis::Vertical);
        if v_raw.is_finite() && v_raw != 0.0 {
            *v_accum += v_raw;
        }
    }
    if s.has_axis(Axis::Horizontal) {
        let h_raw = s.scroll_value(Axis::Horizontal);
        if h_raw.is_finite() && h_raw != 0.0 {
            *h_accum += h_raw;
        }
    }

    process_scroll_accumulators(v_accum, h_accum, counters, now);
}

fn process_scroll_accumulators(
    v_accum: &mut f64,
    h_accum: &mut f64,
    counters: &InputCounters,
    now: u64,
) {
    let v_notches_signed = (*v_accum / SCROLL_NOTCH_THRESHOLD) as i64;
    let h_notches_signed = (*h_accum / SCROLL_NOTCH_THRESHOLD) as i64;
    let v_notches = v_notches_signed.unsigned_abs();
    let h_notches = h_notches_signed.unsigned_abs();

    if v_notches > 0 || h_notches > 0 {
        counters
            .scroll_events
            .fetch_add(v_notches + h_notches, Ordering::Release);

        if v_notches > 0 {
            if v_notches_signed > 0 {
                counters.scroll_down.fetch_add(v_notches, Ordering::Release);
            } else {
                counters.scroll_up.fetch_add(v_notches, Ordering::Release);
            }
            *v_accum = (v_notches_signed as f64).mul_add(-SCROLL_NOTCH_THRESHOLD, *v_accum);
        }
        if h_notches > 0 {
            counters
                .scroll_horizontal
                .fetch_add(h_notches, Ordering::Release);
            *h_accum = (h_notches_signed as f64).mul_add(-SCROLL_NOTCH_THRESHOLD, *h_accum);
        }

        counters.last_activity_ms.store(now, Ordering::Release);
    }
}

fn unified_input_loop(
    start: Instant,
    mouse_idle_threshold: u64,
    jiggler_enabled: bool,
    jiggler_config: &JigglerConfig,
    counters: &InputCounters,
) {
    let mut libinput = Libinput::new_with_udev(LibinputInterfaceImpl);

    if let Err(e) = libinput.udev_assign_seat("seat0") {
        tracing::warn!(
            "Failed to assign libinput seat: {:?}. Input monitoring disabled.",
            e
        );
        return;
    }

    tracing::info!("Unified libinput input monitoring started");

    let mut kb_tracker = IntervalTracker::new(jiggler_config);
    let mut mouse_tracker = IntervalTracker::new(jiggler_config);
    let mut last_mouse_tracker_ms: u64 = 0;
    let mut last_jiggler_check = Instant::now();

    let mut motion_dx: f64 = 0.0;
    let mut motion_dy: f64 = 0.0;
    let mut motion_window_start_ms: u64 = 0;
    const MOTION_WINDOW_MS: u64 = 2000;

    let mut v_accum: f64 = 0.0;
    let mut h_accum: f64 = 0.0;

    const JIGGLER_CHECK_INTERVAL: Duration = Duration::from_secs(10);

    let raw_fd = libinput.as_raw_fd();
    let timeout = Timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };

    loop {
        // SAFETY: raw_fd is valid for the lifetime of libinput
        #[allow(unsafe_code)]
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
        let mut pollfds = [PollFd::new(&borrowed_fd, PollFlags::IN)];

        match poll(&mut pollfds, Some(&timeout)) {
            Ok(_) => {
                if let Err(e) = libinput.dispatch() {
                    tracing::warn!("libinput dispatch error: {:?}", e);
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }

                for event in &mut libinput {
                    let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

                    match event {
                        Event::Keyboard(KeyboardEvent::Key(k)) => {
                            if k.key_state() != KeyState::Pressed {
                                continue;
                            }
                            let code = k.key() as u16;

                            if BTN_MOUSE_RANGE.contains(&code) || code == BTN_TOUCH {
                                counters.mouse_clicks.fetch_add(1, Ordering::Release);
                                match code {
                                    BTN_LEFT => {
                                        counters.left_clicks.fetch_add(1, Ordering::Release)
                                    }
                                    BTN_RIGHT => {
                                        counters.right_clicks.fetch_add(1, Ordering::Release)
                                    }
                                    BTN_MIDDLE => {
                                        counters.middle_clicks.fetch_add(1, Ordering::Release)
                                    }
                                    _ => 0,
                                };
                                counters.last_activity_ms.store(now, Ordering::Release);
                                if jiggler_enabled {
                                    mouse_tracker.record(now);
                                }
                            } else if code != BTN_TOOL_FINGER {
                                counters.keystrokes.fetch_add(1, Ordering::Release);
                                if code == KEY_BACKSPACE || code == KEY_DELETE {
                                    counters.backspace_count.fetch_add(1, Ordering::Release);
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
                                    counters.modifier_count.fetch_add(1, Ordering::Release);
                                }
                                counters.last_activity_ms.store(now, Ordering::Release);
                                counters.last_keyboard_ms.store(now, Ordering::Release);
                                if jiggler_enabled {
                                    kb_tracker.record(now);
                                }
                            }
                        }

                        Event::Pointer(PointerEvent::Motion(m)) => {
                            let dx = m.dx();
                            let dy = m.dy();

                            if dx.is_finite() && dy.is_finite() {
                                let dist = (dx.abs() + dy.abs()) as u64;
                                counters.mouse_distance.fetch_add(dist, Ordering::Release);

                                motion_dx += dx;
                                motion_dy += dy;

                                let window_expired =
                                    now.saturating_sub(motion_window_start_ms) > MOTION_WINDOW_MS;

                                let net_sq = motion_dy.mul_add(motion_dy, motion_dx * motion_dx);
                                let threshold_f64 = mouse_idle_threshold as f64;
                                let threshold_sq = threshold_f64 * threshold_f64;
                                let above_threshold = net_sq >= threshold_sq;

                                if window_expired || above_threshold {
                                    if above_threshold {
                                        counters.last_activity_ms.store(now, Ordering::Release);
                                    }
                                    motion_dx = 0.0;
                                    motion_dy = 0.0;
                                    motion_window_start_ms = now;
                                }

                                if jiggler_enabled
                                    && now.saturating_sub(last_mouse_tracker_ms) >= 1000
                                {
                                    mouse_tracker.record(now);
                                    last_mouse_tracker_ms = now;
                                }
                            }
                        }

                        Event::Pointer(PointerEvent::Button(b)) => {
                            if b.button_state() != ButtonState::Pressed {
                                continue;
                            }
                            let btn = b.button();
                            counters.mouse_clicks.fetch_add(1, Ordering::Release);
                            match btn {
                                272 => counters.left_clicks.fetch_add(1, Ordering::Release),
                                273 => counters.right_clicks.fetch_add(1, Ordering::Release),
                                274 => counters.middle_clicks.fetch_add(1, Ordering::Release),
                                _ => 0,
                            };
                            counters.last_activity_ms.store(now, Ordering::Release);
                        }

                        Event::Pointer(PointerEvent::ScrollWheel(s)) => {
                            if s.has_axis(Axis::Vertical) {
                                let v_v120 = s.scroll_value_v120(Axis::Vertical);
                                if v_v120.is_finite() && v_v120 != 0.0 {
                                    let v_notches = (v_v120.abs() / 120.0) as u64;
                                    if v_notches > 0 {
                                        counters
                                            .scroll_events
                                            .fetch_add(v_notches, Ordering::Release);
                                        if v_v120 > 0.0 {
                                            counters
                                                .scroll_down
                                                .fetch_add(v_notches, Ordering::Release);
                                        } else {
                                            counters
                                                .scroll_up
                                                .fetch_add(v_notches, Ordering::Release);
                                        }
                                        counters.last_activity_ms.store(now, Ordering::Release);
                                    }
                                }
                            }
                            if s.has_axis(Axis::Horizontal) {
                                let h_v120 = s.scroll_value_v120(Axis::Horizontal);
                                if h_v120.is_finite() && h_v120 != 0.0 {
                                    let h_notches = (h_v120.abs() / 120.0) as u64;
                                    if h_notches > 0 {
                                        counters
                                            .scroll_events
                                            .fetch_add(h_notches, Ordering::Release);
                                        counters
                                            .scroll_horizontal
                                            .fetch_add(h_notches, Ordering::Release);
                                        counters.last_activity_ms.store(now, Ordering::Release);
                                    }
                                }
                            }
                        }

                        Event::Pointer(PointerEvent::ScrollFinger(s)) => {
                            handle_finger_scroll(&s, &mut v_accum, &mut h_accum, counters, now);
                        }

                        Event::Pointer(PointerEvent::ScrollContinuous(s)) => {
                            handle_continuous_scroll(&s, &mut v_accum, &mut h_accum, counters, now);
                        }

                        Event::Device(DeviceEvent::Added(_)) => {
                            tracing::debug!("libinput: device added");
                        }

                        Event::Device(DeviceEvent::Removed(_)) => {
                            tracing::debug!("libinput: device removed");
                        }

                        _ => {}
                    }
                }
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                tracing::warn!("poll error: {}", e);
                thread::sleep(Duration::from_secs(1));
            }
        }

        counters.heartbeat.fetch_add(1, Ordering::Release);

        if jiggler_enabled
            && Instant::now().duration_since(last_jiggler_check) >= JIGGLER_CHECK_INTERVAL
        {
            let now_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            let kb_ms = counters.last_keyboard_ms.load(Ordering::Acquire);
            let kb_age_ms = now_ms.saturating_sub(kb_ms);
            let window_ms = jiggler_config.window_secs.saturating_mul(1000);

            let mouse_artificial = mouse_tracker.is_artificial() && kb_age_ms >= window_ms;
            let artificial = kb_tracker.is_artificial() || mouse_artificial;
            counters
                .jiggler_pattern
                .store(artificial, Ordering::Release);
            last_jiggler_check = Instant::now();
        }
    }
}

pub fn start_idle_monitor(
    start: Instant,
    jiggler_config: JigglerConfig,
    mouse_idle_threshold: u64,
) -> InputStats {
    let counters = Arc::new(InputCounters::new());
    let stats = InputStats {
        counters: Arc::clone(&counters),
    };

    if jiggler_config.enabled {
        let counters_jiggler = Arc::clone(&counters);
        let blacklist = jiggler_config.process_blacklist.clone();
        if let Err(e) = thread::Builder::new()
            .name("jiggler-scan".into())
            .spawn(move || {
                let mut jiggler_iterations: u64 = 0;
                const MAX_JIGGLER_ITERATIONS: u64 = u64::MAX;
                loop {
                    jiggler_iterations = jiggler_iterations.saturating_add(1);
                    if jiggler_iterations == MAX_JIGGLER_ITERATIONS {
                        tracing::warn!("jiggler scan loop reached iteration limit, exiting");
                        break;
                    }
                    let found = scan_jiggler_processes(&blacklist);
                    counters_jiggler
                        .jiggler_process
                        .store(found, Ordering::Release);
                    thread::sleep(Duration::from_secs(30));
                }
            })
        {
            tracing::warn!(
                "Failed to spawn jiggler-scan thread: {}. Jiggler detection disabled.",
                e
            );
        }
    }

    let jiggler_enabled = jiggler_config.enabled;
    let counters_poll = Arc::clone(&counters);

    if let Err(e) = thread::Builder::new()
        .name("input-poll".into())
        .spawn(move || {
            const MAX_RESPAWN_ATTEMPTS: u32 = 10;
            const MIN_RESPAWN_INTERVAL: Duration = Duration::from_secs(5);
            const RESPAWN_COUNT_RESET: Duration = Duration::from_mins(5);

            let mut respawn_count: u32 = 0;
            let mut last_respawn = Instant::now();

            loop {
                let jiggler_cfg = jiggler_config.clone();

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    unified_input_loop(
                        start,
                        mouse_idle_threshold,
                        jiggler_enabled,
                        &jiggler_cfg,
                        &counters_poll,
                    );
                }));

                match result {
                    Ok(()) => {
                        tracing::warn!(
                            "unified_input_loop returned normally (attempt {}/{}), retrying...",
                            respawn_count + 1,
                            MAX_RESPAWN_ATTEMPTS,
                        );
                        if Instant::now().duration_since(last_respawn) >= RESPAWN_COUNT_RESET {
                            respawn_count = 0;
                        }
                        respawn_count += 1;
                        if respawn_count >= MAX_RESPAWN_ATTEMPTS {
                            tracing::error!(
                                "input-poll thread exceeded max respawn attempts, giving up"
                            );
                            break;
                        }
                        let since_last = Instant::now().duration_since(last_respawn);
                        if let Some(remaining) = MIN_RESPAWN_INTERVAL.checked_sub(since_last) {
                            thread::sleep(remaining);
                        }
                        last_respawn = Instant::now();
                    }
                    Err(panic_info) => {
                        let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                            (*s).to_string()
                        } else if let Some(s) = panic_info.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_string()
                        };
                        tracing::error!(
                            "input-poll thread panicked (attempt {}/{}): {}",
                            respawn_count + 1,
                            MAX_RESPAWN_ATTEMPTS,
                            msg,
                        );

                        if Instant::now().duration_since(last_respawn) >= RESPAWN_COUNT_RESET {
                            respawn_count = 0;
                        }
                        respawn_count += 1;
                        if respawn_count >= MAX_RESPAWN_ATTEMPTS {
                            tracing::error!(
                                "input-poll thread exceeded max respawn attempts, giving up"
                            );
                            break;
                        }

                        let since_last = Instant::now().duration_since(last_respawn);
                        if let Some(remaining) = MIN_RESPAWN_INTERVAL.checked_sub(since_last) {
                            thread::sleep(remaining);
                        }
                        last_respawn = Instant::now();

                        tracing::warn!("Respawning input-poll thread...");
                    }
                }
            }
        })
    {
        tracing::error!(
            "Failed to spawn input-poll thread: {}. Idle detection will not work!",
            e
        );
    }

    stats
}
