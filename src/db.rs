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
    pub input: InputSnapshot,
    pub jiggler_detected: bool,
    pub input_offsets: Vec<u32>,
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

    if !applied.contains(&"001_fix_historical_categories".to_string()) {
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

    if !applied.contains(&"002_apply_title_rules".to_string()) && !config.title_rules.is_empty() {
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

    if !applied.contains(&"003_app_scoped_title_rules".to_string()) {
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

    Ok(())
}

/// Reclassify all events in the database according to the current
/// configuration.
pub fn reclassify_all(conn: &mut Connection, config: &Config) -> Result<(), Error> {
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
            scroll_up, scroll_down, scroll_horizontal
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
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
        ],
    )?;
    Ok(())
}

fn decode_input_offsets(blob: &[u8]) -> Vec<u32> {
    blob.chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
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

    let mut prev_offset: u64 = 0;
    for &offset in offsets {
        let offset_u64 = u64::from(offset);
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

    let last_offset = u64::from(*offsets.last().unwrap_or(&0));
    let total_u64 = u64::try_from(total_duration_ms).unwrap_or(0);
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
            let offsets = decode_input_offsets(&blob);
            let total_duration = old_active
                .saturating_add(old_passive)
                .saturating_add(old_idle);
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
