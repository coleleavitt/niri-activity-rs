# Decisions

## Key Architectural Decisions
- SINGLE libinput thread replaces 2 threads (input-poll + libinput-scroll)
- InputCounters struct groups all 18 atomic fields (13 AtomicU64 + 2 AtomicBool + heartbeat + last_keyboard_ms + last_meaningful_input_ms)
- Keep jiggler-scan thread (orthogonal, process scanning)
- Keep input-hotplug (inotify) thread through Task 5, remove in Task 6
- InputStats public API must remain identical (watcher.rs has 9 call sites)

## Scope Boundaries
- ONLY src/input.rs and Cargo.toml change
- watcher.rs, db.rs, config.rs are READ-ONLY
- No new features, no async, no gesture tracking

## Dependency Changes
- ADD rustix = { version = "1", features = ["event"] }
- REMOVE evdev = "0.13" (in Task 6)
- REMOVE inotify (in Task 6, if not used elsewhere)
