# niri-activity-rs Codebase Improvement

## TL;DR

> **Quick Summary**: Comprehensive code quality, architecture, and project hygiene pass informed by a 4-agent deep review + Metis gap analysis. Fixes real issues, removes false positives from initial review.
> 
> **Deliverables**:
> - Release profile optimizations (smaller/faster binary)
> - All minor/patch deps updated + toml 0.8→1.x major bump
> - Magic numbers extracted to named constants
> - report/mod.rs (2871 lines) split into 4 focused submodules
> - watcher.rs watch() (703 lines) extracted into 5-6 focused functions
> - 81 eprintln! calls migrated to tracing (with TUI-aware subscriber)
> - ~8-10 unnecessary .clone() calls eliminated
> - Error enum expanded with 3-6 specific variants
> - Doc comments on all public functions
> - 10-15 new unit tests for report query functions
> - CI/CD workflow (build + clippy + test)
> 
> **Estimated Effort**: Large
> **Parallel Execution**: YES — 4 waves
> **Critical Path**: Task 1 → Task 4 → Task 10 → Task 11

---

## Context

### Original Request
User requested a full codebase review of niri-activity-rs with recommendations on what to change/modify, then asked for a work plan covering all findings.

### Interview Summary
**Key Discussions**:
- 4 parallel agents reviewed: architecture, Rust idioms, dependencies, build health
- User confirmed they want ALL actionable items addressed in one plan
- No explicit priority ordering given — user trusts the plan

**Research Findings**:
- Project compiles clean, clippy passes, no errors
- Clean git working tree, up to date with origin/master
- Edition 2024, no workspace, 24 direct deps, 351 total
- Lock-free concurrency with atomics — well done
- Saturating arithmetic throughout — no panic risk
- Good enum state machines, timezone handling
- 12 unit tests in config.rs, no other tests

### Metis Review
**Identified Gaps (addressed)**:

- **"205 eprintln!() calls" was WRONG**: Actual count is 81 `eprintln!` + 205 `println!`. 107 of those `println!` are user-facing report output in show_*/generate_*/export_* functions — these MUST NOT be migrated to tracing.

- **"44 unnecessary .clone() calls" was MISLEADING**: Only ~8-10 are truly unnecessary. ~34 are required by ownership semantics (HashMap insertion, thread spawning, struct construction). "Hot paths" framing was inaccurate — report code runs once per CLI invocation.

- **"Regex → str::contains()" was WRONG**: Production code already branches correctly based on `regex = true/false` config flag. Architecture handles this.

- **"MSRV violations" is a NON-ISSUE**: Edition 2024 requires Rust 1.85+, which covers all used APIs (cast_signed 1.87, is_multiple_of 1.87, Duration::from_mins 1.84). These are clippy false positives given the edition.

- **"LazyLock for regex" is WRONG**: Regex patterns come from user config at runtime, not hardcoded statics. LazyLock is inapplicable.

- **"excel-extra-sheets → runtime config" is TOO COMPLEX**: Feature gates 12 `#[cfg]` blocks including different `use` imports and function signatures. Non-trivial conversion — deferred.

- **TUI + tracing conflict identified**: ratatui takes over terminal. Any tracing output to stderr during TUI mode would corrupt display. Subscriber must be conditional (file for TUI, stderr for CLI).

- **niri-ipc path dependency breaks CI**: `path = "../niri/niri-ipc"` won't resolve in CI. Must handle in workflow.

---

## Work Objectives

### Core Objective
Improve code quality, architecture, and project hygiene across niri-activity-rs based on verified findings from a comprehensive review.

### Concrete Deliverables
- `Cargo.toml` with release profile, updated deps, MSRV declaration
- `src/report/` split into `query.rs`, `display.rs`, `export.rs`, `analysis.rs`
- `src/watcher.rs` with extracted functions, `watch()` < 200 lines
- All `eprintln!` replaced with `tracing` macros
- `src/error.rs` with 3-6 new specific variants
- `src/report/tests.rs` with 10-15 new unit tests
- `.github/workflows/ci.yml`

### Definition of Done
- [ ] `cargo build --release` succeeds with zero warnings
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test` passes with ≥21 tests (was 12)
- [ ] Release binary exists and is functional

### Must Have
- Every task passes `cargo build && cargo clippy && cargo test`
- report/mod.rs split preserves all public API — main.rs requires zero changes
- watcher.rs watch() function body < 200 lines
- Zero `eprintln!` remaining after tracing migration
- `println!` count in report display functions UNCHANGED (107 calls)
- TUI mode shows no corrupted output from tracing

### Must NOT Have (Guardrails)
- **G1**: MUST NOT migrate `println!` in report display/export functions to tracing — those 107 calls ARE the user-facing output
- **G2**: report/mod.rs split is EXTRACTION ONLY — no signature changes, no renames, no public API changes
- **G3**: Error enum expansion MAX 6 new variants — keep String fallbacks
- **G4**: Clone elimination targets ONLY verified unnecessary clones (~8-10) — DO NOT touch the ~34 required clones
- **G5**: No new dependencies beyond `tracing` + `tracing-subscriber` without justification
- **G6**: DO NOT fix "regex → str::contains()" — architecture already handles this correctly
- **G7**: DO NOT convert excel-extra-sheets feature to runtime config — too complex for this pass
- **G8**: DO NOT add MSRV violation fixes — Edition 2024 covers all used APIs
- **G9**: One commit per logical task, each commit passes build+clippy+test

---

## Verification Strategy

> **ZERO HUMAN INTERVENTION** — ALL verification is agent-executed. No exceptions.

### Test Decision
- **Infrastructure exists**: YES (12 unit tests in config.rs)
- **Automated tests**: Tests-after (add tests for report query functions)
- **Framework**: `cargo test` (built-in)

### QA Policy
Every task MUST include agent-executed QA scenarios.
Evidence saved to `.sisyphus/evidence/task-{N}-{scenario-slug}.{ext}`.

- **Build verification**: `cargo build && cargo clippy && cargo test`
- **Metric checks**: Line counts, grep counts, binary size comparisons
- **Functional checks**: Run relevant CLI subcommands to verify behavior

---

## Execution Strategy

### Parallel Execution Waves

```
Wave 1 (Start Immediately — foundation, no dependencies):
├── Task 1: Establish baseline + release profile [quick]
├── Task 2: Dependency version bumps [quick]
└── Task 3: Extract magic numbers to constants [quick]

Wave 2 (After Wave 1 — core refactoring, MAX PARALLEL):
├── Task 4: Split report/mod.rs into submodules (depends: 1, 3) [unspecified-high]
├── Task 5: Extract watcher.rs watch() function (depends: 1) [deep]
└── Task 6: Migrate eprintln! to tracing (depends: 2) [unspecified-high]

Wave 3 (After Wave 2 — refinement):
├── Task 7: Eliminate unnecessary .clone() calls (depends: 4, 5) [unspecified-low]
├── Task 8: Expand error enum (depends: 5, 6) [unspecified-low]
├── Task 9: Add doc comments (depends: 4) [quick]
└── Task 10: Add unit tests for report queries (depends: 4) [deep]

Wave 4 (After Wave 3 — CI):
└── Task 11: Add CI/CD workflow (depends: 10) [quick]

Wave FINAL (After ALL tasks — 4 parallel reviews, then user okay):
├── Task F1: Plan compliance audit (oracle)
├── Task F2: Code quality review (unspecified-high)
├── Task F3: Real manual QA (unspecified-high)
└── Task F4: Scope fidelity check (deep)
→ Present results → Get explicit user okay

Critical Path: Task 1 → Task 4 → Task 10 → Task 11 → F1-F4 → user okay
Parallel Speedup: ~60% faster than sequential (4 waves vs 11 sequential)
Max Concurrent: 3 (Waves 1 & 2)
```

### Dependency Matrix

| Task | Depends On | Blocks    | Wave |
| ---- | ---------- | --------- | ---- |
| 1    | —          | 4, 5      | 1    |
| 2    | —          | 6, 11     | 1    |
| 3    | —          | 4         | 1    |
| 4    | 1, 3       | 7, 9, 10  | 2    |
| 5    | 1          | 7, 8      | 2    |
| 6    | 2          | 8         | 2    |
| 7    | 4, 5       | —         | 3    |
| 8    | 5, 6       | —         | 3    |
| 9    | 4          | —         | 3    |
| 10   | 4          | 11        | 3    |
| 11   | 10         | —         | 4    |

### Agent Dispatch Summary

- **Wave 1**: **3** — T1 → `quick`, T2 → `quick`, T3 → `quick`
- **Wave 2**: **3** — T4 → `unspecified-high`, T5 → `deep`, T6 → `unspecified-high`
- **Wave 3**: **4** — T7 → `unspecified-low`, T8 → `unspecified-low`, T9 → `quick`, T10 → `deep`
- **Wave 4**: **1** — T11 → `quick`
- **FINAL**: **4** — F1 → `oracle`, F2 → `unspecified-high`, F3 → `unspecified-high`, F4 → `deep`

---

## TODOs

- [x] 1. Establish Baseline & Release Profile

  **What to do**:
  - Run `cargo test` to verify all 12 existing tests pass — record output as baseline
  - Add release profile to `Cargo.toml`:
    ```toml
    [profile.release]
    opt-level = 3
    lto = "thin"
    codegen-units = 1
    strip = "symbols"
    ```
  - Run `cargo build --release` and record binary size
  - Add `rust-version = "1.85"` to `[package]` section (Edition 2024 minimum)

  **Must NOT do**:
  - Do NOT use `lto = true` (full LTO) — `"thin"` is safer with bundled SQLite C code
  - Do NOT add `panic = "abort"` — the project may rely on unwinding for cleanup
  - Do NOT change any source code — this is config only

  **Recommended Agent Profile**:
  - **Category**: `quick`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with Tasks 2, 3)
  - **Blocks**: Tasks 4, 5
  - **Blocked By**: None

  **References**:
  - `Cargo.toml:1-15` — Package metadata section where rust-version goes
  - `Cargo.toml` bottom — Where to add `[profile.release]` section
  - `src/config.rs` tests — The 12 existing tests to verify as baseline

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: Baseline tests pass
    Tool: Bash
    Steps:
      1. Run `cargo test 2>&1`
      2. Assert output contains "test result: ok"
      3. Assert test count ≥ 12
    Expected Result: All existing tests pass
    Evidence: .sisyphus/evidence/task-1-baseline-tests.txt

  Scenario: Release build succeeds with optimizations
    Tool: Bash
    Steps:
      1. Run `cargo build --release 2>&1`
      2. Assert exit code 0
      3. Run `ls -la target/release/niri-activity-rs`
      4. Record binary size
    Expected Result: Binary compiles successfully
    Evidence: .sisyphus/evidence/task-1-release-build.txt

  Scenario: rust-version declared
    Tool: Bash
    Steps:
      1. Run `grep 'rust-version' Cargo.toml`
      2. Assert output contains "1.85"
    Expected Result: MSRV is declared
    Evidence: .sisyphus/evidence/task-1-msrv.txt
  ```

  **Commit**: YES
  - Message: `build: add release profile optimizations and declare MSRV`
  - Files: `Cargo.toml`
  - Pre-commit: `cargo build --release && cargo test`

- [x] 2. Dependency Version Bumps

  **What to do**:
  - Update all 9 minor/patch dependencies in `Cargo.toml`:
    - `chrono` 0.4.43 → latest 0.4.x
    - `clap` 4.5.56 → latest 4.x
    - `inotify` 0.11.0 → latest 0.11.x
    - `lettre` 0.11.19 → latest 0.11.x
    - `libc` 0.2.180 → latest 0.2.x
    - `owo-colors` 4.2.3 → latest 4.x
    - `rusqlite` 0.38.0 → latest (check if 0.39.x is compatible)
    - `tempfile` 3.25.0 → latest 3.x
    - `zbus` 5.13.2 → latest 5.x
  - Bump `toml` from `"0.8"` to `"1"` (major version)
  - After toml bump: verify `Config(#[from] toml::de::Error)` in `src/error.rs` still compiles
  - Run `cargo update`
  - DO NOT touch `rust_xlsxwriter` (git fork — leave pinned)

  **Must NOT do**:
  - Do NOT update rust_xlsxwriter fork
  - Do NOT add new dependencies
  - Do NOT change any source code — deps only

  **Recommended Agent Profile**:
  - **Category**: `quick`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with Tasks 1, 3)
  - **Blocks**: Tasks 6, 11
  - **Blocked By**: None

  **References**:
  - `Cargo.toml:12-50` — Dependencies section with current versions
  - `src/error.rs:11` — `Config(#[from] toml::de::Error)` that must still work after toml bump
  - `Cargo.lock` — Will be regenerated by `cargo update`

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: All deps updated and build passes
    Tool: Bash
    Steps:
      1. Run `cargo build 2>&1`
      2. Assert exit code 0, zero warnings
      3. Run `cargo clippy 2>&1`
      4. Assert exit code 0
      5. Run `cargo test 2>&1`
      6. Assert "test result: ok"
    Expected Result: Clean build with updated deps
    Evidence: .sisyphus/evidence/task-2-deps-build.txt

  Scenario: toml major bump doesn't break error type
    Tool: Bash
    Steps:
      1. Run `grep -n 'toml::de::Error' src/error.rs`
      2. Assert the line exists
      3. Run `cargo build 2>&1 | grep -i 'error'`
      4. Assert no compilation errors
    Expected Result: toml::de::Error still resolves correctly
    Evidence: .sisyphus/evidence/task-2-toml-compat.txt
  ```

  **Commit**: YES
  - Message: `deps: update all dependencies including toml 0.8→1.x`
  - Files: `Cargo.toml`, `Cargo.lock`
  - Pre-commit: `cargo build && cargo clippy && cargo test`

- [x] 3. Extract Magic Numbers to Named Constants

  **What to do**:
  - Search for hardcoded numeric literals that appear 2+ times or whose meaning is non-obvious
  - Extract to named `const` values near the top of each file or in a dedicated constants block
  - Key targets:
    - `src/report/mod.rs`: `3_600_000` (MS_PER_HOUR), `60_000` (MS_PER_MIN), `86_400_000` (MS_PER_DAY), `15` (APP_DISPLAY_LIMIT), `900_000` (TIMELINE_BUCKET_MS — 15 min), `300_000` (MIN_STREAK_MS)
    - `src/input.rs`: `10_000` (MAX_TRACKER_CAPACITY), `100` (JIGGLER_INTERVAL_THRESHOLD_MS), `15.0` (SCROLL_NOTCH_THRESHOLD)
    - `src/watcher.rs`: `300` (5-min flush interval in secs), `30` (suspend detection threshold secs)
  - Use `ast_grep_search` or grep to find all numeric literals first
  - Name constants descriptively following Rust `SCREAMING_SNAKE_CASE` convention

  **Must NOT do**:
  - Do NOT extract obviously clear literals like `0`, `1`, `2`, `100` (percentages)
  - Do NOT create a separate constants.rs file — keep constants near their usage
  - Do NOT change any logic — pure renaming

  **Recommended Agent Profile**:
  - **Category**: `quick`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with Tasks 1, 2)
  - **Blocks**: Task 4 (constants should be in place before report split)
  - **Blocked By**: None

  **References**:
  - `src/report/mod.rs` — Search for `3_600_000`, `60_000`, `86_400_000`, `900_000`
  - `src/input.rs:35-50` — Existing constant definitions to follow the pattern
  - `src/watcher.rs` — Search for `Duration::from_secs(300)`, `Duration::from_secs(30)`

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: Magic numbers replaced with named constants
    Tool: Bash
    Steps:
      1. Run `grep -rn '3_600_000\|3600000' src/`
      2. Assert 0 matches (all extracted to MS_PER_HOUR or similar)
      3. Run `grep -rn 'const.*MS_PER' src/report/`
      4. Assert ≥ 3 constant definitions exist
    Expected Result: Numeric literals replaced with descriptive names
    Evidence: .sisyphus/evidence/task-3-constants.txt

  Scenario: Build still passes after extraction
    Tool: Bash
    Steps:
      1. Run `cargo build && cargo clippy && cargo test`
      2. Assert all pass
    Expected Result: Pure rename — zero behavior change
    Evidence: .sisyphus/evidence/task-3-build.txt
  ```

  **Commit**: YES
  - Message: `refactor: extract magic numbers to named constants`
  - Files: `src/report/mod.rs`, `src/input.rs`, `src/watcher.rs`
  - Pre-commit: `cargo build && cargo test`

- [x] 4. Split report/mod.rs into Submodules

  **What to do**:
  - Extract `src/report/mod.rs` (2871 lines) into 4 new submodules:
    - `src/report/query.rs` — All query functions: `query_today()`, `query_metrics_range()`, `query_timeline()`, `query_report_range()`, and private helpers like `group_apps()`, `classify_gap()`, `dominant_app()`, `query_streaks()`, `query_gaps()`, `query_input_metrics()`, `query_flow_sessions()`, `query_fatigue_indicators()`, `compute_*()` helpers
    - `src/report/display.rs` — All terminal display functions: `show_today()`, `show_metrics_range()`, `show_timeline()`, `generate_report_range()`, `show_comparison()`, `delta_str()` helpers
    - `src/report/export.rs` — All export functions: `export_csv_range()`, `export_json_range()`, `export_cron_summary()`, `export_heatmap_range()`
  - Keep in `src/report/mod.rs`: `App` struct, `TimeRange` enum, `TimeBounds` struct, all `pub use` re-exports
  - The `mod.rs` file should be < 150 lines after extraction
  - `main.rs` must require ZERO changes — all public API preserved via re-exports
  - Use `lsp_find_references` on each function before moving to verify no breakage

  **Must NOT do**:
  - Do NOT change any function signatures
  - Do NOT rename any functions
  - Do NOT change any public API — only file locations change
  - Do NOT touch `excel.rs` or `types.rs` (existing submodules)
  - Do NOT redesign the query/display API or add abstractions

  **Recommended Agent Profile**:
  - **Category**: `unspecified-high`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 2 (with Tasks 5, 6)
  - **Blocks**: Tasks 7, 9, 10
  - **Blocked By**: Tasks 1, 3

  **References**:
  - `src/report/mod.rs` — The 2871-line file to split
  - `src/report/types.rs` — Existing submodule pattern to follow
  - `src/report/excel.rs` — Existing submodule pattern to follow
  - `src/main.rs:9` — `mod report; use report::*;` — must not change
  - `src/tui.rs` — Imports from report module — must not break

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: mod.rs is small, new files exist
    Tool: Bash
    Steps:
      1. Run `wc -l src/report/mod.rs`
      2. Assert < 150 lines
      3. Run `ls src/report/`
      4. Assert query.rs, display.rs, export.rs all exist
      5. Run `wc -l src/report/query.rs src/report/display.rs src/report/export.rs`
      6. Assert combined > 2500 lines (content moved, not deleted)
    Expected Result: Code extracted, not lost
    Evidence: .sisyphus/evidence/task-4-file-sizes.txt

  Scenario: Public API unchanged
    Tool: Bash
    Steps:
      1. Run `cargo build && cargo clippy && cargo test`
      2. Assert all pass with zero warnings
      3. Run `grep -c 'pub use' src/report/mod.rs`
      4. Assert re-exports exist for all public items
      5. Run `grep 'use report' src/main.rs src/tui.rs`
      6. Assert no changes needed in consumer files
    Expected Result: Pure extraction — zero API change
    Evidence: .sisyphus/evidence/task-4-api-check.txt

  Scenario: main.rs unchanged
    Tool: Bash
    Steps:
      1. Run `git diff src/main.rs`
      2. Assert empty output (no changes)
    Expected Result: Consumer code unaffected
    Evidence: .sisyphus/evidence/task-4-main-unchanged.txt
  ```

  **Commit**: YES
  - Message: `refactor: split report/mod.rs into query, display, export submodules`
  - Files: `src/report/mod.rs`, `src/report/query.rs`, `src/report/display.rs`, `src/report/export.rs`
  - Pre-commit: `cargo build && cargo clippy && cargo test`

- [x] 5. Extract watcher.rs watch() into Focused Functions

  **What to do**:
  - Break the `watch()` function (lines ~245-948, ~703 lines) into 5-6 focused private functions:
    - `handle_shutdown(signal_flag, ...)` — SIGINT/SIGTERM handling + final flush
    - `handle_suspend_resume(logind, accum, ...)` — Wall-clock jump detection + D-Bus resume signal handling
    - `handle_lock_unlock(logind, accum, ...)` — Lock state transitions + flush on lock
    - `handle_idle_transitions(accum, config, ...)` — Active→Passive→Idle→Away state machine
    - `handle_niri_event(event, accum, ...)` — WindowsChanged, WindowFocusChanged, WindowClosed dispatch
    - `print_status_line(window, state, ...)` — Colored status output formatting
  - Follow the EXISTING extraction pattern: `flush_session()`, `FlushContext`, `SessionAccum` are already helpers — extend this approach
  - `watch()` becomes an orchestrator: event loop + dispatch to handlers, body < 200 lines
  - All new functions are `fn` (private), not `pub fn`

  **Must NOT do**:
  - Do NOT change any behavior — pure extraction
  - Do NOT add new state or modify ActivityState enum
  - Do NOT touch `flush_session()`, `FlushContext`, `SessionAccum` (existing helpers)
  - Do NOT make any extracted function `pub`

  **Recommended Agent Profile**:
  - **Category**: `deep`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 2 (with Tasks 4, 6)
  - **Blocks**: Tasks 7, 8
  - **Blocked By**: Task 1

  **References**:
  - `src/watcher.rs:245-948` — The watch() function to extract from
  - `src/watcher.rs:33-94` — `FlushContext`, `SessionAccum`, `FlushReset` — existing extraction pattern to follow
  - `src/watcher.rs:100-220` — `flush_session()`, `connect_to_niri()` — existing helpers showing the style
  - `src/watcher.rs:1-30` — `ActivityState` enum, `millis_u64()` — state definitions

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: watch() is under 200 lines
    Tool: Bash
    Steps:
      1. Run `grep -n 'pub fn watch' src/watcher.rs` to find start line
      2. Count lines to the closing brace
      3. Assert body < 200 lines
    Expected Result: Function is now an orchestrator, not a monolith
    Evidence: .sisyphus/evidence/task-5-watch-size.txt

  Scenario: New helper functions exist
    Tool: Bash
    Steps:
      1. Run `grep '^fn ' src/watcher.rs | wc -l`
      2. Assert ≥ 10 private functions (was ~5)
      3. Run `grep 'pub fn' src/watcher.rs | wc -l`
      4. Assert only `watch` is public
    Expected Result: Logic extracted to focused helpers
    Evidence: .sisyphus/evidence/task-5-functions.txt

  Scenario: Build and tests pass
    Tool: Bash
    Steps:
      1. Run `cargo build && cargo clippy && cargo test`
      2. Assert all pass
    Expected Result: Zero behavior change
    Evidence: .sisyphus/evidence/task-5-build.txt
  ```

  **Commit**: YES
  - Message: `refactor: extract watcher watch() into focused handler functions`
  - Files: `src/watcher.rs`
  - Pre-commit: `cargo build && cargo clippy && cargo test`

- [x] 6. Migrate eprintln! to tracing

  **What to do**:
  - Add dependencies to `Cargo.toml`:
    ```toml
    tracing = "0.1"
    tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
    ```
  - Add subscriber initialization in `src/main.rs`:
    - For CLI commands: stderr subscriber with env-filter (`RUST_LOG=info` default)
    - For TUI command: file-based subscriber OR disable tracing (ratatui takes over terminal — stderr output would corrupt display)
  - Replace all 81 `eprintln!` calls with appropriate tracing macros:
    - Error conditions → `tracing::error!`
    - Warnings (e.g., thread failures) → `tracing::warn!`
    - Status info (e.g., "watching...") → `tracing::info!`
    - Debug details → `tracing::debug!`
  - Replace ~30 diagnostic `println!` calls in `watcher.rs`, `input.rs`, `logind.rs`, `agent_activity.rs` with tracing macros
  - **CRITICAL**: DO NOT touch `println!` in `src/report/mod.rs` (or its new submodules after task 4) — those 107 calls ARE user-facing report output
  - Remove `print_stderr = "allow"` from clippy config in `Cargo.toml` once migration is complete

  **Must NOT do**:
  - MUST NOT migrate `println!` in report display/export functions (show_*, generate_*, export_*)
  - MUST NOT add structured tracing fields in v1 — simple message strings only
  - MUST NOT add span instrumentation in v1
  - MUST NOT add log rotation or file output for CLI mode

  **Recommended Agent Profile**:
  - **Category**: `unspecified-high`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 2 (with Tasks 4, 5)
  - **Blocks**: Task 8
  - **Blocked By**: Task 2

  **References**:
  - `src/watcher.rs` — Grep for `eprintln!` — majority of diagnostic logging lives here
  - `src/input.rs` — Grep for `eprintln!` — input thread error logging
  - `src/logind.rs` — Grep for `eprintln!` — D-Bus thread error logging
  - `src/agent_activity.rs` — Grep for `eprintln!` — agent detection logging
  - `src/main.rs` — Where to add tracing subscriber init
  - `src/tui.rs` — TUI mode that conflicts with stderr output
  - `src/report/mod.rs` — 107 `println!` calls that MUST NOT be touched

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: Zero eprintln! remaining
    Tool: Bash
    Steps:
      1. Run `grep -rn 'eprintln!' src/ | wc -l`
      2. Assert result is 0
    Expected Result: All diagnostic logging migrated to tracing
    Evidence: .sisyphus/evidence/task-6-no-eprintln.txt

  Scenario: Report println! count unchanged
    Tool: Bash
    Steps:
      1. Run `grep -c 'println!' src/report/mod.rs src/report/display.rs src/report/export.rs 2>/dev/null | paste -sd+ | bc`
      2. Assert count ≈ 107 (may shift slightly due to task 4 file split)
    Expected Result: User-facing output untouched
    Evidence: .sisyphus/evidence/task-6-println-preserved.txt

  Scenario: Build passes with tracing
    Tool: Bash
    Steps:
      1. Run `cargo build && cargo clippy && cargo test`
      2. Assert all pass
    Expected Result: Clean integration
    Evidence: .sisyphus/evidence/task-6-build.txt

  Scenario: TUI mode doesn't show corrupted output
    Tool: interactive_bash (tmux)
    Steps:
      1. Run `cargo run -- tui --today` in tmux
      2. Wait 2 seconds, observe terminal
      3. Assert no tracing output visible in TUI
      4. Send 'q' key to quit
    Expected Result: TUI renders cleanly without tracing interference
    Evidence: .sisyphus/evidence/task-6-tui-clean.png
  ```

  **Commit**: YES
  - Message: `feat: migrate diagnostic logging from eprintln to tracing`
  - Files: `Cargo.toml`, `src/main.rs`, `src/watcher.rs`, `src/input.rs`, `src/logind.rs`, `src/agent_activity.rs`
  - Pre-commit: `cargo build && cargo clippy && cargo test`

- [x] 7. Eliminate Unnecessary .clone() Calls

  **What to do**:
  - Target ONLY the ~8-10 verified unnecessary clones. Use `ast_grep_search` to find all `.clone()` sites first, then assess each individually:
    - `src/report/` (after split): `bounds.since_str.clone()` and `bounds.now_str.clone()` — use `&str` references instead
    - `src/report/` (after split): duplicate `entry.app_id.clone()` in loops — clone once into a local, reuse
    - `src/tui.rs`: `range.clone()` — pass by reference where the callee doesn't need ownership
    - `src/tui.rs`: `config.schedule.start.clone()` — borrow instead
    - `src/watcher.rs` (after extraction): assess clones in extracted functions for reference opportunities
  - Each clone removal must compile clean — assess individually, not bulk-replace

  **Must NOT do**:
  - Do NOT touch clones required by HashMap insertion (~10 instances)
  - Do NOT touch clones required by thread spawning / move closures (~5 instances)
  - Do NOT touch clones required by struct construction where ownership is needed (~19 instances)
  - Do NOT introduce lifetime parameters to avoid clones — keep it simple

  **Recommended Agent Profile**:
  - **Category**: `unspecified-low`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 3 (with Tasks 8, 9, 10)
  - **Blocks**: None
  - **Blocked By**: Tasks 4, 5

  **References**:
  - `src/report/query.rs` (after task 4) — Search for `.clone()` in query functions
  - `src/tui.rs:65,67,90,92` — TUI clone sites
  - `src/watcher.rs` (after task 5) — Extracted function clone sites

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: Clone count decreased
    Tool: Bash
    Steps:
      1. Run `grep -rc '\.clone()' src/report/ src/tui.rs src/watcher.rs | paste -sd+ | bc`
      2. Assert count decreased by 6-10 from baseline
    Expected Result: Unnecessary allocations removed
    Evidence: .sisyphus/evidence/task-7-clone-count.txt

  Scenario: Build passes — no ownership errors
    Tool: Bash
    Steps:
      1. Run `cargo build && cargo clippy && cargo test`
      2. Assert all pass
    Expected Result: All remaining clones are necessary
    Evidence: .sisyphus/evidence/task-7-build.txt
  ```

  **Commit**: YES
  - Message: `perf: eliminate unnecessary .clone() calls in report and tui`
  - Files: `src/report/*.rs`, `src/tui.rs`, `src/watcher.rs`
  - Pre-commit: `cargo build && cargo clippy && cargo test`

- [x] 8. Expand Error Enum with Specific Variants

  **What to do**:
  - Add 3-6 specific error variants to `src/error.rs`:
    ```rust
    #[error("failed to connect to niri IPC socket")]
    NiriConnectionFailed,
    
    #[error("niri event stream closed unexpectedly")]
    NiriEventStreamClosed,
    
    #[error("invalid date range: {from} to {to}")]
    InvalidDateRange { from: String, to: String },
    
    #[error("failed to connect to logind D-Bus")]
    LogindConnectionFailed,
    
    #[error("logind session not found for current user")]
    LogindSessionNotFound,
    ```
  - KEEP the existing `NiriIpc(String)`, `NiriError(String)`, `Logind(String)` as catch-all fallbacks
  - Update call sites where the specific error variant fits — use `grep` to find all `Error::NiriError(format!` etc.
  - Total `error.rs` should be < 60 lines

  **Must NOT do**:
  - Do NOT add more than 6 new variants
  - Do NOT remove existing String-based variants (they serve as fallbacks)
  - Do NOT add `#[from]` on new variants unless they wrap a specific error type
  - Do NOT add custom Display impls — use thiserror `#[error("...")]`

  **Recommended Agent Profile**:
  - **Category**: `unspecified-low`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 3 (with Tasks 7, 9, 10)
  - **Blocks**: None
  - **Blocked By**: Tasks 5, 6

  **References**:
  - `src/error.rs` — Current error enum (21 lines)
  - `src/watcher.rs` — Search for `Error::NiriError`, `Error::NiriIpc` construction sites
  - `src/logind.rs` — Search for `Error::Logind` construction sites
  - `src/main.rs` — Error handling in command dispatch

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: Error enum expanded correctly
    Tool: Bash
    Steps:
      1. Run `wc -l src/error.rs`
      2. Assert < 60 lines
      3. Run `grep -c 'NiriConnectionFailed\|NiriEventStreamClosed\|InvalidDateRange\|LogindConnectionFailed\|LogindSessionNotFound' src/error.rs`
      4. Assert ≥ 3 new variants exist
    Expected Result: Specific variants added, file stays concise
    Evidence: .sisyphus/evidence/task-8-error-enum.txt

  Scenario: All match arms exhaustive
    Tool: Bash
    Steps:
      1. Run `cargo build && cargo clippy && cargo test`
      2. Assert all pass — no non-exhaustive match warnings
    Expected Result: All error consumers handle new variants
    Evidence: .sisyphus/evidence/task-8-build.txt
  ```

  **Commit**: YES
  - Message: `refactor: expand error enum with specific variants`
  - Files: `src/error.rs`, `src/watcher.rs`, `src/logind.rs`
  - Pre-commit: `cargo build && cargo clippy && cargo test`

- [x] 9. Add Doc Comments to Public Functions

  **What to do**:
  - Add one-liner `///` doc comments to all undocumented `pub fn` across the codebase (~25 functions)
  - Add module-level `//!` docs to new submodules from task 4: `query.rs`, `display.rs`, `export.rs`
  - Focus on PURPOSE, not implementation details
  - Examples of good doc comments:
    ```rust
    /// Query today's activity breakdown by application.
    pub fn query_today(...) { ... }
    
    /// Export activity data as CSV to the specified path.
    pub fn export_csv_range(...) { ... }
    ```

  **Must NOT do**:
  - No parameter-level docs in v1 — just purpose statements
  - No examples in doc comments in v1
  - No doc comments on private functions
  - Do NOT add excessive documentation — one line per function

  **Recommended Agent Profile**:
  - **Category**: `quick`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 3 (with Tasks 7, 8, 10)
  - **Blocks**: None
  - **Blocked By**: Task 4

  **References**:
  - `src/report/query.rs` (after task 4) — New submodule needing `//!` header
  - `src/report/display.rs` (after task 4) — New submodule needing `//!` header
  - `src/report/export.rs` (after task 4) — New submodule needing `//!` header
  - `src/watcher.rs` — `pub fn watch()` needs doc comment
  - `src/db.rs` — `pub fn init_db()`, `pub fn insert_event()` need doc comments

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: All pub fn have doc comments
    Tool: Bash
    Steps:
      1. For each .rs file, check that every `pub fn` line has a `///` comment on the preceding line
      2. Run `cargo doc 2>&1`
      3. Assert no "missing documentation" warnings (if warn enabled)
    Expected Result: Complete public API documentation
    Evidence: .sisyphus/evidence/task-9-docs.txt
  ```

  **Commit**: YES
  - Message: `docs: add doc comments to all public functions and new modules`
  - Files: `src/*.rs`, `src/report/*.rs`
  - Pre-commit: `cargo doc`

- [x] 10. Add Unit Tests for Report Query Functions

  **What to do**:
  - Add `#[cfg(test)] mod tests` sections in the appropriate report submodules (from task 4)
  - Use in-memory SQLite (`Connection::open_in_memory()`) for test databases
  - Target 10-15 new tests covering pure logic functions:
    - `query_today()` — empty DB, single event, multiple apps
    - `query_metrics_range()` — single day, multi-day range
    - `classify_gap()` — sleep vs break classification
    - `group_apps()` — app grouping and sorting
    - `compute_consistency_score()` — scoring edge cases
    - `query_report_range()` — full report generation
  - Set up test helper to create DB schema + insert test events
  - DO NOT test IPC, file I/O, TUI rendering, or email

  **Must NOT do**:
  - Do NOT add mocking libraries (no mockall dependency)
  - Do NOT test external integrations (Niri IPC, D-Bus, libinput)
  - Do NOT test TUI rendering
  - Do NOT create a separate test binary — use `#[cfg(test)]` inline

  **Recommended Agent Profile**:
  - **Category**: `deep`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 3 (with Tasks 7, 8, 9)
  - **Blocks**: Task 11
  - **Blocked By**: Task 4

  **References**:
  - `src/config.rs:1170-1313` — Existing test module pattern to follow
  - `src/db.rs:init_db()` — Schema creation function to use in test setup
  - `src/db.rs:insert_event()` — Insert function for test data
  - `src/report/query.rs` (after task 4) — Functions to test
  - `src/report/types.rs` — Data structures returned by query functions

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: New tests exist and pass
    Tool: Bash
    Steps:
      1. Run `cargo test 2>&1`
      2. Assert output contains "test result: ok"
      3. Count total tests — assert ≥ 22 (was 12)
      4. Run `grep -rc '#\[test\]' src/report/ | paste -sd+ | bc`
      5. Assert ≥ 10 new test functions
    Expected Result: Comprehensive test coverage for report queries
    Evidence: .sisyphus/evidence/task-10-tests.txt

  Scenario: Tests use in-memory SQLite
    Tool: Bash
    Steps:
      1. Run `grep -rn 'open_in_memory\|:memory:' src/report/`
      2. Assert matches found (tests don't create files)
    Expected Result: Tests are fast and leave no artifacts
    Evidence: .sisyphus/evidence/task-10-in-memory.txt
  ```

  **Commit**: YES
  - Message: `test: add unit tests for report query functions`
  - Files: `src/report/query.rs` or `src/report/tests.rs`
  - Pre-commit: `cargo test`

- [x] 11. Add CI/CD Workflow

  **What to do**:
  - Create `.github/workflows/ci.yml` with a single job:
    - Trigger: push to master, pull requests
    - Steps: checkout, install stable Rust (via dtolnay/rust-toolchain), cargo build, cargo clippy -- -D warnings, cargo test
  - Handle `niri-ipc` path dependency (`path = "../niri/niri-ipc"`):
    - Option A: Clone niri repo alongside in CI
    - Option B: Use `[patch]` section to override with git source in CI
    - Choose the simpler option that works
  - Handle system dependencies: `libinput-dev`, `libudev-dev` (or equivalent) needed for compilation
  - NO MSRV matrix, NO coverage reporting, NO release automation in v1

  **Must NOT do**:
  - Do NOT add multi-version Rust matrix
  - Do NOT add coverage tools (tarpaulin, llvm-cov)
  - Do NOT add release/publish automation
  - Do NOT add nightly-only checks

  **Recommended Agent Profile**:
  - **Category**: `quick`
  - **Skills**: [`rust-style`]

  **Parallelization**:
  - **Can Run In Parallel**: NO
  - **Parallel Group**: Wave 4 (solo)
  - **Blocks**: None
  - **Blocked By**: Tasks 10, 2

  **References**:
  - `Cargo.toml:40` — `niri-ipc = { path = "../niri/niri-ipc" }` — path dependency that breaks CI
  - `Cargo.toml:28` — `input = "0.9"` — needs libinput system library
  - `Cargo.toml:38` — `rusqlite` with bundled feature — compiles SQLite from source (no system dep needed)

  **Acceptance Criteria**:

  **QA Scenarios**:
  ```
  Scenario: CI workflow file exists and is valid
    Tool: Bash
    Steps:
      1. Run `cat .github/workflows/ci.yml`
      2. Assert file exists
      3. Assert contains `cargo build`
      4. Assert contains `cargo clippy`
      5. Assert contains `cargo test`
      6. Assert contains trigger on push/PR
    Expected Result: Valid CI workflow
    Evidence: .sisyphus/evidence/task-11-ci.txt
  ```

  **Commit**: YES
  - Message: `ci: add GitHub Actions build, clippy, and test workflow`
  - Files: `.github/workflows/ci.yml`
  - Pre-commit: YAML validation

---

## Final Verification Wave

> 4 review agents run in PARALLEL. ALL must APPROVE. Present consolidated results to user and get explicit "okay" before completing.
>
> **Do NOT auto-proceed after verification. Wait for user's explicit approval.**

- [x] F1. **Plan Compliance Audit** — `oracle`
  Read the plan end-to-end. For each "Must Have": verify implementation exists (read file, run command). For each "Must NOT Have": search codebase for forbidden patterns — reject with file:line if found. Check evidence files exist in .sisyphus/evidence/. Compare deliverables against plan.
  Output: `Must Have [N/N] | Must NOT Have [N/N] | Tasks [N/N] | VERDICT: APPROVE/REJECT`

- [x] F2. **Code Quality Review** — `unspecified-high`
  Run `cargo build --release`, `cargo clippy -- -D warnings`, `cargo test`. Review all changed files for: `as any`, empty catches, println in non-report code, commented-out code, unused imports. Check AI slop: excessive comments, over-abstraction, generic names.
  Output: `Build [PASS/FAIL] | Clippy [PASS/FAIL] | Tests [N pass/N fail] | Files [N clean/N issues] | VERDICT`

- [x] F3. **Real Manual QA** — `unspecified-high`
  Start from clean state. Run `cargo run -- today`, `cargo run -- metrics --today`, `cargo run -- timeline --today`, `cargo run -- report --today`. Verify output looks correct. Run `cargo run -- export --today --format csv` and check output. Save evidence.
  Output: `Scenarios [N/N pass] | Integration [N/N] | VERDICT`

- [x] F4. **Scope Fidelity Check** — `deep`
  For each task: read "What to do", read actual diff (git log/diff). Verify 1:1 — everything in spec was built, nothing beyond spec was built. Check "Must NOT do" compliance. Detect cross-task contamination. Flag unaccounted changes.
  Output: `Tasks [N/N compliant] | Contamination [CLEAN/N issues] | Unaccounted [CLEAN/N files] | VERDICT`

---

## Commit Strategy

| Task | Message                                            | Files                                       | Pre-commit              |
| ---- | -------------------------------------------------- | ------------------------------------------- | ----------------------- |
| 1    | `build: add release profile optimizations`         | `Cargo.toml`                                | `cargo build --release` |
| 2    | `deps: update all dependencies + toml 0.8→1.x`    | `Cargo.toml`, `Cargo.lock`                  | `cargo build && cargo test` |
| 3    | `refactor: extract magic numbers to named constants` | `src/report/mod.rs`, `src/input.rs`, `src/watcher.rs` | `cargo build && cargo test` |
| 4    | `refactor: split report/mod.rs into submodules`    | `src/report/*.rs`                           | `cargo build && cargo test` |
| 5    | `refactor: extract watcher watch() into focused functions` | `src/watcher.rs`                     | `cargo build && cargo test` |
| 6    | `feat: migrate diagnostic logging to tracing`      | `Cargo.toml`, `src/main.rs`, `src/watcher.rs`, `src/input.rs`, `src/logind.rs`, `src/agent_activity.rs` | `cargo build && cargo test` |
| 7    | `perf: eliminate unnecessary .clone() calls`       | `src/report/*.rs`, `src/tui.rs`             | `cargo build && cargo test` |
| 8    | `refactor: expand error enum with specific variants` | `src/error.rs`, call sites                 | `cargo build && cargo test` |
| 9    | `docs: add doc comments to public functions`       | `src/*.rs`                                  | `cargo doc`             |
| 10   | `test: add unit tests for report query functions`  | `src/report/tests.rs` or `src/report/*.rs`  | `cargo test`            |
| 11   | `ci: add GitHub Actions workflow`                  | `.github/workflows/ci.yml`                  | YAML lint               |

---

## Success Criteria

### Verification Commands
```bash
cargo build --release    # Expected: success, zero warnings
cargo clippy -- -D warnings  # Expected: success, zero warnings
cargo test               # Expected: ≥21 tests, all pass
ls -la target/release/niri-activity-rs  # Expected: binary exists
grep -c 'eprintln!' src/*.rs src/**/*.rs  # Expected: 0
wc -l src/report/mod.rs  # Expected: < 150 lines
```

### Final Checklist
- [ ] All "Must Have" present
- [ ] All "Must NOT Have" absent
- [ ] All tests pass
- [ ] Binary compiles and runs
