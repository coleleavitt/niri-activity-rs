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

use crate::error::Error;

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
        /// Number of days to show
        #[arg(short, long, default_value = "1")]
        days: u32,
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
        /// Number of days to cover
        #[arg(short, long, default_value = "1")]
        days: u32,
    },
    /// Export CSV matching ActivTrak column format
    Export {
        /// Number of days to export
        #[arg(short, long, default_value = "30")]
        days: u32,
    },
    /// Launch interactive TUI dashboard
    Tui {
        /// Number of days to cover
        #[arg(short, long, default_value = "7")]
        days: u32,
    },
    /// Initialize config file with examples
    Init,
}

fn main() {
    let cli = Cli::parse();

    let result: Result<(), Error> = match cli.command {
        None => tui::run_tui(7),
        Some(Commands::Tui { days }) => tui::run_tui(days),
        Some(Commands::Watch { quiet }) => watcher::watch(quiet),
        Some(Commands::Today) => report::App::open().and_then(|app| report::show_today(&app)),
        Some(Commands::Metrics { days }) => {
            report::App::open().and_then(|app| report::show_metrics(&app, days))
        }
        Some(Commands::Timeline { days, bucket }) => {
            report::App::open().and_then(|app| report::show_timeline(&app, days, bucket))
        }
        Some(Commands::Report { days }) => {
            report::App::open().and_then(|app| report::generate_report(&app, days))
        }
        Some(Commands::Export { days }) => {
            report::App::open().and_then(|app| report::export_csv(&app, days))
        }
        Some(Commands::Init) => config::init_config(),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
