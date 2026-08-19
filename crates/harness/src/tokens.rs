use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::detect;
use crate::harness::Harness;

/// Tokens an agent has consumed and produced.
///
/// A rising `output` between two reads is direct evidence that a model is
/// generating right now, which a file timestamp cannot distinguish from an
/// unrelated touch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenUsage {
    /// Tokens the model actually generated, excluding anything it merely read.
    ///
    /// Cache reads dwarf real output — a long session reads hundreds of
    /// thousands of cached tokens per turn — so totalling everything would
    /// swamp the generation signal.
    pub fn generated(self) -> u64 {
        self.output + self.reasoning
    }

    pub fn total(self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            reasoning: self.reasoning.saturating_add(other.reasoning),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
        }
    }
}

/// Where an agent records token counts, if it records them at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// OpenCode writes a `step-finish` part per model turn.
    OpenCodeParts,
    /// Codex appends a `token_count` event to the active rollout log.
    CodexRollout,
}

impl Harness {
    pub fn token_source(self) -> Option<TokenSource> {
        match self {
            Harness::OpenCode => Some(TokenSource::OpenCodeParts),
            Harness::Codex => Some(TokenSource::CodexRollout),
            _ => None,
        }
    }
}

/// Tokens recorded by `harness` in the last `window_ms` milliseconds.
///
/// `None` means the agent does not record tokens, or its store is unreadable.
pub fn recent_usage(harness: Harness, window_ms: i64) -> Option<TokenUsage> {
    let source = harness.token_source()?;

    // OpenCode's `part` table has no index on `time_updated`, so this query
    // full-scans a store that reaches tens of gigabytes. A store nobody wrote
    // to cannot hold new tokens, and that check costs one stat, so it gates
    // the scan: 361ms becomes 0.02ms whenever an agent is idle.
    let window = std::time::Duration::from_millis(u64::try_from(window_ms).unwrap_or(u64::MAX));
    if !crate::is_active(harness, window) {
        return None;
    }

    match source {
        TokenSource::OpenCodeParts => opencode_usage(window_ms),
        TokenSource::CodexRollout => codex_usage(window_ms),
    }
}

/// Sum token usage across every agent that reports it.
pub fn recent_usage_all(window_ms: i64) -> TokenUsage {
    Harness::ALL
        .iter()
        .filter_map(|h| recent_usage(*h, window_ms))
        .fold(TokenUsage::default(), TokenUsage::saturating_add)
}

const OPENCODE_STEP_FINISH: &str = "\
    SELECT COALESCE(SUM(json_extract(data,'$.tokens.input')),0), \
           COALESCE(SUM(json_extract(data,'$.tokens.output')),0), \
           COALESCE(SUM(json_extract(data,'$.tokens.reasoning')),0), \
           COALESCE(SUM(json_extract(data,'$.tokens.cache.read')),0), \
           COALESCE(SUM(json_extract(data,'$.tokens.cache.write')),0) \
    FROM part \
    WHERE time_updated > (strftime('%s','now') * 1000 - ?1) \
      AND json_extract(data,'$.type') = 'step-finish'";

fn opencode_usage(window_ms: i64) -> Option<TokenUsage> {
    let path = detect::resolve("~/.local/share/opencode/opencode*.db")
        .into_iter()
        .find(|p| p.exists())?;

    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // The agent writing this database holds it constantly; waiting on a lock
    // would stall the caller's poll loop for no benefit.
    let _ = conn.busy_timeout(std::time::Duration::ZERO);

    conn.query_row(OPENCODE_STEP_FINISH, [window_ms], |row| {
        Ok(TokenUsage {
            input: row.get::<_, i64>(0)?.max(0) as u64,
            output: row.get::<_, i64>(1)?.max(0) as u64,
            reasoning: row.get::<_, i64>(2)?.max(0) as u64,
            cache_read: row.get::<_, i64>(3)?.max(0) as u64,
            cache_write: row.get::<_, i64>(4)?.max(0) as u64,
        })
    })
    .ok()
}

/// Bytes to read from the end of a rollout log.
///
/// Rollouts grow to tens of megabytes, but a `token_count` event is emitted
/// every turn, so the newest one is always near the end.
const ROLLOUT_TAIL_BYTES: u64 = 256 * 1024;

fn codex_usage(window_ms: i64) -> Option<TokenUsage> {
    let dir = detect::expand_tilde("~/.codex/sessions")?;
    let path = newest_file(&dir)?;

    let age_ms = fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()?
        .elapsed()
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    if age_ms > window_ms {
        return None;
    }

    parse_codex_tail(&read_tail(&path, ROLLOUT_TAIL_BYTES)?)
}

fn read_tail(path: &Path, bytes: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(bytes))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Extract the last `last_token_usage` block from a rollout tail.
///
/// Codex reports a running total plus the usage of the most recent turn; the
/// per-turn figure is what indicates current work.
fn parse_codex_tail(tail: &str) -> Option<TokenUsage> {
    let start = tail.rfind("\"last_token_usage\"")?;
    let open = tail[start..].find('{')? + start;
    let close = tail[open..].find('}')? + open;
    let block = &tail[open..=close];

    Some(TokenUsage {
        input: json_u64(block, "input_tokens"),
        output: json_u64(block, "output_tokens"),
        reasoning: json_u64(block, "reasoning_output_tokens"),
        cache_read: json_u64(block, "cached_input_tokens"),
        cache_write: 0,
    })
}

fn json_u64(text: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\":");
    let Some(start) = text.find(&needle) else {
        return 0;
    };
    text[start + needle.len()..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

/// Newest regular file anywhere beneath `dir`.
fn newest_file(dir: &Path) -> Option<std::path::PathBuf> {
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if let Ok(modified) = meta.modified() {
                if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                    best = Some((modified, entry.path()));
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_excludes_cached_reads() {
        let usage = TokenUsage {
            input: 2,
            output: 260,
            reasoning: 40,
            cache_read: 581_113,
            cache_write: 282,
        };
        assert_eq!(
            usage.generated(),
            300,
            "cache reads must not inflate output"
        );
        assert_eq!(usage.total(), 581_697);
    }

    #[test]
    fn parses_a_real_codex_token_event() {
        let line = r#"{"timestamp":"2026-08-13T01:53:00.126Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":9771202,"cached_input_tokens":9151616,"output_tokens":44346,"reasoning_output_tokens":11272,"total_tokens":9815548},"last_token_usage":{"input_tokens":174639,"cached_input_tokens":0,"output_tokens":75,"reasoning_output_tokens":45,"total_tokens":174714},"model_context_window":950000},"rate_limits":null}}"#;

        let usage = parse_codex_tail(line).expect("parses");
        // The per-turn block, not the session total that precedes it.
        assert_eq!(usage.input, 174_639);
        assert_eq!(usage.output, 75);
        assert_eq!(usage.reasoning, 45);
        assert_eq!(usage.generated(), 120);
    }

    #[test]
    fn takes_the_last_event_when_several_are_present() {
        let tail = r#"{"last_token_usage":{"output_tokens":11}}
{"last_token_usage":{"output_tokens":22}}"#;
        assert_eq!(parse_codex_tail(tail).expect("parses").output, 22);
    }

    #[test]
    fn missing_or_malformed_input_yields_no_usage() {
        assert_eq!(parse_codex_tail("no tokens here"), None);
        assert_eq!(json_u64("{\"output_tokens\":}", "output_tokens"), 0);
        assert_eq!(json_u64("{}", "output_tokens"), 0);
    }

    #[test]
    fn only_some_harnesses_report_tokens() {
        assert!(Harness::OpenCode.token_source().is_some());
        assert!(Harness::Codex.token_source().is_some());
        assert!(Harness::ClaudeCode.token_source().is_none());
        assert!(Harness::Grok.token_source().is_none());
    }

    #[test]
    fn reading_usage_never_panics() {
        for h in Harness::ALL {
            let _ = recent_usage(*h, 60_000);
        }
        let _ = recent_usage_all(60_000);
    }
}
