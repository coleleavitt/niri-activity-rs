mod agent_activity;
mod config;
mod db;
mod email;
mod error;
mod fmt;
mod input;
mod logind;
mod report;
mod theme;
mod tui;
mod watcher;

use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::error::Error;

// Time constants
const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_MIN: i64 = 60_000;

/// Shared time-range arguments flattened into subcommands that need them.
#[derive(Debug, Clone, clap::Args)]
struct TimeRangeArgs {
    #[arg(long)]
    aligned: bool,
    #[arg(long)]
    today: bool,
    #[arg(long)]
    yesterday: bool,
    #[arg(long)]
    last_week: bool,
    #[arg(long)]
    this_week: bool,
    #[arg(long)]
    last_month: bool,
    #[arg(long)]
    this_month: bool,
    #[arg(long)]
    week: bool,
    #[arg(long)]
    month: bool,
    /// Start date (YYYY-MM-DD), use with --to
    #[arg(long)]
    from: Option<String>,
    /// End date (YYYY-MM-DD), use with --from
    #[arg(long)]
    to: Option<String>,
}

/// Output format for the export subcommand.
#[derive(Debug, Clone, clap::ValueEnum)]
enum ExportFormat {
    Csv,
    Xlsx,
    Json,
    Heatmap,
    Cron,
}

fn parse_time_range(days: u32, time: &TimeRangeArgs) -> Result<report::TimeRange, Error> {
    if let (Some(from_str), Some(to_str)) = (&time.from, &time.to) {
        let start = NaiveDate::parse_from_str(from_str, "%Y-%m-%d")
            .map_err(|e| Error::NiriError(format!("invalid --from date: {}", e)))?;
        let end = NaiveDate::parse_from_str(to_str, "%Y-%m-%d")
            .map_err(|e| Error::NiriError(format!("invalid --to date: {}", e)))?;
        if start > end {
            return Err(Error::NiriError(format!(
                "--from date ({}) must be on or before --to date ({})",
                from_str, to_str
            )));
        }
        return Ok(report::TimeRange::DateRange(start, end));
    }
    if time.from.is_some() || time.to.is_some() {
        return Err(Error::NiriError(
            "--from and --to must be used together".into(),
        ));
    }

    const MAX_DAYS: u32 = 10_000;
    if days > MAX_DAYS {
        return Err(Error::NiriError(format!(
            "--days {} exceeds maximum of {}",
            days, MAX_DAYS
        )));
    }

    Ok(if time.today {
        report::TimeRange::Days(0)
    } else if time.yesterday {
        report::TimeRange::Yesterday
    } else if time.last_week || time.week {
        report::TimeRange::LastWeek
    } else if time.this_week {
        report::TimeRange::ThisWeek
    } else if time.last_month || time.month {
        report::TimeRange::LastMonth
    } else if time.this_month {
        report::TimeRange::ThisMonth
    } else if time.aligned {
        if days == 0 {
            return Err(Error::NiriError("--aligned requires --days >= 1".into()));
        }
        report::TimeRange::DaysAligned(days)
    } else {
        report::TimeRange::Days(days)
    })
}

#[derive(Parser)]
#[command(name = "niri-activity-rs")]
#[command(about = "Track window focus on Niri compositor")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the watcher daemon
    Watch {
        /// Suppress per-event output (still logs untracked apps)
        #[arg(short, long)]
        quiet: bool,
    },
    /// Show today's activity
    Today,
    /// Show productivity metrics
    Metrics {
        #[arg(short, long, default_value = "1")]
        days: u32,
        #[command(flatten)]
        time: TimeRangeArgs,
    },
    /// Show activity timeline in 15-min buckets
    Timeline {
        /// Number of days back (0 = today)
        #[arg(short, long, default_value = "0")]
        days: u32,
        /// Bucket size in minutes
        #[arg(short, long, default_value = "15")]
        bucket: u32,
    },
    /// Generate a full activity report
    Report {
        #[arg(short, long, default_value = "1")]
        days: u32,
        #[command(flatten)]
        time: TimeRangeArgs,
        /// Compare current period with previous period of same length
        #[arg(long)]
        compare: bool,
    },
    /// Export data in CSV or Excel format
    Export {
        #[arg(short, long, default_value = "30")]
        days: u32,
        #[command(flatten)]
        time: TimeRangeArgs,
        /// Output format (csv, xlsx, json, heatmap, or cron)
        #[arg(short, long, default_value = "csv")]
        format: ExportFormat,
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Launch interactive TUI dashboard
    Tui {
        #[arg(short, long, default_value = "7")]
        days: u32,
        #[command(flatten)]
        time: TimeRangeArgs,
    },
    /// Initialize config file with examples
    Init,
    /// Fix false-active records in the database (reclassify active → passive
    /// for events with 0 keystrokes and 0 mouse clicks)
    FixFalseActive {
        /// Only show what would be fixed, don't modify the database
        #[arg(long)]
        dry_run: bool,
    },
    /// Reclassify active/passive/idle times using new thresholds (only for
    /// events with input_offsets)
    ReclassifyThresholds {
        /// Seconds of no input before Active → Passive (default: use config
        /// value)
        #[arg(long)]
        idle_threshold: Option<u64>,
        /// Seconds of no input before Passive → Idle (default: use config
        /// value)
        #[arg(long)]
        deep_idle: Option<u64>,
    },
    /// Send activity report via email
    Email {
        #[arg(short, long, default_value = "7")]
        days: u32,
        #[command(flatten)]
        time: TimeRangeArgs,
        /// Send weekly report (last Mon-Sun)
        #[arg(long)]
        weekly: bool,
        /// Send monthly report (last full month)
        #[arg(long)]
        monthly: bool,
        /// Test email configuration
        #[arg(long)]
        test: bool,
        /// Secure config file permissions (chmod 600)
        #[arg(long)]
        secure: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    // TUI mode takes over the terminal; disable tracing to stderr.
    // For CLI commands, enable tracing with env-filter (RUST_LOG=info default).
    let is_tui = matches!(cli.command, None | Some(Commands::Tui { .. }));
    if !is_tui {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_writer(std::io::stderr)
            .init();
    }

    // Load env file (SMTP creds etc.) before any threads spawn.
    if let Err(e) = config::load_env_file() {
        tracing::warn!("failed to load env file: {e}");
    }

    let result: Result<(), Error> = match cli.command {
        None => tui::run_tui_range(report::TimeRange::Days(7)),
        Some(Commands::Tui { days, time }) => {
            parse_time_range(days, &time).and_then(tui::run_tui_range)
        }
        Some(Commands::Watch { quiet }) => watcher::watch(quiet),
        Some(Commands::Today) => report::App::open().and_then(|app| report::show_today(&app)),
        Some(Commands::Metrics { days, time }) => parse_time_range(days, &time).and_then(|range| {
            report::App::open().and_then(|app| report::show_metrics_range(&app, range))
        }),
        Some(Commands::Timeline { days, bucket }) => {
            report::App::open().and_then(|app| report::show_timeline(&app, days, bucket))
        }
        Some(Commands::Report {
            days,
            time,
            compare,
        }) => parse_time_range(days, &time).and_then(|range| {
            report::App::open().and_then(|app| {
                if compare {
                    report::show_comparison(&app, range)
                } else {
                    report::generate_report_range(&app, range)
                }
            })
        }),
        Some(Commands::Export {
            days,
            time,
            format,
            output,
        }) => parse_time_range(days, &time).and_then(|range| {
            report::App::open().and_then(|app| match format {
                ExportFormat::Xlsx => {
                    let path = output.unwrap_or_else(|| "activity_report.xlsx".to_string());
                    report::export_xlsx_range(&app, range, &path)
                }
                ExportFormat::Json => report::export_json_range(&app, range),
                ExportFormat::Heatmap => report::export_heatmap_range(&app, range),
                ExportFormat::Cron => report::export_cron_summary(&app, range),
                ExportFormat::Csv => report::export_csv_range(&app, range),
            })
        }),
        Some(Commands::Init) => config::init_config(),
        Some(Commands::FixFalseActive { dry_run }) => (|| -> Result<(), Error> {
            let cfg = config::load_config()?;
            let data_dir = config::get_data_dir()?;
            let db_path = data_dir.join("activity.db");
            let conn = rusqlite::Connection::open(&db_path)?;
            let input_active_ms = cfg.input_active_secs.saturating_mul(1000);

            if dry_run {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM events
                          WHERE keystrokes = 0
                            AND mouse_clicks = 0
                            AND active_ms > ?1",
                    rusqlite::params![i64::try_from(input_active_ms).unwrap_or(i64::MAX)],
                    |row| row.get(0),
                )?;
                let total_ms: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(active_ms), 0) FROM events
                          WHERE keystrokes = 0
                            AND mouse_clicks = 0
                            AND active_ms > ?1",
                    rusqlite::params![i64::try_from(input_active_ms).unwrap_or(i64::MAX)],
                    |row| row.get(0),
                )?;
                let hours = total_ms / MS_PER_HOUR;
                let mins = (total_ms % MS_PER_HOUR) / MS_PER_MIN;
                println!(
                    "[dry-run] Would reclassify {} events ({} false-active → passive, {}h {}m total)",
                    count, count, hours, mins
                );
            } else {
                let fixed = db::fix_false_active(&conn, input_active_ms)?;
                println!(
                    "Fixed {} false-active events (active_ms → passive_ms)",
                    fixed
                );
            }
            Ok(())
        })(),
        Some(Commands::ReclassifyThresholds {
            idle_threshold,
            deep_idle,
        }) => (|| -> Result<(), Error> {
            let cfg = config::load_config()?;
            let idle_secs = idle_threshold.unwrap_or(cfg.idle_threshold_secs);
            let deep_secs = deep_idle.unwrap_or(cfg.deep_idle_secs);

            let data_dir = config::get_data_dir()?;
            let db_path = data_dir.join("activity.db");
            let mut conn = rusqlite::Connection::open(&db_path)?;
            db::run_migrations(&mut conn, &cfg)?;

            let (updated, total) = db::reclassify_with_thresholds(&mut conn, idle_secs, deep_secs)?;
            println!(
                "Reclassified {}/{} events with input_offsets (idle={}s, deep_idle={}s)",
                updated, total, idle_secs, deep_secs
            );
            if total == 0 {
                println!(
                    "Note: No events have input_offsets stored yet. Only future events will support retroactive reclassification."
                );
            }
            Ok(())
        })(),
        Some(Commands::Email {
            days,
            time,
            weekly,
            monthly,
            test,
            secure,
        }) => (|| -> Result<(), Error> {
            if secure {
                let config_path = config::get_config_path()?;
                email::secure_config_permissions(&config_path)?;
            }
            if test {
                let cfg = config::load_config()?;
                email::test_email_config(&cfg)?;
            } else if time.from.is_some() || time.to.is_some() {
                // Custom date range takes precedence
                let range = parse_time_range(days, &time)?;
                let period_name = if let (Some(from), Some(to)) = (&time.from, &time.to) {
                    format!("Custom ({} to {})", from, to)
                } else {
                    "Custom".to_string()
                };
                report::App::open()
                    .and_then(|app| email::send_report(&app, range, &period_name))?;
            } else if weekly {
                report::App::open().and_then(|app| email::send_weekly_report(&app))?;
            } else if monthly {
                report::App::open().and_then(|app| email::send_monthly_report(&app))?;
            } else if !secure {
                return Err(Error::NiriError(
                    "Specify --weekly, --monthly, --from/--to, --test, or --secure".into(),
                ));
            }
            Ok(())
        })(),
    };

    if let Err(e) = result {
        // Use eprintln! instead of tracing::error! because TUI mode
        // doesn't initialize a tracing subscriber, making tracing a no-op
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
