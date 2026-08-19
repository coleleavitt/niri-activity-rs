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

    Ok(())
}

/// Reclassify all events in the database according to the current
/// configuration.
pub fn reclassify_all(conn: &mut Connection, config: &Config) -> Result<(), Error> {
    if !config.can_reclassify_all() {
        tracing::warn!("Skipping category reclassification because browser history is unavailable");
        return Ok(());
    }

    // Safety: check row count before loading all rows into memory.
    // For databases with >5M events, this could use significant RAM.
    const MAX_RECLASSIFY_ROWS: i64 = 5_000_000;
    let row_count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    if row_count > MAX_RECLASSIFY_ROWS {
        tracing::warn!(
            "{} events exceeds reclassify limit ({}), skipping bulk reclassify",
            row_count,
            MAX_RECLASSIFY_ROWS
        );
        return Ok(());
    }

    let tx = conn.transaction()?;
    let rows: Vec<(i64, String, String, String)> = {
        let mut stmt = tx.prepare("SELECT id, app_id, title, category FROM events")?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    let mut updated = 0i64;
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
    if updated > 0 {
        tracing::info!("Reclassified {} events to match current config", updated);
    }
    Ok(())
}

/// Fill `agent_ms` for events recorded before agent tracking existed.
///
/// Reconstructs agent activity from harness logs and intersects it with each
/// session's own span, so a five-minute focus that overlapped two busy minutes
/// is credited two minutes rather than five.
fn events_needing_agent_ms(
    conn: &Connection,
    since_secs: i64,
) -> Result<Vec<(i64, String, i64)>, Error> {
    let since_rfc3339 = chrono::DateTime::from_timestamp(since_secs, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id, timestamp, active_ms + COALESCE(passive_ms,0) + idle_ms
           FROM events WHERE agent_ms IS NULL AND timestamp >= ?1",
    )?;
    let rows = stmt
        .query_map(params![&since_rfc3339], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Measure any events the live watcher never recorded agent time for.
///
/// Scans from the oldest unmeasured event rather than a fixed window, so a
/// daemon that was down for a day and one down for a month both heal without
/// a configured lookback. Returns `Ok(0)` immediately when nothing is pending,
/// which the partial index makes effectively free.
pub fn heal_missing_agent_ms(conn: &mut Connection) -> Result<i64, Error> {
    // MIN over an empty set yields one row holding NULL, so the Option is the
    // "nothing pending" signal rather than a missing row.
    let oldest: Option<String> = conn.query_row(
        "SELECT MIN(timestamp) FROM events WHERE agent_ms IS NULL",
        [],
        |row| row.get(0),
    )?;

    let Some(oldest) = oldest else {
        return Ok(0);
    };
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(&oldest) else {
        return Ok(0);
    };
    backfill_agent_ms(conn, start.timestamp())
}

pub fn backfill_agent_ms(conn: &mut Connection, since_secs: i64) -> Result<i64, Error> {
    let until_secs = Utc::now().timestamp();
    let busy = harness::busy_minutes(since_secs, until_secs);
    if busy.is_empty() {
        return Ok(0);
    }

    // Only events inside the scanned window can be measured. Writing 0 to an
    // event outside it would claim no agent ran, when the truth is that no
    // log was read for that time.
    let rows = events_needing_agent_ms(conn, since_secs)?;
    let tx = conn.transaction()?;
    let mut filled = 0i64;
    for (id, timestamp, span_ms) in &rows {
        let Ok(start) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
            continue;
        };
        let start_ms = start.timestamp_millis();
        let agent_ms = busy.overlap_ms(start_ms, start_ms.saturating_add(*span_ms));
        tx.execute(
            "UPDATE events SET agent_ms = ?1 WHERE id = ?2",
            params![agent_ms, id],
        )?;
        if agent_ms > 0 {
            filled = filled.saturating_add(1);
        }
    }
    tx.commit()?;
    Ok(filled)
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
            "SELECT id, input_offsets, active_ms, passive_ms, idle_ms FROM events WHERE input_offsets IS NOT NULL"
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
pub fn backfill_projects(
    conn: &mut Connection,
    aliases: &std::collections::HashMap<String, String>,
) -> Result<(i64, i64), Error> {
    use crate::project::detect_project_from_title;

    // Build the IN clause for terminal apps
    let placeholders: String = TERMINAL_APP_IDS
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");

    let count_sql = format!(
        "SELECT COUNT(*) FROM events WHERE project IS NULL AND app_id IN ({})",
        placeholders
    );

    let total: i64 = {
        let mut stmt = conn.prepare(&count_sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = TERMINAL_APP_IDS
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        stmt.query_row(params.as_slice(), |row| row.get(0))?
    };

    if total == 0 {
        println!("No terminal events with NULL project found. Nothing to backfill.");
        return Ok((0, 0));
    }

    println!("Found {} terminal events with NULL project", total);

    let select_sql = format!(
        "SELECT id, title FROM events WHERE project IS NULL AND app_id IN ({}) ORDER BY id",
        placeholders
    );

    // Read all candidate rows (id, title)
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(&select_sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = TERMINAL_APP_IDS
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        stmt.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    let mut detected: i64 = 0;
    let mut processed: i64 = 0;
    let batch_size = 1000;

    for chunk in rows.chunks(batch_size) {
        let tx = conn.transaction()?;
        for (id, title) in chunk {
            if let Some(mut project) = detect_project_from_title(title) {
                // Apply aliases
                if let Some(alias) = aliases.get(&project) {
                    project = alias.clone();
                }
                tx.execute(
                    "UPDATE events SET project = ?1 WHERE id = ?2",
                    params![project, id],
                )?;
                detected += 1;
            }
            processed += 1;
        }
        tx.commit()?;

        // Print progress every batch
        println!(
            "Backfilled {} of {} events ({} with project detected)",
            processed, total, detected
        );
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

        let eligible = events_needing_agent_ms(&conn, since).expect("query");
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
        let rows = events_needing_agent_ms(&conn, 0).expect("query");
        assert_eq!(rows.len(), 2);
    }
}
