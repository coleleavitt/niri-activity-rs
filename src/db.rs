use chrono::Utc;
use rusqlite::{params, Connection};

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

pub fn run_migrations(conn: &Connection, config: &Config) -> Result<(), Error> {
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
            .filter_map(|r| r.ok())
            .collect()
    };

    if !applied.contains(&"001_fix_historical_categories".to_string()) {
        let mut updated = 0i64;
        for (app_id, category) in &config.categories {
            let count = conn.execute(
                "UPDATE events SET category = ?1 WHERE app_id = ?2 AND category != ?1",
                params![category.to_string(), app_id],
            )?;
            updated += count as i64;
        }
        conn.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["001_fix_historical_categories", Utc::now().to_rfc3339()],
        )?;
        if updated > 0 {
            eprintln!(
                "Migration 001: fixed {} events with stale categories",
                updated
            );
        }
    }

    if !applied.contains(&"002_apply_title_rules".to_string()) && !config.title_rules.is_empty() {
        let mut stmt = conn.prepare("SELECT id, app_id, title FROM events")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut updated = 0i64;
        for (id, app_id, title) in &rows {
            let correct = config.classify(app_id, title);
            let count = conn.execute(
                "UPDATE events SET category = ?1 WHERE id = ?2 AND category != ?1",
                params![correct.to_string(), id],
            )?;
            updated += count as i64;
        }

        conn.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["002_apply_title_rules", Utc::now().to_rfc3339()],
        )?;
        if updated > 0 {
            eprintln!(
                "Migration 002: reclassified {} events with title rules",
                updated
            );
        }
    }

    if !applied.contains(&"003_app_scoped_title_rules".to_string()) {
        let mut stmt = conn.prepare("SELECT id, app_id, title, category FROM events")?;
        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut updated = 0i64;
        for (id, app_id, title, old_category) in &rows {
            let correct = config.classify(app_id, title);
            if correct.to_string() != *old_category {
                conn.execute(
                    "UPDATE events SET category = ?1 WHERE id = ?2",
                    params![correct.to_string(), id],
                )?;
                updated += 1;
            }
        }

        conn.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["003_app_scoped_title_rules", Utc::now().to_rfc3339()],
        )?;
        if updated > 0 {
            eprintln!(
                "Migration 003: reclassified {} events with app-scoped title rules",
                updated
            );
        }
    }

    if !applied.contains(&"004_add_passive_and_jiggler".to_string()) {
        conn.execute_batch(
            "ALTER TABLE events ADD COLUMN passive_ms INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE events ADD COLUMN jiggler_detected INTEGER NOT NULL DEFAULT 0;",
        )?;
        conn.execute(
            "INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)",
            params!["004_add_passive_and_jiggler", Utc::now().to_rfc3339()],
        )?;
        eprintln!("Migration 004: added passive_ms and jiggler_detected columns");
    }

    Ok(())
}

pub fn reclassify_all(conn: &Connection, config: &Config) -> Result<(), Error> {
    let mut stmt = conn.prepare("SELECT id, app_id, title, category FROM events")?;
    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut updated = 0i64;
    for (id, app_id, title, old_category) in &rows {
        let correct = config.classify(app_id, title).to_string();
        if correct != *old_category {
            conn.execute(
                "UPDATE events SET category = ?1 WHERE id = ?2",
                params![correct, id],
            )?;
            updated += 1;
        }
    }
    if updated > 0 {
        eprintln!("Reclassified {} events to match current config", updated);
    }
    Ok(())
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
            snapshot.input.keystrokes as i64,
            snapshot.input.mouse_clicks as i64,
            snapshot.input.scroll_events as i64,
            snapshot.input.mouse_distance as i64,
            snapshot.jiggler_detected as i32,
        ],
    )?;
    Ok(())
}
