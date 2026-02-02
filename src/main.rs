use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use niri_ipc::{socket::Socket, Request, Response};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("niri ipc error: {0}")]
    NiriIpc(#[from] std::io::Error),
    #[error("niri returned error: {0}")]
    NiriError(String),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("unexpected response from niri")]
    UnexpectedResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowEvent {
    pub timestamp: DateTime<Utc>,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub duration_ms: i64,
}

#[derive(Parser)]
#[command(name = "actitivty-rs")]
#[command(about = "Track window focus on Niri compositor")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the watcher daemon
    Watch {
        /// Poll interval in milliseconds
        #[arg(short, long, default_value = "5000")]
        interval: u64,
    },
    /// Show today's activity
    Today,
    /// Show activity summary
    Summary {
        /// Number of days to show
        #[arg(short, long, default_value = "7")]
        days: u32,
    },
}

fn get_db_path() -> std::path::PathBuf {
    let dirs = directories::ProjectDirs::from("", "", "actitivty-rs")
        .expect("Could not determine data directory");
    let data_dir = dirs.data_dir();
    std::fs::create_dir_all(data_dir).expect("Could not create data directory");
    data_dir.join("activity.db")
}

fn init_db(conn: &Connection) -> Result<(), Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY,
            timestamp TEXT NOT NULL,
            app_id TEXT,
            title TEXT,
            duration_ms INTEGER NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_timestamp ON events(timestamp)",
        [],
    )?;
    Ok(())
}

fn get_focused_window() -> Result<Option<(String, String)>, Error> {
    let mut socket = Socket::connect()?;
    let reply = socket.send(Request::FocusedWindow)?;

    match reply {
        Ok(Response::FocusedWindow(Some(window))) => {
            let app_id = window.app_id.unwrap_or_else(|| "unknown".to_string());
            let title = window.title.unwrap_or_else(|| "".to_string());
            Ok(Some((app_id, title)))
        }
        Ok(Response::FocusedWindow(None)) => Ok(None),
        Ok(_) => Err(Error::UnexpectedResponse),
        Err(e) => Err(Error::NiriError(e)),
    }
}

fn watch(interval_ms: u64) -> Result<(), Error> {
    let db_path = get_db_path();
    println!("Database: {}", db_path.display());

    let conn = Connection::open(&db_path)?;
    init_db(&conn)?;

    let interval = Duration::from_millis(interval_ms);
    let mut last_window: Option<(String, String)> = None;
    let mut last_change = Utc::now();

    println!("Watching window focus (interval: {}ms)...", interval_ms);
    println!("Press Ctrl+C to stop\n");

    loop {
        match get_focused_window() {
            Ok(current) => {
                let now = Utc::now();

                if current != last_window {
                    if let Some((ref app_id, ref title)) = last_window {
                        let duration = (now - last_change).num_milliseconds();

                        conn.execute(
                            "INSERT INTO events (timestamp, app_id, title, duration_ms) VALUES (?1, ?2, ?3, ?4)",
                            params![last_change.to_rfc3339(), app_id, title, duration],
                        )?;

                        println!(
                            "[{}] {} - \"{}\" ({}ms)",
                            last_change.format("%H:%M:%S"),
                            app_id,
                            truncate(title, 50),
                            duration
                        );
                    }

                    last_window = current;
                    last_change = now;
                }
            }
            Err(e) => {
                eprintln!("Error getting focused window: {}", e);
            }
        }

        thread::sleep(interval);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

fn show_today() -> Result<(), Error> {
    let db_path = get_db_path();
    let conn = Connection::open(&db_path)?;

    let today = Utc::now().format("%Y-%m-%d").to_string();

    let mut stmt = conn.prepare(
        "SELECT app_id, SUM(duration_ms) as total_ms 
         FROM events 
         WHERE timestamp >= ?1 
         GROUP BY app_id 
         ORDER BY total_ms DESC",
    )?;

    let rows = stmt.query_map([&today], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    println!("Activity for today ({}):\n", today);
    println!("{:<30} {:>10}", "Application", "Time");
    println!("{}", "-".repeat(42));

    for row in rows {
        let (app_id, total_ms) = row?;
        let hours = total_ms / 3_600_000;
        let mins = (total_ms % 3_600_000) / 60_000;
        let secs = (total_ms % 60_000) / 1000;

        println!("{:<30} {:>2}h {:>2}m {:>2}s", app_id, hours, mins, secs);
    }

    Ok(())
}

fn show_summary(days: u32) -> Result<(), Error> {
    let db_path = get_db_path();
    let conn = Connection::open(&db_path)?;

    let since = (Utc::now() - chrono::Duration::days(days as i64))
        .format("%Y-%m-%d")
        .to_string();

    let mut stmt = conn.prepare(
        "SELECT DATE(timestamp) as day, app_id, SUM(duration_ms) as total_ms 
         FROM events 
         WHERE timestamp >= ?1 
         GROUP BY day, app_id 
         ORDER BY day DESC, total_ms DESC",
    )?;

    let rows = stmt.query_map([&since], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    let mut current_day = String::new();

    for row in rows {
        let (day, app_id, total_ms) = row?;

        if day != current_day {
            if !current_day.is_empty() {
                println!();
            }
            println!("=== {} ===", day);
            current_day = day;
        }

        let hours = total_ms / 3_600_000;
        let mins = (total_ms % 3_600_000) / 60_000;

        if hours > 0 || mins >= 1 {
            println!("  {:<28} {:>2}h {:>2}m", app_id, hours, mins);
        }
    }

    Ok(())
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Watch { interval } => watch(interval),
        Commands::Today => show_today(),
        Commands::Summary { days } => show_summary(days),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
