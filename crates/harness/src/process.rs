use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use crate::harness::Harness;

#[cfg(unix)]
fn is_current_user(entry_uid: u32, current_uid: u32) -> bool {
    entry_uid == current_uid
}

#[cfg(unix)]
fn trace_not_before_process(
    trace_start_secs: i64,
    trace_start_nanos: u32,
    process_birth_secs: i64,
    process_birth_nanos: i64,
) -> bool {
    (trace_start_secs, i64::from(trace_start_nanos)) >= (process_birth_secs, process_birth_nanos)
}

/// Process state character from the contents of `/proc/<pid>/stat`.
///
/// The command name sits in parentheses and may itself contain spaces or
/// parentheses, so the state is the first field after the *final* `)`.
#[cfg(unix)]
fn stat_state(stat: &str) -> Option<char> {
    let (_, after_comm) = stat.rsplit_once(')')?;
    after_comm.split_whitespace().next()?.chars().next()
}

/// Whether a state character describes a process that can still do work.
///
/// `comm` stays readable while a dead process waits to be reaped, so a zombie
/// answers every identity check exactly as the live process it used to be. An
/// `agent.prompt` span that died unclosed would then read as open forever and
/// credit agent time to a process that cannot write another byte.
#[cfg(unix)]
fn is_live_state(state: char) -> bool {
    !matches!(state, 'Z' | 'X' | 'x')
}

/// Fail closed: a process whose state cannot be read is not evidence of work.
#[cfg(unix)]
fn pid_is_live(proc_dir: &std::path::Path) -> bool {
    fs::read_to_string(proc_dir.join("stat"))
        .ok()
        .and_then(|stat| stat_state(&stat))
        .is_some_and(is_live_state)
}

#[cfg(unix)]
pub(crate) fn pid_matches_harness_since(
    pid: u64,
    harness: Harness,
    trace_start_secs: i64,
    trace_start_nanos: u32,
) -> bool {
    let path = std::path::PathBuf::from("/proc").join(pid.to_string());
    let Ok(current_uid) = fs::metadata("/proc/self").map(|metadata| metadata.uid()) else {
        return false;
    };
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !is_current_user(metadata.uid(), current_uid) {
        return false;
    }
    // procfs exposes process-directory ctime as the process birth instant.
    // Compare at the filesystem's available precision. A trace in the same
    // whole second but earlier nanoseconds belongs to the previous PID owner.
    if !trace_not_before_process(
        trace_start_secs,
        trace_start_nanos,
        metadata.ctime(),
        metadata.ctime_nsec(),
    ) {
        return false;
    }
    if !pid_is_live(&path) {
        return false;
    }
    fs::read_to_string(path.join("comm")).is_ok_and(|comm| comm.trim() == harness.process_name())
}

#[cfg(not(unix))]
pub(crate) fn pid_matches_harness_since(
    _pid: u64,
    _harness: Harness,
    _trace_start_secs: i64,
    _trace_start_nanos: u32,
) -> bool {
    false
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
        // A dead process waiting to be reaped keeps its name, and "running"
        // must mean a process that can still act.
        #[cfg(unix)]
        if !pid_is_live(&entry.path()) {
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

    #[cfg(unix)]
    #[test]
    fn process_birth_comparison_uses_subsecond_precision() {
        assert!(!trace_not_before_process(
            100,
            400_000_000,
            100,
            500_000_000
        ));
        assert!(trace_not_before_process(100, 500_000_000, 100, 500_000_000));
        assert!(trace_not_before_process(100, 600_000_000, 100, 500_000_000));
    }

    #[cfg(unix)]
    #[test]
    fn the_state_field_survives_a_command_name_with_punctuation() {
        // /proc/<pid>/stat wraps comm in parentheses without escaping, so the
        // state is the first field after the last one.
        assert_eq!(stat_state("42 (prime-agent) S 1 42 42 0"), Some('S'));
        assert_eq!(stat_state("42 (odd (name) x) Z 1 42 42 0"), Some('Z'));
        assert_eq!(stat_state("not a stat line"), None);
        assert!(is_live_state('S'));
        assert!(is_live_state('R'));
        assert!(!is_live_state('Z'));
        assert!(!is_live_state('X'));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_process_state_is_not_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!pid_is_live(dir.path()));
    }

    #[cfg(unix)]
    #[test]
    fn a_zombie_process_is_not_live() {
        // An exited-but-unreaped agent keeps a readable comm, which used to be
        // enough to hold an unclosed prompt span open forever.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let proc_dir = std::path::PathBuf::from("/proc").join(child.id().to_string());

        let mut state = None;
        for _ in 0..200 {
            state = fs::read_to_string(proc_dir.join("stat"))
                .ok()
                .and_then(|stat| stat_state(&stat));
            if state == Some('Z') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(state, Some('Z'), "child never became a zombie");
        assert!(
            fs::read_to_string(proc_dir.join("comm")).is_ok(),
            "a zombie still answers identity checks"
        );
        assert!(!pid_is_live(&proc_dir));

        child.wait().expect("reap");
    }

    #[cfg(unix)]
    #[test]
    fn a_zombie_does_not_keep_a_prompt_span_open() {
        let Some(source) = ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .find(|path| std::path::Path::new(path).exists())
        else {
            return;
        };
        // comm comes from the name the binary was executed under, so a symlink
        // named after a harness reproduces the identity a dead agent leaves
        // behind. A symlink rather than a copy: writing an executable while
        // sibling tests fork can make the exec fail with ETXTBSY.
        let dir = tempfile::tempdir().expect("tempdir");
        let impostor = dir.path().join(Harness::Jcode.process_name());
        std::os::unix::fs::symlink(source, &impostor).expect("symlink");
        let mut child = std::process::Command::new(&impostor)
            .spawn()
            .expect("spawn");
        let pid = u64::from(child.id());
        let proc_dir = std::path::PathBuf::from("/proc").join(pid.to_string());

        for _ in 0..200 {
            let state = fs::read_to_string(proc_dir.join("stat"))
                .ok()
                .and_then(|stat| stat_state(&stat));
            if state == Some('Z') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // A span recorded after the process was born, by a PID whose comm still
        // matches: every surviving check says "live agent" except its state.
        let birth = proc_dir.metadata().expect("proc metadata").ctime();
        let matched = pid_matches_harness_since(pid, Harness::Jcode, birth.saturating_add(1), 0);

        child.wait().expect("reap");
        assert!(!matched, "a dead agent cannot still be writing a prompt");
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
