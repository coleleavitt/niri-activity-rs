# Learnings

## Architecture Decisions
- Consolidate evdev + libinput → single libinput thread (libinput wraps evdev internally)
- Use poll(2) via rustix on libinput.as_raw_fd() instead of thread::sleep(10ms)
- Keep atomic swap(0) pattern — correct for aggregate counting
- Use dx()/dy() (accelerated) for mouse distance — normalized cross-device
- scroll_value_v120(Axis) / 120.0 for wheel notch count (matches 120 hi-res convention)
- scroll_value(Axis) + SCROLL_NOTCH_THRESHOLD=15.0 for finger/continuous scroll

## libinput Key/Button Codes
- libinput key codes ARE identical to Linux kernel codes (same BTN_LEFT=272, KEY_BACKSPACE=14)
- libinput Keyboard events: .key() returns u32, .key_state() returns KeyState::Pressed/Released
- libinput Pointer Button: .button() returns u32, .button_state() returns ButtonState::Pressed/Released
- libinput wraps evdev internally — running both threads was redundant

## Scroll
- ScrollWheel: use scroll_value_v120(Axis) / 120.0 for notch count
- ScrollFinger/Continuous: use scroll_value(Axis) + accumulator + SCROLL_NOTCH_THRESHOLD
- scroll-stop events have value == 0.0 — must NOT count these
- direction: sign of scroll_value(Axis::Vertical) → scroll_up vs scroll_down

## Edge Cases Identified by Metis
- poll() EINTR: retry on EINTR
- f64→u64: .abs() then as u64 (truncation fine for distance accumulation)
- NaN/infinity from libinput: guard with .is_finite() before conversion
- Scroll value 0.0 = scroll-stop event: skip
- BTN_TOUCH touchscreen tap → Touch events in libinput, NOT Pointer::Button
- net-displacement threshold: adapt from i64 accumulator to f64 accumulator

## Cargo.toml
- ADD: rustix = { version = "1", features = ["event"] }
- REMOVE: evdev = "0.13"
- REMOVE: inotify (once hotplug thread removed in Task 6)
