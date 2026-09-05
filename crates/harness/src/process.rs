use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use crate::harness::Harness;

#[cfg(unix)]
fn is_current_user(entry_uid: u32, current_uid: u32) -> bool {
    entry_uid == current_uid
}

/// Harnesses with a matching process currently running.
///
/// Presence alone does not mean an agent is working — a CLI left open at a
/// prompt still shows up — so callers should pair this with a file-activity
/// check or a recent-input requirement.
pub fn running() -> Vec<Harness> {
    #[cfg(not(unix))]
    return Vec::new();

    #[cfg(unix)]
    let Ok(current_uid) = fs::metadata("/proc/self").map(|metadata| metadata.uid()) else {
        return Vec::new();
    };
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
        #[cfg(unix)]
        if !entry
            .metadata()
            .map(|metadata| is_current_user(metadata.uid(), current_uid))
            .unwrap_or(false)
        {
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

    #[cfg(unix)]
    #[test]
    fn ownership_filter_is_fail_closed() {
        assert!(is_current_user(1000, 1000));
        assert!(!is_current_user(1001, 1000));
    }

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
