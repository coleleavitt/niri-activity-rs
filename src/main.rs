mod config;
mod db;
mod error;
mod fmt;
mod input;
mod logind;
mod report;
mod theme;
mod tui;
mod watcher;

use clap::{Parser, Subcommand};
use chrono::NaiveDate;

use crate::error::Error;

fn parse_time_range(
    days: u32,
    aligned: bool,
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
        return Err(Error::NiriError("--from and --to must be used together".into()));
    }
    
    Ok(if yesterday {
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
}

fn main() {
    let cli = Cli::parse();

    let result: Result<(), Error> = match cli.command {
        None => tui::run_tui_range(report::TimeRange::Days(7)),
        Some(Commands::Tui {
            days, aligned, yesterday, last_week, this_week,
            last_month, this_month, week, month, from, to,
        }) => {
            parse_time_range(days, aligned, yesterday, last_week, this_week, last_month, this_month, week, month, from, to)
                .and_then(tui::run_tui_range)
        }
        Some(Commands::Watch { quiet }) => watcher::watch(quiet),
        Some(Commands::Today) => report::App::open().and_then(|app| report::show_today(&app)),
        Some(Commands::Metrics {
            days, aligned, yesterday, last_week, this_week,
            last_month, this_month, week, month, from, to,
        }) => {
            parse_time_range(days, aligned, yesterday, last_week, this_week, last_month, this_month, week, month, from, to)
                .and_then(|range| report::App::open().and_then(|app| report::show_metrics_range(&app, range)))
        }
        Some(Commands::Timeline { days, bucket }) => {
            report::App::open().and_then(|app| report::show_timeline(&app, days, bucket))
        }
        Some(Commands::Report {
            days, aligned, yesterday, last_week, this_week,
            last_month, this_month, week, month, from, to, compare,
        }) => {
            parse_time_range(days, aligned, yesterday, last_week, this_week, last_month, this_month, week, month, from, to)
                .and_then(|range| report::App::open().and_then(|app| {
                    if compare {
                        report::show_comparison(&app, range)
                    } else {
                        report::generate_report_range(&app, range)
                    }
                }))
        }
        Some(Commands::Export {
            days, aligned, yesterday, last_week, this_week,
            last_month, this_month, week, month, from, to, format, output,
        }) => {
            parse_time_range(days, aligned, yesterday, last_week, this_week, last_month, this_month, week, month, from, to)
                .and_then(|range| report::App::open().and_then(|app| match format.as_str() {
                    "xlsx" | "excel" => {
                        let path = output.unwrap_or_else(|| "activity_report.xlsx".to_string());
                        report::export_xlsx_range(&app, range, &path)
                    }
                    "json" => report::export_json_range(&app, range),
                    "heatmap" => report::export_heatmap_range(&app, range),
                    "cron" | "summary" => report::export_cron_summary(&app, range),
                    _ => report::export_csv_range(&app, range),
                }))
        }
        Some(Commands::Init) => config::init_config(),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
