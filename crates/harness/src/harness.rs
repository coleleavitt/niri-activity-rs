use std::fmt;

/// An AI coding agent installed on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    OpenCode,
    ClaudeCode,
    Codex,
    Grok,
    GajaeCode,
    Jfc,
    Jcode,
    Amp,
    Cursor,
    Kimi,
    Gemini,
    Qwen,
}

/// Where an agent leaves evidence that it is working.
///
/// Agents write continuously while streaming a response, so a recent write is
/// a proxy for "the model is producing output right now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A SQLite database, matched literally or by a single `*` in the file
    /// name.
    Database(&'static str),
    /// A directory of append-only logs. Appends do not update the directory's
    /// own mtime, so these are checked per file.
    LogDir(&'static str),
    /// A single append-only log file.
    LogFile(&'static str),
}

impl Signal {
    pub fn path(self) -> &'static str {
        match self {
            Signal::Database(p) | Signal::LogDir(p) | Signal::LogFile(p) => p,
        }
    }
}

impl Harness {
    pub const ALL: &'static [Harness] = &[
        Harness::OpenCode,
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::Grok,
        Harness::GajaeCode,
        Harness::Jfc,
        Harness::Jcode,
        Harness::Amp,
        Harness::Cursor,
        Harness::Kimi,
        Harness::Gemini,
        Harness::Qwen,
    ];

    /// Command name as it appears in `/proc/<pid>/comm`.
    ///
    /// Linux truncates that file to 15 characters, so every name here must
    /// already be short enough to compare directly.
    pub fn process_name(self) -> &'static str {
        match self {
            Harness::OpenCode => "opencode",
            Harness::ClaudeCode => "claude",
            Harness::Codex => "codex",
            Harness::Grok => "grok",
            Harness::GajaeCode => "gjc",
            Harness::Jfc => "jfc",
            Harness::Jcode => "jcode",
            Harness::Amp => "amp",
            Harness::Cursor => "cursor",
            Harness::Kimi => "kimi",
            Harness::Gemini => "gemini",
            Harness::Qwen => "qwen",
        }
    }

    /// Paths that change while this agent is working, cheapest first.
    ///
    /// Ordering matters: detection stops at the first hit, so a single-file
    /// check belongs ahead of a directory scan.
    pub fn signals(self) -> &'static [Signal] {
        match self {
            Harness::OpenCode => &[
                Signal::Database("~/.local/share/opencode/opencode*.db"),
                Signal::LogDir("~/.local/share/opencode/storage/session"),
            ],
            Harness::ClaudeCode => &[
                Signal::LogDir("~/.claude/transcripts"),
                Signal::LogDir("~/.claude/projects"),
            ],
            Harness::Codex => &[
                Signal::Database("~/.codex/state_5.sqlite"),
                Signal::LogFile("~/.codex/history.jsonl"),
                Signal::LogDir("~/.codex/sessions"),
            ],
            Harness::Grok => &[
                Signal::Database("~/.grok/sessions/session_search.sqlite"),
                Signal::LogFile("~/.grok/logs/unified.jsonl"),
                Signal::LogDir("~/.grok/sessions"),
            ],
            Harness::GajaeCode => &[
                Signal::Database("~/.gjc/agent/agent.db"),
                Signal::Database("~/.gjc/agent/history.db"),
                Signal::LogDir("~/.gjc/agent/sessions"),
            ],
            Harness::Jfc => &[
                // Foreground and daemon logs beneath this root are appended during
                // streaming and tool execution. The old ~/.jfc/audit path was
                // project-relative in JFC and wrong for most working directories.
                Signal::LogDir("~/.config/jfc/logs"),
                // Keep the daemon subtree as an independent signal. A bounded
                // root scan may truncate before reaching it, while last_activity
                // must still observe recent background-agent writes.
                Signal::LogDir("~/.config/jfc/logs/daemon/agents"),
            ],
            Harness::Jcode => &[Signal::LogDir("~/.jcode/logs")],
            Harness::Amp => &[Signal::LogDir("~/.amp/file-changes")],
            Harness::Cursor => &[Signal::LogDir("~/.cursor/chats")],
            Harness::Kimi => &[Signal::LogDir("~/.kimi/sessions")],
            Harness::Gemini => &[Signal::LogDir("~/.gemini/history")],
            Harness::Qwen => &[Signal::LogDir("~/.qwen/sessions")],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Harness::OpenCode => "OpenCode",
            Harness::ClaudeCode => "Claude Code",
            Harness::Codex => "Codex",
            Harness::Grok => "Grok",
            Harness::GajaeCode => "Gajae Code",
            Harness::Jfc => "jfc",
            Harness::Jcode => "jcode",
            Harness::Amp => "Amp",
            Harness::Cursor => "Cursor",
            Harness::Kimi => "Kimi",
            Harness::Gemini => "Gemini",
            Harness::Qwen => "Qwen",
        }
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_names_fit_the_kernel_comm_limit() {
        for h in Harness::ALL {
            assert!(
                h.process_name().len() <= 15,
                "{h} would be truncated in /proc/<pid>/comm"
            );
        }
    }

    #[test]
    fn every_harness_declares_a_signal() {
        for h in Harness::ALL {
            assert!(!h.signals().is_empty(), "{h} has no signal to check");
        }
    }

    #[test]
    fn claude_checks_transcripts_and_projects() {
        let signals = Harness::ClaudeCode.signals();
        assert!(signals.contains(&Signal::LogDir("~/.claude/transcripts")));
        assert!(signals.contains(&Signal::LogDir("~/.claude/projects")));
    }

    #[test]
    fn jfc_keeps_daemon_agents_as_a_dedicated_signal() {
        let signals = Harness::Jfc.signals();
        assert!(signals.contains(&Signal::LogDir("~/.config/jfc/logs")));
        assert!(signals.contains(&Signal::LogDir("~/.config/jfc/logs/daemon/agents")));
    }

    #[test]
    fn identities_are_unique() {
        let mut names: Vec<_> = Harness::ALL.iter().map(|h| h.process_name()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two harnesses share a process name");
    }
}
