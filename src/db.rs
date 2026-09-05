use chrono::Utc;
use rusqlite::{Connection, params};

use crate::config::Config;
use crate::error::Error;
use crate::input::InputSnapshot;
use crate::watcher::WindowInfo;

pub struct SessionSnapshot<'a> {
    pub window: WindowInfo,
    pub config: &'a Config,
    pub focus_start: chrono::DateTime<Utc>,
    pub active_ms: i64,
    pub passive_ms: i64,
    pub idle_ms: i64,
    /// Milliseconds a coding agent was working during this session.
    ///
    /// Overlaps active/passive/idle rather than partitioning them — an agent
    /// runs while the user types, reads, or steps away.
    pub agent_ms: i64,
    pub input: InputSnapshot,
    pub jiggler_detected: bool,
    pub input_offsets: Vec<u32>,
    pub project: Option<String>,
    pub git_branch: Option<String>,
}

/// Initialize the database schema and pragmas for optimal performance.
pub fn init_db(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;",
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY,
            timestamp TEXT NOT NULL,
            app_id TEXT,
            title TEXT,
            category TEXT NOT NULL,
            active_ms INTEGER NOT NULL,
            idle_ms INTEGER NOT NULL,
            keystrokes INTEGER NOT NULL DEFAULT 0,
            mouse_clicks INTEGER NOT NULL DEFAULT 0,
            scroll_events INTEGER NOT NULL DEFAULT 0,
            mouse_distance INTEGER NOT NULL DEFAULT 0
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_timestamp ON events(timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_category ON events(category)",
        [],
    )?;
    Ok(())
}

/// Run all pending database migrations and apply category reclassifications.
pub fn run_migrations(conn: &mut Connection, config: &Config) -> Result<(), Error> {
    init_db(conn)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS migrations (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            applied_at TEXT NOT NULL
        )",
        [],
    )?;

    let applied: Vec<String> = {
        let mut stmt = conn.prepare("SELECT name FROM migrations")?;
        stmt.query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };

    if !applied.contains(&"001_fix_historical_categories".to_string())
        && config.can_reclassify_all()
    {
        let tx = conn.transaction()?;
        let mut updated = 0i64;
        for (app_id, category) in &config.categories {
            let count = tx.execute(
                "UPDATE events SET category = ?1 WHERE app_id = ?2 AND category != ?1",
                params![category.to_string(), app_id],
            )?;
            updated = updated.saturating_add(i64::try_from(count).unwrap_or(i64::MAX));
        }
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["001_fix_historical_categories", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        if updated > 0 {
            tracing::info!(
                "Migration 001: fixed {} events with stale categories",
                updated
            );
        }
    }

    if !applied.contains(&"002_apply_title_rules".to_string())
        && !config.title_rules.is_empty()
        && config.can_reclassify_all()
    {
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare("SELECT id, app_id, title FROM events")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut updated = 0i64;
        for (id, app_id, title) in &rows {
            let correct = config.classify(app_id, title);
            let count = tx.execute(
                "UPDATE events SET category = ?1 WHERE id = ?2 AND category != ?1",
                params![correct.to_string(), id],
            )?;
            updated = updated.saturating_add(i64::try_from(count).unwrap_or(i64::MAX));
        }

        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["002_apply_title_rules", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        if updated > 0 {
            tracing::info!(
                "Migration 002: reclassified {} events with title rules",
                updated
            );
        }
    }

    if !applied.contains(&"003_app_scoped_title_rules".to_string()) && config.can_reclassify_all() {
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare("SELECT id, app_id, title, category FROM events")?;
        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut updated = 0i64;
        for (id, app_id, title, old_category) in &rows {
            let correct = config.classify(app_id, title);
            if correct.to_string() != *old_category {
                tx.execute(
                    "UPDATE events SET category = ?1 WHERE id = ?2",
                    params![correct.to_string(), id],
                )?;
                updated = updated.saturating_add(1);
            }
        }

        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["003_app_scoped_title_rules", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        if updated > 0 {
            tracing::info!(
                "Migration 003: reclassified {} events with app-scoped title rules",
                updated
            );
        }
    }

    if !applied.contains(&"004_add_passive_and_jiggler".to_string()) {
        let tx = conn.transaction()?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN passive_ms INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN jiggler_detected INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["004_add_passive_and_jiggler", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!("Migration 004: added passive_ms and jiggler_detected columns");
    }

    if !applied.contains(&"005_add_input_offsets".to_string()) {
        let tx = conn.transaction()?;
        tx.execute("ALTER TABLE events ADD COLUMN input_offsets BLOB", [])?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["005_add_input_offsets", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!(
            "Migration 005: added input_offsets column for retroactive reclassification"
        );
    }

    if !applied.contains(&"007_add_sent_reports".to_string()) {
        let tx = conn.transaction()?;
        tx.execute(
            "CREATE TABLE IF NOT EXISTS sent_reports (
                id INTEGER PRIMARY KEY,
                period_type TEXT NOT NULL,
                period_key TEXT NOT NULL,
                sent_at TEXT NOT NULL,
                UNIQUE(period_type, period_key)
            )",
            [],
        )?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["007_add_sent_reports", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!("Migration 007: added sent_reports table for automated scheduling");
    }

    if !applied.contains(&"006_add_granular_input_metrics".to_string()) {
        let tx = conn.transaction()?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN backspace_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN modifier_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN left_clicks INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN right_clicks INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN middle_clicks INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN scroll_up INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN scroll_down INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "ALTER TABLE events ADD COLUMN scroll_horizontal INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["006_add_granular_input_metrics", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!("Migration 006: added granular input metrics columns");
    }

    if !applied.contains(&"008_add_project_column".to_string()) {
        let tx = conn.transaction()?;
        tx.execute("ALTER TABLE events ADD COLUMN project TEXT", [])?;
        tx.execute(
            "CREATE INDEX IF NOT EXISTS idx_project ON events(project)",
            [],
        )?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["008_add_project_column", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!("Migration 008: added project column and index");
    }

    if !applied.contains(&"009_add_git_branch_column".to_string()) {
        let tx = conn.transaction()?;
        tx.execute("ALTER TABLE events ADD COLUMN git_branch TEXT", [])?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["009_add_git_branch_column", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!("Migration 009: added git_branch column");
    }

    if !applied.contains(&"010_add_agent_ms_column".to_string()) {
        let tx = conn.transaction()?;
        // Rows written before this migration have no measurement, which is
        // not the same as measuring zero agent time; NULL keeps that
        // distinction so historical reports can exclude them.
        tx.execute("ALTER TABLE events ADD COLUMN agent_ms INTEGER", [])?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["010_add_agent_ms_column", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!("Migration 010: added agent_ms column");
    }

    if !applied.contains(&"011_index_pending_agent_ms".to_string()) {
        let tx = conn.transaction()?;
        // Partial rather than a plain index on agent_ms: only unmeasured rows
        // are ever looked up this way, so indexing the rest would cost space
        // and slow every insert for entries no query reads.
        tx.execute(
            "CREATE INDEX IF NOT EXISTS idx_agent_ms_pending
             ON events(timestamp) WHERE agent_ms IS NULL",
            [],
        )?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["011_index_pending_agent_ms", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!("Migration 011: indexed events pending agent measurement");
    }

    if !applied.contains(&"012_add_scheduled_report_jobs".to_string()) {
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TABLE scheduled_report_jobs (
                period_type TEXT NOT NULL,
                period_key TEXT NOT NULL,
                range_start TEXT NOT NULL,
                range_end TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'sent')),
                owner TEXT,
                claimed_at TEXT,
                lease_expires_at TEXT,
                attempt_count INTEGER NOT NULL DEFAULT 0,
                last_attempt_at TEXT,
                sent_at TEXT,
                last_error TEXT,
                PRIMARY KEY(period_type, period_key)
            );
            INSERT INTO scheduled_report_jobs (
                period_type, period_key, range_start, range_end, state, sent_at
            )
            SELECT period_type, period_key, period_key, period_key, 'sent', sent_at
            FROM sent_reports;",
        )?;
        tx.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["012_add_scheduled_report_jobs", Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        tracing::info!("Migration 012: added durable scheduled report jobs");
    }

    Ok(())
}

/// Maximum rows read and updated in one reclassification transaction.
const RECLASSIFY_BATCH_SIZE: usize = 10_000;

/// Reclassify all events in the database according to the current
/// configuration.
pub fn reclassify_all(conn: &mut Connection, config: &Config) -> Result<(), Error> {
    reclassify_all_in_batches(conn, config, RECLASSIFY_BATCH_SIZE)
}

fn reclassify_all_in_batches(
    conn: &mut Connection,
    config: &Config,
    batch_size: usize,
) -> Result<(), Error> {
    if !config.can_reclassify_all() {
        tracing::warn!("Skipping category reclassification because browser history is unavailable");
        return Ok(());
    }

    let batch_size = i64::try_from(batch_size.max(1)).unwrap_or(i64::MAX);
    let mut last_id: Option<i64> = None;
    let mut updated = 0i64;

    loop {
        let tx = conn.transaction()?;
        let rows: Vec<(i64, String, String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, app_id, title, category
                   FROM events
                  WHERE ?1 IS NULL OR id > ?1
                  ORDER BY id
                  LIMIT ?2",
            )?;
            stmt.query_map(params![last_id, batch_size], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let Some((page_last_id, _, _, _)) = rows.last() else {
            tx.commit()?;
            break;
        };
        last_id = Some(*page_last_id);

        for (id, app_id, title, old_category) in &rows {
            let correct = config.classify(app_id, title).to_string();
            if correct != *old_category {
                tx.execute(
                    "UPDATE events SET category = ?1 WHERE id = ?2",
                    params![correct, id],
                )?;
                updated = updated.saturating_add(1);
            }
        }
        tx.commit()?;
    }

    if updated > 0 {
        tracing::info!("Reclassified {} events to match current config", updated);
    }
    Ok(())
}

/// Fill `agent_ms` for events recorded before agent tracking existed.
///
/// Upper bound on rows one backfill pass will load/update, bounding memory and
/// write-lock duration. The heal loop advances across passes, so a large first
/// run still converges over several passes instead of one unbounded scan.
const MAX_AGENT_BACKFILL_ROWS: usize = 50_000;

/// Rows recent enough that the live watcher may still fill their agent_ms are
/// left alone by the healer, so heal and the daemon do not fight over them.
/// Only events older than this are considered "settled" and get a measured
/// value (including a measured 0), which is what lets heal converge.
const AGENT_HEAL_GRACE_SECS: i64 = 900; // 15 minutes

/// Reconstructs agent activity from harness logs and intersects it with each
/// session's own span, so a five-minute focus that overlapped two busy minutes
/// is credited two minutes rather than five.
///
/// Bounded to `MAX_AGENT_BACKFILL_ROWS` and ordered oldest-first so repeated
/// passes make forward progress. `before_secs` excludes rows too recent to
/// have settled (the live watcher may still measure them).
fn events_needing_agent_ms_after(
    conn: &Connection,
    since_secs: i64,
    before_secs: i64,
    after: Option<(&str, i64)>,
) -> Result<Vec<(i64, String, i64)>, Error> {
    let since_rfc3339 = chrono::DateTime::from_timestamp(since_secs, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339();
    let before_rfc3339 = chrono::DateTime::from_timestamp(before_secs, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339();
    let (after_timestamp, after_id) =
        after.map_or((None, 0), |(timestamp, id)| (Some(timestamp), id));
    let mut stmt = conn.prepare(
        "SELECT id, timestamp, active_ms + COALESCE(passive_ms,0) + idle_ms
           FROM events
          WHERE agent_ms IS NULL AND timestamp >= ?1 AND timestamp < ?2
            AND (?3 IS NULL OR timestamp > ?3 OR (timestamp = ?3 AND id > ?4))
          ORDER BY timestamp, id
          LIMIT ?5",
    )?;
    let rows = stmt
        .query_map(
            params![
                &since_rfc3339,
                &before_rfc3339,
                after_timestamp,
                after_id,
                i64::try_from(MAX_AGENT_BACKFILL_ROWS).unwrap_or(i64::MAX)
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
fn events_needing_agent_ms(
    conn: &Connection,
    since_secs: i64,
    before_secs: i64,
) -> Result<Vec<(i64, String, i64)>, Error> {
    events_needing_agent_ms_after(conn, since_secs, before_secs, None)
}

/// Measure any events the live watcher never recorded agent time for.
///
/// Scans from the oldest unmeasured event rather than a fixed window, so a
/// daemon that was down for a day and one down for a month both heal without
/// a configured lookback. Returns `Ok(0)` immediately when nothing is pending,
/// which the partial index makes effectively free.
///
/// Converges: settled rows (older than the grace window) always receive a
/// measured value — a measured 0 when no agent ran — so they stop being NULL
/// and are not rescanned on the next startup. Only rows inside the grace window
/// stay NULL, and those are few.
pub fn heal_missing_agent_ms(conn: &mut Connection) -> Result<i64, Error> {
    // Malformed timestamps remain unknown rather than being stamped measured
    // zero. Exclude them from the boundary so they cannot wedge valid rows.
    let oldest: Option<i64> = conn.query_row(
        "SELECT MIN(unixepoch(timestamp))
           FROM events
          WHERE agent_ms IS NULL AND unixepoch(timestamp) IS NOT NULL",
        [],
        |row| row.get(0),
    )?;

    let Some(oldest) = oldest else {
        return Ok(0);
    };
    backfill_agent_ms(conn, oldest)
}

pub fn backfill_agent_ms(conn: &mut Connection, since_secs: i64) -> Result<i64, Error> {
    let now_secs = Utc::now().timestamp();
    // Settle boundary: rows newer than this may still be filled by the live
    // watcher, so the healer leaves them NULL to avoid a race.
    let before_secs = now_secs.saturating_sub(AGENT_HEAL_GRACE_SECS);
    if before_secs <= since_secs {
        return Ok(0);
    }

    // Log discovery is substantially more expensive than the indexed event
    // query. Compute it once for the requested interval, then reuse it while
    // draining bounded database batches.
    let busy = harness::busy_minutes(since_secs, before_secs);
    let mut settled = 0i64;
    let mut cursor: Option<(String, i64)> = None;

    loop {
        let rows = events_needing_agent_ms_after(
            conn,
            since_secs,
            before_secs,
            cursor
                .as_ref()
                .map(|(timestamp, id)| (timestamp.as_str(), *id)),
        )?;
        if rows.is_empty() {
            break;
        }
        cursor = rows
            .last()
            .map(|(id, timestamp, _)| (timestamp.clone(), *id));

        // Each transaction is independently bounded so large repairs do not
        // hold the write lock indefinitely.
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE events SET agent_ms = ?1 WHERE id = ?2")?;
            for (id, timestamp, span_ms) in &rows {
                let Ok(start) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
                    // Keep malformed rows unknown. The cursor advances past
                    // them so valid rows behind them can still be repaired.
                    continue;
                };
                let start_ms = start.timestamp_millis();
                let agent_ms = busy.overlap_ms(start_ms, start_ms.saturating_add(*span_ms));
                settled = settled.saturating_add(
                    i64::try_from(stmt.execute(params![agent_ms, id])?).unwrap_or(i64::MAX),
                );
            }
        }
        tx.commit()?;
    }

    Ok(settled)
}

/// Reclassify events with zero input as passive instead of active, returning
/// count updated.
pub fn fix_false_active(conn: &Connection, input_active_ms: u64) -> Result<i64, Error> {
    let updated = conn.execute(
        "UPDATE events
            SET passive_ms = passive_ms + active_ms,
                active_ms = 0
          WHERE keystrokes = 0
            AND mouse_clicks = 0
            AND active_ms > ?1",
        params![i64::try_from(input_active_ms).unwrap_or(i64::MAX)],
    )?;
    Ok(i64::try_from(updated).unwrap_or(i64::MAX))
}

/// Insert a session event into the database with all activity metrics.
pub fn insert_event(conn: &Connection, snapshot: SessionSnapshot<'_>) -> Result<(), Error> {
    let category = snapshot
        .config
        .classify(&snapshot.window.app_id, &snapshot.window.title);
    let offsets_blob: Vec<u8> = snapshot
        .input_offsets
        .iter()
        .flat_map(|&n| n.to_le_bytes())
        .collect();
    conn.execute(
        "INSERT INTO events (
            timestamp, app_id, title, category, active_ms, passive_ms, idle_ms,
            keystrokes, mouse_clicks, scroll_events, mouse_distance,
            jiggler_detected, input_offsets,
            backspace_count, modifier_count, left_clicks, right_clicks, middle_clicks,
            scroll_up, scroll_down, scroll_horizontal, project, git_branch, agent_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
        params![
            snapshot.focus_start.to_rfc3339(),
            &snapshot.window.app_id,
            &snapshot.window.title,
            category.to_string(),
            snapshot.active_ms,
            snapshot.passive_ms,
            snapshot.idle_ms,
            i64::try_from(snapshot.input.keystrokes).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.mouse_clicks).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.scroll_events).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.mouse_distance).unwrap_or(i64::MAX),
            snapshot.jiggler_detected as i32,
            offsets_blob,
            i64::try_from(snapshot.input.backspace_count).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.modifier_count).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.left_clicks).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.right_clicks).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.middle_clicks).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.scroll_up).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.scroll_down).unwrap_or(i64::MAX),
            i64::try_from(snapshot.input.scroll_horizontal).unwrap_or(i64::MAX),
            snapshot.project.as_deref(),
            snapshot.git_branch.as_deref(),
            snapshot.agent_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn decode_input_offsets(blob: &[u8]) -> Vec<u32> {
    blob.chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

const INPUT_OFFSET_DRIFT_TOLERANCE_MS: u32 = 1_000;

pub(crate) fn normalize_input_offsets(blob: &[u8], total_duration_ms: i64) -> Option<Vec<u32>> {
    let mut offsets = decode_input_offsets(blob);
    let duration = u32::try_from(total_duration_ms.max(0)).unwrap_or(u32::MAX);
    let shift = offsets
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        .saturating_sub(duration);
    if shift > INPUT_OFFSET_DRIFT_TOLERANCE_MS {
        return None;
    }
    for offset in &mut offsets {
        *offset = offset.saturating_sub(shift).min(duration);
    }
    offsets.sort_unstable();
    offsets.dedup();
    Some(offsets)
}

fn replay_classification(
    offsets: &[u32],
    total_duration_ms: i64,
    idle_threshold_ms: u64,
    deep_idle_threshold_ms: u64,
) -> (i64, i64, i64) {
    if offsets.is_empty() || total_duration_ms <= 0 {
        return (0, 0, total_duration_ms.max(0));
    }

    let mut active_ms: i64 = 0;
    let mut passive_ms: i64 = 0;
    let mut idle_ms: i64 = 0;

    let total_u64 = u64::try_from(total_duration_ms).unwrap_or(0);
    let mut prev_offset: u64 = 0;
    for &offset in offsets {
        let offset_u64 = u64::from(offset).min(total_u64);
        if offset_u64 <= prev_offset {
            continue;
        }
        let gap = offset_u64.saturating_sub(prev_offset);

        if gap <= idle_threshold_ms {
            active_ms = active_ms.saturating_add(i64::try_from(gap).unwrap_or(i64::MAX));
        } else if gap <= deep_idle_threshold_ms {
            active_ms =
                active_ms.saturating_add(i64::try_from(idle_threshold_ms).unwrap_or(i64::MAX));
            passive_ms = passive_ms.saturating_add(
                i64::try_from(gap.saturating_sub(idle_threshold_ms)).unwrap_or(i64::MAX),
            );
        } else {
            active_ms =
                active_ms.saturating_add(i64::try_from(idle_threshold_ms).unwrap_or(i64::MAX));
            passive_ms = passive_ms.saturating_add(
                i64::try_from(deep_idle_threshold_ms.saturating_sub(idle_threshold_ms))
                    .unwrap_or(i64::MAX),
            );
            idle_ms = idle_ms.saturating_add(
                i64::try_from(gap.saturating_sub(deep_idle_threshold_ms)).unwrap_or(i64::MAX),
            );
        }
        prev_offset = offset_u64;
    }

    let last_offset = prev_offset;
    if total_u64 > last_offset {
        let trailing_gap = total_u64 - last_offset;
        if trailing_gap <= idle_threshold_ms {
            active_ms = active_ms.saturating_add(i64::try_from(trailing_gap).unwrap_or(i64::MAX));
        } else if trailing_gap <= deep_idle_threshold_ms {
            active_ms =
                active_ms.saturating_add(i64::try_from(idle_threshold_ms).unwrap_or(i64::MAX));
            passive_ms = passive_ms.saturating_add(
                i64::try_from(trailing_gap - idle_threshold_ms).unwrap_or(i64::MAX),
            );
        } else {
            active_ms =
                active_ms.saturating_add(i64::try_from(idle_threshold_ms).unwrap_or(i64::MAX));
            passive_ms = passive_ms.saturating_add(
                i64::try_from(deep_idle_threshold_ms.saturating_sub(idle_threshold_ms))
                    .unwrap_or(i64::MAX),
            );
            idle_ms = idle_ms.saturating_add(
                i64::try_from(trailing_gap - deep_idle_threshold_ms).unwrap_or(i64::MAX),
            );
        }
    }

    (active_ms, passive_ms, idle_ms)
}

/// Reclassify active/passive/idle time based on input offset thresholds,
/// returning (updated, total).
pub fn reclassify_with_thresholds(
    conn: &mut Connection,
    idle_threshold_secs: u64,
    deep_idle_secs: u64,
) -> Result<(i64, i64), Error> {
    if deep_idle_secs <= idle_threshold_secs {
        return Err(Error::InvalidArgument(format!(
            "deep-idle threshold ({deep_idle_secs}s) must be greater than idle threshold ({idle_threshold_secs}s)"
        )));
    }

    let idle_threshold_ms = idle_threshold_secs.saturating_mul(1000);
    let deep_idle_threshold_ms = deep_idle_secs.saturating_mul(1000);

    #[allow(clippy::type_complexity)] // One-shot DB row tuple, not worth a named struct
    let rows: Vec<(i64, Option<Vec<u8>>, i64, i64, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT id, input_offsets, active_ms, passive_ms, idle_ms
               FROM events
              WHERE input_offsets IS NOT NULL AND length(input_offsets) > 0",
        )?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    let total_rows = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let mut updated: i64 = 0;

    let tx = conn.transaction()?;
    for (id, blob_opt, old_active, old_passive, old_idle) in rows {
        if let Some(blob) = blob_opt {
            if blob.len() % 4 != 0 {
                continue;
            }
            let total_duration = old_active
                .saturating_add(old_passive)
                .saturating_add(old_idle);
            let Some(offsets) = normalize_input_offsets(&blob, total_duration) else {
                continue;
            };
            let (new_active, new_passive, new_idle) = replay_classification(
                &offsets,
                total_duration,
                idle_threshold_ms,
                deep_idle_threshold_ms,
            );

            if new_active != old_active || new_passive != old_passive || new_idle != old_idle {
                tx.execute(
                    "UPDATE events SET active_ms = ?1, passive_ms = ?2, idle_ms = ?3 WHERE id = ?4",
                    params![new_active, new_passive, new_idle, id],
                )?;
                updated = updated.saturating_add(1);
            }
        }
    }
    tx.commit()?;

    Ok((updated, total_rows))
}

/// Terminal app IDs that may contain project info in their window titles.
const TERMINAL_APP_IDS: &[&str] = &[
    "Alacritty",
    "kitty",
    "foot",
    "org.wezfurlong.wezterm",
    "wezterm",
    "alacritty",
    "org.codeberg.dnkl.foot",
];

/// Backfill the `project` column for terminal events that have NULL project.
///
/// Processes rows in batches for performance, applying
/// `detect_project_from_title` and optional project aliases. Returns
/// (total_candidates, detected_count).
const PROJECT_BACKFILL_BATCH_SIZE: usize = 1_000;

pub fn backfill_projects(
    conn: &mut Connection,
    aliases: &std::collections::HashMap<String, String>,
) -> Result<(i64, i64), Error> {
    backfill_projects_in_batches(conn, aliases, PROJECT_BACKFILL_BATCH_SIZE)
}

fn backfill_projects_in_batches(
    conn: &mut Connection,
    aliases: &std::collections::HashMap<String, String>,
    batch_size: usize,
) -> Result<(i64, i64), Error> {
    use crate::project::detect_project_from_title;

    let placeholders: String = TERMINAL_APP_IDS
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let terminal_params: Vec<&dyn rusqlite::types::ToSql> = TERMINAL_APP_IDS
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let count_sql =
        format!("SELECT COUNT(*) FROM events WHERE project IS NULL AND app_id IN ({placeholders})");
    let total: i64 = conn
        .prepare(&count_sql)?
        .query_row(terminal_params.as_slice(), |row| row.get(0))?;

    if total == 0 {
        println!("No terminal events with NULL project found. Nothing to backfill.");
        return Ok((0, 0));
    }
    println!("Found {total} terminal events with NULL project");

    let cursor_param = TERMINAL_APP_IDS.len() + 1;
    let limit_param = cursor_param + 1;
    let select_sql = format!(
        "SELECT id, title
           FROM events
          WHERE project IS NULL
            AND app_id IN ({placeholders})
            AND (?{cursor_param} IS NULL OR id > ?{cursor_param})
          ORDER BY id
          LIMIT ?{limit_param}"
    );
    let batch_limit = i64::try_from(batch_size.max(1)).unwrap_or(i64::MAX);
    let mut last_id: Option<i64> = None;
    let mut detected = 0i64;
    let mut processed = 0i64;

    loop {
        let rows: Vec<(i64, String)> = {
            let mut query_params = terminal_params.clone();
            query_params.push(&last_id);
            query_params.push(&batch_limit);
            let mut stmt = conn.prepare(&select_sql)?;
            stmt.query_map(query_params.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let Some((page_last_id, _)) = rows.last() else {
            break;
        };
        last_id = Some(*page_last_id);

        let tx = conn.transaction()?;
        for (id, title) in &rows {
            if let Some(mut project) = detect_project_from_title(title) {
                if let Some(alias) = aliases.get(&project) {
                    project = alias.clone();
                }
                tx.execute(
                    "UPDATE events SET project = ?1 WHERE id = ?2",
                    params![project, id],
                )?;
                detected = detected.saturating_add(1);
            }
            processed = processed.saturating_add(1);
        }
        tx.commit()?;
        println!("Backfilled {processed} of {total} events ({detected} with project detected)");
    }

    Ok((total, detected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Category, Config, DomainRule};

    #[test]
    fn migrations_initialize_a_fresh_database() {
        let mut conn = Connection::open_in_memory().expect("in-memory db");

        run_migrations(&mut conn, &Config::default()).expect("migrations");

        let events_table: String = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'events'",
                [],
                |row| row.get(0),
            )
            .expect("events table");
        assert_eq!(events_table, "events");
    }

    #[test]
    fn replay_clamps_input_offsets_to_the_event_duration() {
        let classified = replay_classification(&[360_000], 300_000, 120_000, 300_000);

        assert_eq!(classified, (120_000, 180_000, 0));
    }

    #[test]
    fn normalization_rebases_drifted_offsets_at_the_flush_boundary() {
        let blob = [1_400u32, 1_900]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();

        assert_eq!(
            normalize_input_offsets(&blob, 1_000),
            Some(vec![500, 1_000])
        );
    }

    #[test]
    fn normalization_preserves_valid_offsets() {
        let blob = [100u32, 200]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();

        assert_eq!(normalize_input_offsets(&blob, 500), Some(vec![100, 200]));
    }

    #[test]
    fn normalization_rejects_large_origin_drift() {
        let blob = [9_000u32, 9_500]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();

        assert_eq!(normalize_input_offsets(&blob, 500), None);
    }

    #[test]
    fn reclassification_rejects_reversed_thresholds() {
        let mut conn = db_with_events(&[]);

        let error = reclassify_with_thresholds(&mut conn, 300, 120)
            .expect_err("deep-idle must follow idle");

        assert!(matches!(error, Error::InvalidArgument(_)));
    }

    #[test]
    fn category_reclassification_drains_multiple_bounded_batches() {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        init_db(&conn).expect("schema");
        for id in 1..=5 {
            conn.execute(
                "INSERT INTO events (id, timestamp, app_id, title, category, active_ms, idle_ms)
                 VALUES (?1, '2026-08-16T12:00:00+00:00', 'foot', 'terminal', 'unproductive', 1, 0)",
                [id],
            )
            .expect("fixture");
        }
        let config = Config {
            categories: std::collections::HashMap::from([(
                "foot".to_string(),
                Category::Productive,
            )]),
            ..Config::default()
        };

        reclassify_all_in_batches(&mut conn, &config, 2).expect("reclassify");

        let productive: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE category = 'productive'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(productive, 5);
    }

    #[test]
    fn threshold_reclassification_skips_empty_input_offsets() {
        let mut conn = db_with_events(&[]);
        conn.execute(
            "INSERT INTO events (
                 timestamp, app_id, title, category, active_ms, passive_ms, idle_ms, input_offsets
             ) VALUES ('2026-08-16T12:00:00+00:00', 'foot', 'terminal', 'productive', 60000, 30000, 0, x'')",
            [],
        )
        .expect("fixture");

        let result = reclassify_with_thresholds(&mut conn, 120, 300).expect("reclassify");
        let times: (i64, i64, i64) = conn
            .query_row(
                "SELECT active_ms, passive_ms, idle_ms FROM events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("times");

        assert_eq!(result, (0, 0), "empty offsets are not safe to replay");
        assert_eq!(times, (60_000, 30_000, 0));
    }

    #[test]
    fn threshold_reclassification_preserves_malformed_input_offsets() {
        for trailing_bytes in 1..=3 {
            let mut conn = db_with_events(&[]);
            let mut blob = 30_000u32.to_le_bytes().to_vec();
            blob.extend(std::iter::repeat_n(0xA5, trailing_bytes));
            conn.execute(
                "INSERT INTO events (
                     timestamp, app_id, title, category, active_ms, passive_ms, idle_ms,
                     input_offsets
                 ) VALUES (
                     '2026-08-16T12:00:00+00:00', 'foot', 'terminal', 'productive',
                     60000, 30000, 10000, ?1
                 )",
                params![blob],
            )
            .expect("fixture");

            let before: (i64, i64, i64, Vec<u8>) = conn
                .query_row(
                    "SELECT active_ms, passive_ms, idle_ms, input_offsets FROM events",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("before");
            let result = reclassify_with_thresholds(&mut conn, 20, 40).expect("reclassify");
            let after: (i64, i64, i64, Vec<u8>) = conn
                .query_row(
                    "SELECT active_ms, passive_ms, idle_ms, input_offsets FROM events",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("after");

            assert_eq!(result, (0, 1));
            assert_eq!(after, before, "trailing byte count: {trailing_bytes}");
        }
    }

    #[test]
    fn project_backfill_drains_multiple_bounded_batches() {
        let mut conn = db_with_events(&[]);
        for id in 1..=5 {
            let title = if id == 3 { "not a project" } else { "OpenCode" };
            conn.execute(
                "INSERT INTO events (
                     id, timestamp, app_id, title, category, active_ms, idle_ms, project
                 ) VALUES (?1, '2026-08-16T12:00:00+00:00', 'foot', ?2, 'productive', 1, 0, NULL)",
                params![id, title],
            )
            .expect("fixture");
        }

        let result = backfill_projects_in_batches(&mut conn, &std::collections::HashMap::new(), 2)
            .expect("backfill");
        let detected: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE project = 'OpenCode'",
                [],
                |row| row.get(0),
            )
            .expect("count");

        assert_eq!(result, (5, 4));
        assert_eq!(detected, 4);
    }

    #[test]
    fn reclassification_preserves_categories_when_browser_history_is_unavailable() {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        init_db(&conn).expect("schema");
        conn.execute(
            "INSERT INTO events (timestamp, app_id, title, category, active_ms, idle_ms)
             VALUES ('2026-08-16T12:00:00+00:00', 'zen', 'A Video', 'unproductive', 60000, 0)",
            [],
        )
        .expect("fixture");
        let config = Config {
            categories: std::collections::HashMap::from([(
                "zen".to_string(),
                Category::Productive,
            )]),
            domain_rules: vec![DomainRule {
                domain: "youtube.com".to_string(),
                category: Category::Unproductive,
            }],
            ..Config::default()
        };

        run_migrations(&mut conn, &config).expect("migrations");
        reclassify_all(&mut conn, &config).expect("reclassify");

        let category: String = conn
            .query_row("SELECT category FROM events", [], |row| row.get(0))
            .expect("category");
        assert_eq!(category, "unproductive");
    }

    fn db_with_events(timestamps: &[&str]) -> Connection {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        init_db(&conn).expect("schema");
        run_migrations(&mut conn, &Config::default()).expect("migrations");
        for ts in timestamps {
            conn.execute(
                "INSERT INTO events (timestamp, app_id, title, category, active_ms, idle_ms)
                 VALUES (?1, 'foot', 't', 'productive', 60000, 0)",
                params![ts],
            )
            .expect("insert");
        }
        conn
    }

    #[test]
    fn backfill_skips_events_older_than_the_scanned_window() {
        // Writing 0 outside the window once stamped 574,214 historical rows
        // as "measured, no agent" when no log had been read for them.
        let conn = db_with_events(&["2026-02-02T12:00:00+00:00", "2026-08-16T12:00:00+00:00"]);
        let since = chrono::DateTime::parse_from_rfc3339("2026-08-10T00:00:00+00:00")
            .expect("fixed date")
            .timestamp();

        // Far-future settle boundary so the window itself, not the grace
        // window, is what limits eligibility in this test.
        let before = chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00+00:00")
            .expect("fixed date")
            .timestamp();
        let eligible = events_needing_agent_ms(&conn, since, before).expect("query");
        assert_eq!(
            eligible.len(),
            1,
            "only the event inside the window is eligible"
        );
    }

    #[test]
    fn healing_is_a_no_op_when_every_event_is_measured() {
        let mut conn = db_with_events(&["2026-08-16T12:00:00+00:00"]);
        conn.execute("UPDATE events SET agent_ms = 0", [])
            .expect("mark measured");

        assert_eq!(
            heal_missing_agent_ms(&mut conn).expect("heal"),
            0,
            "nothing pending must not trigger a log scan"
        );
    }

    #[test]
    fn healing_starts_from_the_oldest_unmeasured_event() {
        let conn = db_with_events(&["2026-02-02T12:00:00+00:00", "2026-08-16T12:00:00+00:00"]);
        let oldest: Option<String> = conn
            .query_row(
                "SELECT MIN(timestamp) FROM events WHERE agent_ms IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("query");

        assert_eq!(oldest.as_deref(), Some("2026-02-02T12:00:00+00:00"));
    }

    #[test]
    fn backfill_considers_every_event_when_the_window_is_wide_enough() {
        let conn = db_with_events(&["2026-02-02T12:00:00+00:00", "2026-08-16T12:00:00+00:00"]);
        let before = chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00+00:00")
            .expect("fixed date")
            .timestamp();
        let rows = events_needing_agent_ms(&conn, 0, before).expect("query");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn backfill_settles_old_rows_to_measured_zero_so_heal_converges() {
        // Two rows well in the past (older than the grace window) with no agent
        // activity. After backfill they must have agent_ms = 0 (measured), not
        // NULL, so the healer does not rescan them forever. This is the
        // convergence fix for unmeasured no-agent rows.
        let mut conn = db_with_events(&["2026-01-01T00:00:00+00:00", "2026-01-01T00:01:00+00:00"]);
        let before_null: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE agent_ms IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(before_null, 2);

        // Scan from the start of that day; there is no harness activity in
        // tests, so every settled row should get a measured 0.
        let since = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00")
            .expect("fixed")
            .timestamp();
        let settled = backfill_agent_ms(&mut conn, since).expect("backfill");
        assert_eq!(settled, 2, "progress includes measured-zero rows");

        let still_null: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE agent_ms IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(still_null, 0, "settled rows must be measured, not NULL");

        // A second heal pass finds nothing pending and is a no-op.
        let second = heal_missing_agent_ms(&mut conn).expect("heal");
        assert_eq!(second, 0, "heal converges: nothing left to scan");
    }

    #[test]
    fn backfill_drains_more_than_one_bounded_batch() {
        let mut conn = db_with_events(&[]);
        let total = i64::try_from(MAX_AGENT_BACKFILL_ROWS).expect("batch size") + 1;
        conn.execute(
            "WITH RECURSIVE sequence(n) AS (
                 SELECT 1
                 UNION ALL SELECT n + 1 FROM sequence WHERE n < ?1
             )
             INSERT INTO events (timestamp, app_id, title, category, active_ms, idle_ms)
             SELECT printf('2026-01-01T00:%02d:%02d+00:00', (n / 60) % 60, n % 60),
                    'foot', 't', 'productive', 1000, 0
               FROM sequence",
            [total],
        )
        .expect("bulk fixture");

        let since = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00")
            .expect("fixed")
            .timestamp();
        let settled = backfill_agent_ms(&mut conn, since).expect("backfill");
        let still_null: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE agent_ms IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("count");

        assert_eq!(settled, total, "progress counts every settled row");
        assert_eq!(still_null, 0, "all batches must drain in one call");
    }

    #[test]
    fn malformed_oldest_timestamp_does_not_wedge_healing() {
        let mut conn = db_with_events(&["not-a-timestamp", "2026-01-01T00:00:00+00:00"]);

        let settled = heal_missing_agent_ms(&mut conn).expect("heal");
        let malformed: Option<i64> = conn
            .query_row(
                "SELECT agent_ms FROM events WHERE timestamp = 'not-a-timestamp'",
                [],
                |row| row.get(0),
            )
            .expect("malformed row");
        let valid: Option<i64> = conn
            .query_row(
                "SELECT agent_ms FROM events WHERE timestamp != 'not-a-timestamp'",
                [],
                |row| row.get(0),
            )
            .expect("valid row");

        assert_eq!(settled, 1);
        assert_eq!(malformed, None, "malformed input must remain unknown");
        assert!(valid.is_some(), "valid rows still heal");
    }

    #[test]
    fn backfill_leaves_recent_rows_null_for_the_live_watcher() {
        // A row inside the grace window must stay NULL so the live daemon, not
        // the healer, measures it (avoids a race).
        let now = Utc::now();
        let recent = now.to_rfc3339();
        let mut conn = db_with_events(&[&recent]);
        let since = now.timestamp() - 3600;
        backfill_agent_ms(&mut conn, since).expect("backfill");
        let null_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE agent_ms IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(null_count, 1, "recent row left for the live watcher");
    }
}
