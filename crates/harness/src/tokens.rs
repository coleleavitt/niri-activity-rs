use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::detect;
use crate::harness::Harness;

/// Tokens an agent has consumed and produced.
///
/// A rising `output` between two reads is direct evidence that a model is
/// generating right now, which a file timestamp cannot distinguish from an
/// unrelated touch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Non-cached tokens supplied to the model.
    pub input: u64,
    /// Generated output excluding reasoning reported separately by the
    /// provider.
    pub output: u64,
    /// Generated reasoning tokens reported separately by the provider.
    pub reasoning: u64,
    /// Input tokens read from a provider cache.
    pub cache_read: u64,
    /// Input tokens written to a provider cache.
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
    opencode_usage_paths(
        detect::resolve("~/.local/share/opencode/opencode*.db"),
        window_ms,
    )
}

fn opencode_usage_paths(
    paths: impl IntoIterator<Item = PathBuf>,
    window_ms: i64,
) -> Option<TokenUsage> {
    let mut total = TokenUsage::default();
    let mut queried = false;
    for path in paths {
        if !path.exists() {
            continue;
        }
        let Ok(conn) = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let _ = conn.busy_timeout(std::time::Duration::ZERO);
        let Ok(usage) = conn.query_row(OPENCODE_STEP_FINISH, [window_ms], |row| {
            Ok(TokenUsage {
                input: row.get::<_, i64>(0)?.max(0) as u64,
                output: row.get::<_, i64>(1)?.max(0) as u64,
                reasoning: row.get::<_, i64>(2)?.max(0) as u64,
                cache_read: row.get::<_, i64>(3)?.max(0) as u64,
                cache_write: row.get::<_, i64>(4)?.max(0) as u64,
            })
        }) else {
            continue;
        };
        queried = true;
        total = total.saturating_add(usage);
    }
    queried.then_some(total)
}

/// Maximum bytes read from each recent rollout. This bounds malformed or
/// unusually large files while covering many dense turns.
const ROLLOUT_TAIL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECENT_ROLLOUTS: usize = 256;

fn codex_usage(window_ms: i64) -> Option<TokenUsage> {
    let dir = detect::expand_tilde("~/.codex/sessions")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let window_secs = window_ms.max(0).saturating_add(999) / 1000;
    let since = now.saturating_sub(window_secs);
    let mut paths = recent_files(&dir, since);
    paths.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    paths.truncate(MAX_RECENT_ROLLOUTS);

    let mut total = TokenUsage::default();
    let mut found = false;
    for (_, path) in paths {
        let Some((tail, truncated)) = read_tail(&path, ROLLOUT_TAIL_BYTES) else {
            continue;
        };
        if let Some(usage) = parse_codex_tail(&tail, truncated, since, now) {
            found = true;
            total = total.saturating_add(usage);
        }
    }
    found.then_some(total)
}

fn recent_files(dir: &Path, since: i64) -> Vec<(SystemTime, PathBuf)> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
                    continue;
                };
                let modified_secs = modified
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|duration| i64::try_from(duration.as_secs()).ok());
                if modified_secs.is_some_and(|seconds| seconds >= since) {
                    files.push((modified, entry.path()));
                }
            }
        }
    }
    files
}

fn read_tail(path: &Path, bytes: u64) -> Option<(String, bool)> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let truncated = len > bytes;
    file.seek(SeekFrom::Start(len.saturating_sub(bytes))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some((String::from_utf8_lossy(&buf).into_owned(), truncated))
}

/// Sum complete in-window Codex `last_token_usage` events.
fn parse_codex_tail(tail: &str, truncated: bool, since: i64, until: i64) -> Option<TokenUsage> {
    let mut text = tail;
    if truncated {
        let (first, rest) = text.split_once('\n')?;
        // The byte boundary can land either within a record or exactly at its
        // start. Keep a complete first record; discard only an invalid prefix.
        if serde_json::from_str::<serde_json::Value>(first).is_err() {
            text = rest;
        }
    }
    let mut total = TokenUsage::default();
    let mut found = false;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let timestamp = value
            .get("timestamp")
            .and_then(|value| value.as_str())
            .and_then(crate::history::parse_rfc3339);
        if !timestamp.is_some_and(|seconds| seconds >= since && seconds <= until) {
            continue;
        }
        let payload = value.get("payload");
        if payload
            .and_then(|value| value.get("type"))
            .and_then(|value| value.as_str())
            != Some("token_count")
        {
            continue;
        }
        let Some(usage) = payload
            .and_then(|value| value.get("info"))
            .and_then(|value| value.get("last_token_usage"))
        else {
            continue;
        };
        let raw_output = json_value_u64(usage, "output_tokens");
        let reasoning = json_value_u64(usage, "reasoning_output_tokens");
        total = total.saturating_add(TokenUsage {
            input: json_value_u64(usage, "input_tokens"),
            // Codex includes reasoning in output_tokens. Store visible output
            // separately so TokenUsage::generated does not double count it.
            output: raw_output.saturating_sub(reasoning),
            reasoning,
            cache_read: json_value_u64(usage, "cached_input_tokens"),
            cache_write: 0,
        });
        found = true;
    }
    found.then_some(total)
}

fn json_value_u64(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_line(timestamp: &str, input: u64, output: u64, reasoning: u64) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input},"output_tokens":{output},"reasoning_output_tokens":{reasoning},"cached_input_tokens":7}}}}}}}}"#
        )
    }

    #[test]
    fn generated_excludes_cached_reads() {
        let usage = TokenUsage {
            input: 2,
            output: 260,
            reasoning: 40,
            cache_read: 581_113,
            cache_write: 282,
        };
        assert_eq!(usage.generated(), 300);
        assert_eq!(usage.total(), 581_697);
    }

    #[test]
    fn sums_in_window_events_and_normalizes_reasoning() {
        let tail = format!(
            "{}\n{}\n{}\n",
            codex_line("2026-01-01T00:00:00Z", 1, 10, 4),
            codex_line("2026-01-01T00:01:00Z", 2, 20, 5),
            codex_line("2025-12-31T23:00:00Z", 100, 100, 100)
        );
        let usage = parse_codex_tail(&tail, false, 1_767_225_600, 1_767_225_700).expect("usage");
        assert_eq!(usage.input, 3);
        assert_eq!(usage.output, 21);
        assert_eq!(usage.reasoning, 9);
        assert_eq!(usage.generated(), 30);
        assert_eq!(usage.cache_read, 14);
    }

    #[test]
    fn old_token_followed_by_recent_non_token_is_ignored() {
        let tail = format!(
            "{}\n{{\"timestamp\":\"2026-01-01T00:01:00Z\",\"payload\":{{\"type\":\"other\"}}}}\n",
            codex_line("2025-12-31T23:00:00Z", 1, 2, 0)
        );
        assert_eq!(
            parse_codex_tail(&tail, false, 1_767_225_600, 1_767_225_700),
            None
        );
    }

    #[test]
    fn discards_partial_first_and_last_lines() {
        let middle = codex_line("2026-01-01T00:00:00Z", 1, 5, 2);
        let tail = format!("partial first\n{middle}\npartial last");
        let usage =
            parse_codex_tail(&tail, true, 1_767_225_600, 1_767_225_700).expect("middle event");
        assert_eq!(usage.generated(), 5);
    }

    #[test]
    fn accepts_a_complete_final_record_without_a_newline() {
        let tail = codex_line("2026-01-01T00:00:00Z", 1, 5, 2);
        let usage = parse_codex_tail(&tail, false, 1_767_225_600, 1_767_225_700)
            .expect("complete final event");
        assert_eq!(usage.generated(), 5);
    }

    #[test]
    fn keeps_a_complete_first_record_at_a_truncated_tail_boundary() {
        let first = codex_line("2026-01-01T00:00:00Z", 1, 5, 2);
        let second = codex_line("2026-01-01T00:01:00Z", 2, 7, 3);
        let tail = format!("{first}\n{second}");
        let usage = parse_codex_tail(&tail, true, 1_767_225_600, 1_767_225_700)
            .expect("both complete events");
        assert_eq!(usage.generated(), 12);
    }

    #[test]
    fn malformed_reasoning_larger_than_output_saturates() {
        let tail = format!("{}\n", codex_line("2026-01-01T00:00:00Z", 1, 3, 9));
        let usage = parse_codex_tail(&tail, false, 1_767_225_600, 1_767_225_700).expect("usage");
        assert_eq!(usage.output, 0);
        assert_eq!(usage.reasoning, 9);
    }

    fn create_opencode(path: &Path, input: i64, output: i64) {
        let conn = rusqlite::Connection::open(path).expect("open");
        conn.execute("CREATE TABLE part (time_updated INTEGER, data TEXT)", [])
            .expect("schema");
        let data = format!(
            r#"{{"type":"step-finish","tokens":{{"input":{input},"output":{output},"reasoning":0,"cache":{{"read":0,"write":0}}}}}}"#
        );
        conn.execute(
            "INSERT INTO part VALUES (strftime('%s','now') * 1000, ?1)",
            [data],
        )
        .expect("insert");
    }

    #[test]
    fn opencode_aggregates_every_matching_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("one.db");
        let second = dir.path().join("two.db");
        create_opencode(&first, 2, 3);
        create_opencode(&second, 5, 7);
        let usage = opencode_usage_paths([first, second], 60_000).expect("queried");
        assert_eq!(usage.input, 7);
        assert_eq!(usage.output, 10);
    }

    #[test]
    fn only_some_harnesses_report_tokens() {
        assert!(Harness::OpenCode.token_source().is_some());
        assert!(Harness::Codex.token_source().is_some());
        assert!(Harness::ClaudeCode.token_source().is_none());
    }

    #[test]
    fn reading_usage_never_panics() {
        for h in Harness::ALL {
            let _ = recent_usage(*h, 60_000);
        }
        let _ = recent_usage_all(60_000);
    }
}
