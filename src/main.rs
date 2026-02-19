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

use crate::error::Error;

fn parse_time_range(
    days: u32,
    aligned: bool,
    today: bool,
    yesterday: bool,
    last_week: bool,
    this_week: bool,
    last_month: bool,
    this_month: bool,
    week: bool,
    month: bool,
    from: Option<String>,
    to: Option<String>,
) -> Result<report::TimeRange, Error> {
    if let (Some(from_str), Some(to_str)) = (&from, &to) {
        let start = NaiveDate::parse_from_str(from_str, "%Y-%m-%d")
            .map_err(|e| Error::NiriError(format!("invalid --from date: {}", e)))?;
        let end = NaiveDate::parse_from_str(to_str, "%Y-%m-%d")
            .map_err(|e| Error::NiriError(format!("invalid --to date: {}", e)))?;
        return Ok(report::TimeRange::DateRange(start, end));
    }
    if from.is_some() || to.is_some() {
        return Err(Error::NiriError(
            "--from and --to must be used together".into(),
        ));
    }

    Ok(if today {
        report::TimeRange::Days(0)
    } else if yesterday {
        report::TimeRange::Yesterday
    } else if last_week || week {
        report::TimeRange::LastWeek
    } else if this_week {
        report::TimeRange::ThisWeek
    } else if last_month || month {
        report::TimeRange::LastMonth
    } else if this_month {
        report::TimeRange::ThisMonth
    } else if aligned {
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
        /// Compare current period with previous period of same length
        #[arg(long)]
        compare: bool,
    },
    /// Export data in CSV or Excel format
    Export {
        #[arg(short, long, default_value = "30")]
        days: u32,
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
        /// Output format (csv, xlsx, json, heatmap, or cron)
        #[arg(short, long, default_value = "csv")]
        format: String,
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Launch interactive TUI dashboard
    Tui {
        #[arg(short, long, default_value = "7")]
        days: u32,
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
    /// Send activity report via email
    Email {
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

    let result: Result<(), Error> = match cli.command {
        None => tui::run_tui_range(report::TimeRange::Days(7)),
        Some(Commands::Tui {
            days,
            aligned,
            today,
            yesterday,
            last_week,
            this_week,
            last_month,
            this_month,
            week,
            month,
            from,
            to,
        }) => parse_time_range(
            days, aligned, today, yesterday, last_week, this_week, last_month, this_month, week,
            month, from, to,
        )
        .and_then(tui::run_tui_range),
        Some(Commands::Watch { quiet }) => watcher::watch(quiet),
        Some(Commands::Today) => report::App::open().and_then(|app| report::show_today(&app)),
        Some(Commands::Metrics {
            days,
            aligned,
            today,
            yesterday,
            last_week,
            this_week,
            last_month,
            this_month,
            week,
            month,
            from,
            to,
        }) => parse_time_range(
            days, aligned, today, yesterday, last_week, this_week, last_month, this_month, week,
            month, from, to,
        )
        .and_then(|range| {
            report::App::open().and_then(|app| report::show_metrics_range(&app, range))
        }),
        Some(Commands::Timeline { days, bucket }) => {
            report::App::open().and_then(|app| report::show_timeline(&app, days, bucket))
        }
        Some(Commands::Report {
            days,
            aligned,
            today,
            yesterday,
            last_week,
            this_week,
            last_month,
            this_month,
            week,
            month,
            from,
            to,
            compare,
        }) => parse_time_range(
            days, aligned, today, yesterday, last_week, this_week, last_month, this_month, week,
            month, from, to,
        )
        .and_then(|range| {
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
            aligned,
            today,
            yesterday,
            last_week,
            this_week,
            last_month,
            this_month,
            week,
            month,
            from,
            to,
            format,
            output,
        }) => parse_time_range(
            days, aligned, today, yesterday, last_week, this_week, last_month, this_month, week,
            month, from, to,
        )
        .and_then(|range| {
            report::App::open().and_then(|app| match format.as_str() {
                "xlsx" | "excel" => {
                    let path = output.unwrap_or_else(|| "activity_report.xlsx".to_string());
                    report::export_xlsx_range(&app, range, &path)
                }
                "json" => report::export_json_range(&app, range),
                "heatmap" => report::export_heatmap_range(&app, range),
                "cron" | "summary" => report::export_cron_summary(&app, range),
                _ => report::export_csv_range(&app, range),
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
                    rusqlite::params![input_active_ms as i64],
                    |row| row.get(0),
                )?;
                let total_ms: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(active_ms), 0) FROM events
                          WHERE keystrokes = 0
                            AND mouse_clicks = 0
                            AND active_ms > ?1",
                    rusqlite::params![input_active_ms as i64],
                    |row| row.get(0),
                )?;
                let hours = total_ms / 3_600_000;
                let mins = (total_ms % 3_600_000) / 60_000;
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
        Some(Commands::Email {
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
            } else if weekly {
                report::App::open().and_then(|app| email::send_weekly_report(&app))?;
            } else if monthly {
                report::App::open().and_then(|app| email::send_monthly_report(&app))?;
            } else if !secure {
                return Err(Error::NiriError(
                    "Specify --weekly, --monthly, --test, or --secure".into(),
                ));
            }
            Ok(())
        })(),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
