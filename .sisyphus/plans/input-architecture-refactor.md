# Input Architecture Refactor: evdev+libinput → Unified libinput with poll(2)

## TL;DR

> **Quick Summary**: Consolidate the redundant dual-thread input monitoring (evdev + libinput) into a single libinput-based thread using `poll(2)` for event-driven wakeup. Fix scroll direction tracking bug. Group 20+ atomic counter params into a clean `InputCounters` struct.
>
> **Deliverables**:
> - Single unified input thread replacing 2 separate threads (evdev poll + libinput scroll)
> - `poll(2)` via `rustix` replacing busy-wait `thread::sleep(10ms)`
> - `InputCounters` struct replacing 20+ individual `&AtomicU64` function params
> - Fixed scroll direction tracking for touchpad/finger scroll (existing bug)
> - Removal of `evdev` and `inotify` dependencies
>
> **Estimated Effort**: Medium (1-2 days)
> **Parallel Execution**: NO — sequential (all changes in same file, each builds on previous)
> **Critical Path**: Task 1 → 2 → 3 → 4 → 5 → 6

---

## Context

### Original Request
Refactor the input monitoring system to use the best architectural approach. User asked to study libinput C code, evdev kernel driver, and Rust crate source to determine optimal design.

### Interview Summary
**Key Discussions**:
- Oracle consultation: Recommends single libinput thread with `poll(2)`, keep atomic swap pattern
- Metis review: Identified scroll direction bug, 6-task sequential plan, detailed edge cases

**Research Findings**:
- Kernel `evdev.c`: Uses ring buffer with SYN_DROPPED on overflow, events delivered via character device
- libinput (`libinput.c`): Uses `epoll_create1` internally, exposes pollable fd via `libinput_get_fd()`/`AsRawFd`
- `input` crate: Implements `AsRawFd` for `Libinput`, exposes all event types (Keyboard, Pointer, Touch, Device)
- libinput key/button codes are identical to Linux kernel codes — same `KEY_BACKSPACE=14`, `BTN_LEFT=272` etc.
- libinput wraps evdev internally — running both is redundant
- **BUG FOUND**: `libinput_scroll_poll()` only updates `scroll_events` — does NOT update `scroll_up`/`scroll_down`/`scroll_horizontal`

### Metis Review
**Identified Gaps** (addressed):
- Touchpad mouse distance semantics change (raw ABS → libinput pointer motion in mm)
- f64 → u64 conversion edge cases (NaN, infinity, negative)
- `poll()` EINTR handling needed
- Scroll value-of-zero (scroll-stop events) must not count
- BTN_TOUCH touchscreen tap → libinput sends as TouchEvent, not PointerEvent
- net-displacement jiggler heuristic needs i64→f64 adaptation

---

## Work Objectives

### Core Objective
Replace the redundant dual-thread evdev+libinput input monitoring with a single, architecturally sound libinput-based thread using `poll(2)` for event-driven wakeup.

### Concrete Deliverables
- `src/input.rs` — rewritten from dual-thread to unified architecture
- `Cargo.toml` — add `rustix`, remove `evdev` and `inotify`

### Definition of Done
- [ ] Single input thread handles keyboard, mouse, scroll, touchpad via libinput
- [ ] `poll(2)` used instead of `thread::sleep(10ms)` — zero CPU when idle
- [ ] `InputCounters` struct replaces 20+ individual atomic params
- [ ] `cargo clippy --all-targets -- -D warnings` passes clean
- [ ] `cargo build --release` succeeds
- [ ] `cargo test` — all tests pass
- [ ] Scroll direction tracked for ALL scroll sources (bug fix)

### Must Have
- `InputStats` and `InputSnapshot` public API unchanged (watcher.rs has 9 call sites)
- All 12 `InputSnapshot` fields populated from libinput events
- Jiggler detection (both pattern and process scan) preserved
- Panic-catch-and-respawn wrapper preserved
- Device hotplug handling (via libinput `DEVICE_ADDED`/`DEVICE_REMOVED`)
- Heartbeat increment each poll cycle

### Must NOT Have (Guardrails)
- G1: Do NOT change `InputSnapshot`, `InputStats` public API, `SessionSnapshot`, or DB schema
- G2: Do NOT refactor `watcher.rs`, `db.rs`, `config.rs` — only `input.rs` and `Cargo.toml`
- G3: Do NOT add gesture tracking, touchscreen features, new config options, or async runtime
- G4: Do NOT change the `snapshot()` swap(0) semantics — the pattern is correct after the earlier fix
- G5: Do NOT add new dependencies beyond `rustix` — remove `evdev` and `inotify`

---

## Verification Strategy

> **ZERO HUMAN INTERVENTION** — ALL verification is agent-executed. No exceptions.

### Test Decision
- **Infrastructure exists**: YES (cargo test, 38 tests)
- **Automated tests**: Tests-after (verify existing tests pass, add new unit tests for InputCounters)
- **Framework**: cargo test (built-in)

### QA Policy
Every task includes agent-executed QA via `cargo clippy`, `cargo build --release`, `cargo test`.
For the core rewrite (Task 4), runtime verification via launching daemon and checking DB captures.

---

## Execution Strategy

### Sequential Execution (same file — cannot parallelize)

```
Wave 1 (Start Immediately):
├── Task 1: Add rustix to Cargo.toml [quick]
└── Task 2: Create InputCounters struct [unspecified-low]

Wave 2 (After Wave 1):
└── Task 3: Thread InputCounters through all functions [unspecified-low]

Wave 3 (After Wave 2):
└── Task 4: Fix scroll direction bug [unspecified-low]

Wave 4 (After Wave 3):
└── Task 5: Implement unified libinput poll loop [deep]

Wave 5 (After Wave 4):
└── Task 6: Remove dead evdev code + inotify thread [unspecified-low]

Wave FINAL (After ALL tasks):
└── Task 7: Final verification [quick]

Critical Path: 1+2 → 3 → 4 → 5 → 6 → 7
```

### Dependency Matrix

| Task | Depends On | Blocks |
|------|-----------|--------|
| 1    | —         | 5      |
| 2    | —         | 3      |
| 3    | 2         | 4      |
| 4    | 3         | 5      |
| 5    | 1, 3, 4   | 6      |
| 6    | 5         | 7      |
| 7    | 6         | —      |

### Agent Dispatch Summary

- **Wave 1**: 2 tasks — T1 `quick`, T2 `unspecified-low`
- **Wave 2**: 1 task — T3 `unspecified-low`
- **Wave 3**: 1 task — T4 `unspecified-low`
- **Wave 4**: 1 task — T5 `deep` (core rewrite)
- **Wave 5**: 1 task — T6 `unspecified-low`
- **FINAL**: 1 task — T7 `quick`

---

## TODOs

- [x] 1. Add `rustix` dependency to Cargo.toml

  **What to do**:
  - Add `rustix = { version = "1", features = ["event"] }` to `[dependencies]` in `Cargo.toml`
  - Run `cargo check` to verify it resolves

  **Must NOT do**:
  - Add any other new dependencies
  - Change any Rust source files

  **Recommended Agent Profile**:
  - **Category**: `quick`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES (with Task 2)
  - **Blocks**: Task 5
  - **Blocked By**: None

  **References**:
  - `Cargo.toml` — existing dependency section to match formatting
  - `rustix` docs: `https://docs.rs/rustix/latest/rustix/event/fn.poll.html`

  **Acceptance Criteria**:
  - [ ] `cargo check` succeeds
  - [ ] `use rustix::event::poll;` compiles

  **QA Scenarios**:
  ```
  Scenario: rustix dependency resolves
    Tool: Bash
    Steps:
      1. Run `cargo check`
      2. Assert exit code 0
    Expected Result: Clean compilation
    Evidence: .sisyphus/evidence/task-1-rustix-check.txt
  ```

  **Commit**: YES (groups with Task 2)
  - Message: `refactor(input): extract InputCounters struct and add rustix dependency`
  - Files: `Cargo.toml`

- [x] 2. Create `InputCounters` struct

  **What to do**:
  - Create `pub struct InputCounters` containing all 13 `AtomicU64` fields and 2 `AtomicBool` fields currently spread across `InputStats`
  - Fields: `last_activity_ms`, `keystrokes`, `mouse_clicks`, `scroll_events`, `mouse_distance`, `backspace_count`, `modifier_count`, `left_clicks`, `right_clicks`, `middle_clicks`, `scroll_up`, `scroll_down`, `scroll_horizontal`, `jiggler_pattern`, `jiggler_process`, `last_keyboard_ms`, `last_meaningful_input_ms`, `heartbeat`
  - Make `InputStats` hold a single `Arc<InputCounters>` instead of 16 individual `Arc<AtomicU64/Bool>`
  - Add `InputCounters::new()` that initializes all fields to 0/false
  - Rewrite `InputStats::snapshot()`, `last_activity_ms()`, `jiggler_detected()`, `heartbeat()` to delegate to `self.counters.field_name`
  - Keep `InputStats` public API IDENTICAL — watcher.rs has 9 call sites

  **Must NOT do**:
  - Change `InputSnapshot` struct
  - Change any public method signatures on `InputStats`
  - Change `watcher.rs` or any other file

  **Recommended Agent Profile**:
  - **Category**: `unspecified-low`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES (with Task 1)
  - **Blocks**: Task 3
  - **Blocked By**: None

  **References**:
  - `src/input.rs:87-106` — current `InputStats` struct definition
  - `src/input.rs:108-137` — current `impl InputStats` (snapshot, last_activity_ms, jiggler_detected, heartbeat)
  - `src/input.rs:646-665` — current `InputStats` construction in `start_idle_monitor` with 16 individual `Arc::new`
  - `src/watcher.rs` — all `input_stats.snapshot()`, `.last_activity_ms()`, `.jiggler_detected()`, `.heartbeat()` call sites (verify unchanged)

  **Acceptance Criteria**:
  - [ ] `InputCounters` struct exists with all atomic fields
  - [ ] `InputStats` holds `Arc<InputCounters>`
  - [ ] `cargo clippy --all-targets -- -D warnings` passes
  - [ ] `cargo test` passes (all 38 tests)

  **QA Scenarios**:
  ```
  Scenario: InputStats public API unchanged
    Tool: Bash
    Steps:
      1. Run `cargo clippy --all-targets -- -D warnings`
      2. Run `cargo test`
      3. grep for `input_stats.snapshot()` in watcher.rs — should still compile
    Expected Result: Zero errors, zero warnings, all tests pass
    Evidence: .sisyphus/evidence/task-2-clippy.txt

  Scenario: No behavioral change
    Tool: Bash
    Steps:
      1. Run `cargo build --release`
      2. Assert exit 0
    Expected Result: Clean build
    Evidence: .sisyphus/evidence/task-2-build.txt
  ```

  **Commit**: YES (groups with Task 1)
  - Message: `refactor(input): extract InputCounters struct and add rustix dependency`
  - Files: `src/input.rs`, `Cargo.toml`

- [x] 3. Thread `InputCounters` through poll functions

  **What to do**:
  - Replace the 20+ individual `&AtomicU64` parameters in `input_poll_inner` with `&InputCounters`
  - Replace the 2 individual `&AtomicU64` params in `libinput_scroll_poll` with `&InputCounters`
  - In `start_idle_monitor`: replace 17 individual `Arc::clone(&stats.field)` calls with a single `Arc::clone(&stats.counters)` (or however the inner Arc is exposed)
  - In the respawn loop inside `start_idle_monitor`: eliminate the re-clone of all Arcs inside the loop body — the `Arc<InputCounters>` is already cloned outside
  - Keep separate params: `start: Instant`, `mouse_idle_threshold: u64`, `jiggler_enabled: bool`, `jiggler_config: &JigglerConfig`, `devices_changed: Arc<AtomicBool>`
  - Access counters via `counters.keystrokes.fetch_add(1, Ordering::Release)` etc.

  **Must NOT do**:
  - Change behavior — this is pure signature refactoring
  - Change `InputStats` or `InputSnapshot` public API
  - Touch `watcher.rs`

  **Recommended Agent Profile**:
  - **Category**: `unspecified-low`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: NO
  - **Blocks**: Task 4
  - **Blocked By**: Task 2

  **References**:
  - `src/input.rs:347-370` — current `input_poll_inner` signature (20+ params)
  - `src/input.rs:251-313` — current `libinput_scroll_poll` signature
  - `src/input.rs:667-702` — current Arc clone dance in `start_idle_monitor`
  - `src/input.rs:805-842` — current re-clone in respawn loop

  **Acceptance Criteria**:
  - [ ] `input_poll_inner` has ≤8 parameters
  - [ ] `libinput_scroll_poll` has ≤4 parameters
  - [ ] `cargo clippy --all-targets -- -D warnings` passes
  - [ ] `cargo test` passes

  **QA Scenarios**:
  ```
  Scenario: Reduced parameter count
    Tool: Bash (grep)
    Steps:
      1. Count parameters of input_poll_inner via grep
      2. Assert ≤8
    Expected Result: Function signature is clean
    Evidence: .sisyphus/evidence/task-3-params.txt

  Scenario: No behavioral change
    Tool: Bash
    Steps:
      1. `cargo clippy --all-targets -- -D warnings`
      2. `cargo test`
    Expected Result: Zero warnings, all tests pass
    Evidence: .sisyphus/evidence/task-3-clippy.txt
  ```

  **Commit**: YES
  - Message: `refactor(input): thread InputCounters through poll functions`
  - Files: `src/input.rs`

- [x] 4. Fix scroll direction bug in libinput scroll thread

  **What to do**:
  - In `libinput_scroll_poll`, after calculating `v_notches` and `h_notches`, update direction counters:
    - If vertical scroll value > 0: `counters.scroll_up.fetch_add(v_notches, ...)`
    - If vertical scroll value < 0: `counters.scroll_down.fetch_add(v_notches, ...)`
    - If horizontal notches > 0: `counters.scroll_horizontal.fetch_add(h_notches, ...)`
  - This fixes the existing bug where touchpad/finger scroll only updates `scroll_events` but loses direction
  - Track the sign of `scroll_value(Axis::Vertical)` before taking `.abs()` for accumulation

  **Must NOT do**:
  - Change how `scroll_events` total is calculated
  - Change any threshold values

  **Recommended Agent Profile**:
  - **Category**: `unspecified-low`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: NO
  - **Blocks**: Task 5
  - **Blocked By**: Task 3

  **References**:
  - `src/input.rs:251-313` — current `libinput_scroll_poll` (only updates `scroll_events`, NOT direction)
  - `src/input.rs:558-605` — evdev scroll handling that DOES track direction (reference for how it should work)

  **Acceptance Criteria**:
  - [ ] `scroll_up`, `scroll_down`, `scroll_horizontal` updated in `libinput_scroll_poll`
  - [ ] `cargo clippy --all-targets -- -D warnings` passes
  - [ ] `cargo test` passes

  **QA Scenarios**:
  ```
  Scenario: Direction counters updated
    Tool: Bash (grep)
    Steps:
      1. grep for `scroll_up.fetch_add` in libinput_scroll_poll function
      2. grep for `scroll_down.fetch_add` in libinput_scroll_poll function
      3. All should be present
    Expected Result: Direction counters used in libinput scroll path
    Evidence: .sisyphus/evidence/task-4-scroll-fix.txt
  ```

  **Commit**: YES
  - Message: `fix(input): track scroll direction for libinput scroll sources`
  - Files: `src/input.rs`

- [x] 5. Implement unified libinput poll loop with poll(2)

  **What to do**:
  - Create `fn unified_input_loop(start: Instant, mouse_idle_threshold: u64, jiggler_enabled: bool, jiggler_config: &JigglerConfig, counters: &InputCounters)` that:
    - Creates `Libinput::new_with_udev(LibinputInterfaceImpl)`, assigns seat `"seat0"`
    - If seat assignment fails, log warning and return (same as current scroll thread behavior)
    - Uses `rustix::event::poll()` with `PollFd` on `libinput.as_raw_fd()`, timeout 1000ms
    - On poll ready: calls `libinput.dispatch()`, iterates events via `for event in &mut libinput`
    - Event handling:
      - `Event::Keyboard(KeyboardEvent::Key(k))` where `k.key_state() == KeyState::Pressed`:
        - `k.key()` returns u32 Linux key code (same as evdev codes)
        - Check if button range (272..=279) or BTN_TOUCH(330) → mouse click counting
        - Else if not BTN_TOOL_FINGER(325) → keystroke counting + backspace/modifier detection
        - Update `last_activity_ms`, `last_keyboard_ms`, `last_meaningful_input_ms` as appropriate
        - Feed `IntervalTracker` for jiggler detection
      - `Event::Pointer(PointerEvent::Motion(m))`:
        - Distance: `(m.dx().abs() + m.dy().abs()) as u64` → `counters.mouse_distance.fetch_add(...)`
        - Net-displacement threshold logic (adapt from i64 to f64): accumulate dx/dy in window, check `sqrt(dx²+dy²)` against threshold
        - Update `last_activity_ms` when above threshold
        - Feed mouse `IntervalTracker` (throttled to 1/sec)
      - `Event::Pointer(PointerEvent::Button(b))` where `b.button_state() == ButtonState::Pressed`:
        - `b.button()` returns u32 (same BTN_LEFT=272, BTN_RIGHT=273, BTN_MIDDLE=274)
        - Increment `mouse_clicks` + specific `left_clicks`/`right_clicks`/`middle_clicks`
        - Update `last_activity_ms`, `last_meaningful_input_ms`
      - `Event::Pointer(PointerEvent::ScrollWheel(s))`:
        - Use `s.scroll_value_v120(Axis::Vertical)` / 120.0 for notch count (matches current hi-res convention)
        - Use `s.scroll_value_v120(Axis::Horizontal)` / 120.0 for horizontal
        - Skip if value is 0 (scroll-stop event)
        - Update `scroll_events`, `scroll_up`/`scroll_down`/`scroll_horizontal` based on sign
      - `Event::Pointer(PointerEvent::ScrollFinger(s))` and `ScrollContinuous(s)`:
        - Use `s.scroll_value(Axis::Vertical/Horizontal)` with accumulator + `SCROLL_NOTCH_THRESHOLD` (15.0)
        - Same direction tracking as wheel
      - `Event::Device(DeviceEvent::Added(d))`: `tracing::debug!("Device added: ...")`
      - `Event::Device(DeviceEvent::Removed(d))`: `tracing::debug!("Device removed: ...")`
      - All other events: ignore
    - Heartbeat: `counters.heartbeat.fetch_add(1, ...)` each poll cycle
    - Jiggler: check `IntervalTracker::is_artificial()` every 10 seconds
    - Handle `poll()` EINTR: retry
    - Handle `dispatch()` errors: log warning, sleep 1s, continue
    - Guard f64→u64: `.abs()` then `as u64` (truncation is fine for distance accumulation)
  - Update `start_idle_monitor()`:
    - Remove the `libinput-scroll` thread spawn entirely
    - Replace the `input-poll` thread body with `unified_input_loop(...)` (still wrapped in panic-catch-respawn)
    - Keep the `jiggler-scan` thread as-is
    - Keep the `input-hotplug` (inotify) thread for now (removed in Task 6)

  **Must NOT do**:
  - Change `InputStats` or `InputSnapshot` public API
  - Add async/tokio
  - Change watcher.rs

  **Recommended Agent Profile**:
  - **Category**: `deep`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: NO
  - **Blocks**: Task 6
  - **Blocked By**: Tasks 1, 3, 4

  **References**:
  - `src/input.rs:53-69` — existing `LibinputInterfaceImpl` (reuse as-is)
  - `src/input.rs:251-313` — current `libinput_scroll_poll` (event handling patterns to follow)
  - `src/input.rs:347-675` — current `input_poll_inner` (ALL behavior to preserve)
  - `src/input.rs:677-899` — current `start_idle_monitor` (thread spawn patterns)
  - `/tmp/input-rs/src/lib.rs:70-78` — libinput event loop example: `input.dispatch()` then `for event in &mut input`
  - `/tmp/evdev-rs/examples/evtest_nonblocking.rs:33-48` — poll + fetch_events pattern
  - `rustix::event::poll` docs — `PollFd::new()`, `PollFlags::IN`, `poll(&mut [pollfd], timeout)`
  - `input::event::keyboard::KeyboardEvent::Key` — `.key()` returns u32, `.key_state()` returns KeyState
  - `input::event::pointer::PointerEvent::Motion` — `.dx()`, `.dy()`, `.dx_unaccelerated()`, `.dy_unaccelerated()`
  - `input::event::pointer::PointerEvent::Button` — `.button()` returns u32, `.button_state()`
  - `input::event::pointer::PointerScrollEvent` — `.scroll_value(Axis)`, `.scroll_value_v120(Axis)`, `.has_axis(Axis)`

  **Acceptance Criteria**:
  - [ ] `cargo clippy --all-targets -- -D warnings` passes
  - [ ] `cargo build --release` succeeds
  - [ ] No `thread::sleep(Duration::from_millis(10))` in the unified poll loop
  - [ ] `rustix::event::poll` used for event-driven wakeup
  - [ ] `libinput-scroll` thread spawn removed from `start_idle_monitor`
  - [ ] All 12 `InputSnapshot` fields populated from libinput events

  **QA Scenarios**:
  ```
  Scenario: Build and lint clean
    Tool: Bash
    Steps:
      1. `cargo clippy --all-targets -- -D warnings`
      2. `cargo build --release`
    Expected Result: Zero warnings, clean build
    Evidence: .sisyphus/evidence/task-5-build.txt

  Scenario: poll(2) used instead of busy-wait
    Tool: Bash (grep)
    Steps:
      1. grep for `rustix::event::poll` in src/input.rs
      2. grep for `thread::sleep(Duration::from_millis(10))` — should NOT be in unified loop
    Expected Result: poll found, sleep(10ms) removed from input loop
    Evidence: .sisyphus/evidence/task-5-poll.txt

  Scenario: Runtime keystroke capture works
    Tool: Bash
    Steps:
      1. `cargo install --path .`
      2. Launch `niri-activity-rs watch -q &` and capture PID
      3. Sleep 10s (user generates input naturally)
      4. Query DB: `sqlite3 activity.db "SELECT SUM(keystrokes) FROM events WHERE timestamp > datetime('now', '-30 seconds')"`
      5. Kill daemon
      6. Assert keystrokes > 0
    Expected Result: Keystrokes captured via unified libinput thread
    Evidence: .sisyphus/evidence/task-5-runtime.txt
  ```

  **Commit**: YES
  - Message: `refactor(input): unify evdev+libinput into single libinput thread with poll(2)`
  - Files: `src/input.rs`

- [x] 6. Remove dead evdev code and inotify thread

  **What to do**:
  - Remove `enumerate_input_devices()` function
  - Remove dead constants no longer referenced: `REL_X`, `REL_Y`, `REL_WHEEL`, `REL_HWHEEL`, `REL_WHEEL_HI_RES`, `REL_HWHEEL_HI_RES`, `ABS_X`, `ABS_Y`, `ABS_MT_POSITION_X`, `ABS_MT_POSITION_Y`, `BTN_TOUCH`, `BTN_TOOL_FINGER`, `BTN_MOUSE_RANGE`
  - Keep `KEY_*` constants and `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE` if used for libinput button code comparison
  - Keep `SCROLL_NOTCH_THRESHOLD` if still used
  - Remove `evdev` from `Cargo.toml` dependencies
  - Remove the inotify hotplug thread from `start_idle_monitor` (libinput handles `DEVICE_ADDED`/`DEVICE_REMOVED` natively)
  - Remove `inotify` from `Cargo.toml` if not used elsewhere
  - Remove `devices_changed: Arc<AtomicBool>` since inotify thread is gone
  - Verify each removal with `cargo build` to ensure nothing depends on removed items

  **Must NOT do**:
  - Remove constants that are still referenced
  - Break compilation

  **Recommended Agent Profile**:
  - **Category**: `unspecified-low`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: NO
  - **Blocks**: Task 7
  - **Blocked By**: Task 5

  **References**:
  - `src/input.rs:316-344` — `enumerate_input_devices()` (to remove)
  - `src/input.rs:20-52` — constants (check which are still used)
  - `src/input.rs:690-747` — inotify hotplug thread (to remove)
  - `Cargo.toml` — `evdev = "0.13"` and `inotify` entries (to remove)

  **Acceptance Criteria**:
  - [ ] `cargo build` succeeds without `evdev` dependency
  - [ ] `cargo clippy --all-targets -- -D warnings` passes
  - [ ] No `evdev::` usage in codebase
  - [ ] No `enumerate_input_devices` references
  - [ ] No unused import warnings

  **QA Scenarios**:
  ```
  Scenario: Clean build without evdev
    Tool: Bash
    Steps:
      1. `cargo build --release`
      2. `cargo clippy --all-targets -- -D warnings`
      3. grep for `evdev::` in src/ — should return nothing
    Expected Result: Clean build, no evdev references
    Evidence: .sisyphus/evidence/task-6-clean.txt
  ```

  **Commit**: YES
  - Message: `refactor(input): remove dead evdev code and inotify thread`
  - Files: `src/input.rs`, `Cargo.toml`

---

## Final Verification Wave

- [x] F1. **Build + Clippy + Test Verification** — `quick`
  Run `cargo clippy --all-targets -- -D warnings`, `cargo build --release`, `cargo test`. All must pass with zero warnings/errors.
  Output: `Build [PASS/FAIL] | Clippy [PASS/FAIL] | Tests [N pass/N fail] | VERDICT`

- [x] F2. **Runtime Verification** — `unspecified-high`
  Install binary via `cargo install --path .`. Run `niri-activity-rs watch` for 15 seconds while generating keyboard + mouse + scroll input. Check SQLite DB for events with non-zero keystrokes, mouse_clicks, scroll_events, mouse_distance. Verify thread count is reduced (should be 2-3 threads: input-poll, jiggler-scan, optionally input-hotplug). Kill daemon, verify graceful shutdown flush.
  Output: `Keystroke capture [PASS/FAIL] | Click capture [PASS/FAIL] | Scroll [PASS/FAIL] | Thread count [N] | Shutdown [PASS/FAIL] | VERDICT`

---

## Commit Strategy

- **Commit 1** (Wave 1): `refactor(input): extract InputCounters struct and add rustix dependency`
  Files: `Cargo.toml`, `src/input.rs`
  Pre-commit: `cargo clippy --all-targets -- -D warnings && cargo test`

- **Commit 2** (Wave 2): `refactor(input): thread InputCounters through poll functions`
  Files: `src/input.rs`
  Pre-commit: `cargo clippy --all-targets -- -D warnings && cargo test`

- **Commit 3** (Wave 3): `fix(input): track scroll direction for libinput scroll sources`
  Files: `src/input.rs`
  Pre-commit: `cargo clippy --all-targets -- -D warnings && cargo test`

- **Commit 4** (Wave 4): `refactor(input): unify evdev+libinput into single libinput thread with poll(2)`
  Files: `src/input.rs`
  Pre-commit: `cargo clippy --all-targets -- -D warnings && cargo test`

- **Commit 5** (Wave 5): `refactor(input): remove dead evdev code and inotify thread`
  Files: `src/input.rs`, `Cargo.toml`
  Pre-commit: `cargo clippy --all-targets -- -D warnings && cargo test`

---

## Success Criteria

### Verification Commands
```bash
cargo clippy --all-targets -- -D warnings  # Expected: zero warnings
cargo build --release                       # Expected: clean build
cargo test                                  # Expected: all tests pass
```

### Final Checklist
- [ ] Single libinput thread replaces dual evdev+libinput threads
- [ ] poll(2) used — zero CPU when idle
- [ ] InputCounters struct — no more 20+ param functions
- [ ] Scroll direction tracked for ALL sources
- [ ] All existing tests pass
- [ ] InputStats/InputSnapshot public API unchanged
- [ ] evdev and inotify dependencies removed
