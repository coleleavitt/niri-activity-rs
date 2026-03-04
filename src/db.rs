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
}

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
            eprintln!(
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
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
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
            eprintln!(
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
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
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
            eprintln!(
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
        eprintln!("Migration 004: added passive_ms and jiggler_detected columns");
    }

    Ok(())
}

pub fn reclassify_all(conn: &mut Connection, config: &Config) -> Result<(), Error> {
    // Safety: check row count before loading all rows into memory.
    // For databases with >5M events, this could use significant RAM.
    const MAX_RECLASSIFY_ROWS: i64 = 5_000_000;
    let row_count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    if row_count > MAX_RECLASSIFY_ROWS {
        eprintln!(
            "Warning: {} events exceeds reclassify limit ({}), skipping bulk reclassify",
            row_count, MAX_RECLASSIFY_ROWS
        );
        return Ok(());
    }

    let rows: Vec<(i64, String, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, app_id, title, category FROM events")?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    let tx = conn.transaction()?;
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
        eprintln!("Reclassified {} events to match current config", updated);
    }
    Ok(())
}

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

pub fn insert_event(conn: &Connection, snapshot: SessionSnapshot<'_>) -> Result<(), Error> {
    let category = snapshot
        .config
        .classify(&snapshot.window.app_id, &snapshot.window.title);
    conn.execute(
        "INSERT INTO events (timestamp, app_id, title, category, active_ms, passive_ms, idle_ms, keystrokes, mouse_clicks, scroll_events, mouse_distance, jiggler_detected) 
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
        ],
    )?;
    Ok(())
}
