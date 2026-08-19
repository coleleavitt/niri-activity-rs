use std::fs;

use crate::harness::Harness;

/// Harnesses with a matching process currently running.
///
/// Presence alone does not mean an agent is working — a CLI left open at a
/// prompt still shows up — so callers should pair this with a file-activity
/// check or a recent-input requirement.
pub fn running() -> Vec<Harness> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        let comm = comm.trim();
        for harness in Harness::ALL {
            // /proc/<pid>/comm is truncated to 15 bytes by the kernel, so a
            // longer command name would never compare equal.
            if comm == harness.process_name() && !found.contains(harness) {
                found.push(*harness);
            }
        }
    }
    found.sort_unstable();
    found
}

pub fn is_running(harness: Harness) -> bool {
    running().contains(&harness)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanning_proc_never_panics() {
        let _ = running();
    }

    #[test]
    fn this_test_binary_is_not_mistaken_for_a_harness() {
        // The test runner's own comm must not collide with a harness name, or
        // every check would report active.
        let comm = fs::read_to_string("/proc/self/comm").unwrap_or_default();
        let comm = comm.trim();
        assert!(
            !Harness::ALL.iter().any(|h| h.process_name() == comm),
            "test binary comm {comm:?} collides with a harness name"
        );
    }
}
