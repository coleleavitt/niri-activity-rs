//! Export functions for CSV, JSON, and heatmap output.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use chrono::Timelike;
use serde::Serialize;

use super::interval::{EventInterval, load_human_intervals};
use super::query::query_report_range;
use super::{App, TimeRange, UNTIL_SENTINEL, day_end_utc, day_start_utc};
use crate::config::{Category, Config};
use crate::error::Error;
use crate::fmt::fmt_hms;

/// Render an export to the file `--output` named, or to stdout without one.
///
/// Every exporter used to print unconditionally, so `--output report.csv`
/// wrote the report to the terminal, created no file and exited 0 — a
/// success code for work that did not happen. Routing all of them through
/// one sink makes the file the writer's concern instead of each exporter's,
/// so no format can quietly lose the flag again.
fn write_export(
    output: Option<&Path>,
    render: impl FnOnce(&mut dyn Write) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut sink: Box<dyn Write> = match output {
        Some(path) => Box::new(BufWriter::new(File::create(path)?)),
        None => Box::new(std::io::stdout().lock()),
    };
    let rendered = render(&mut sink);
    // A buffered file reports a short or failed write only when it is
    // flushed, and dropping the writer discards that error, which would
    // report a truncated export as a success.
    let flushed = rendered.and_then(|()| Ok(sink.flush()?));
    match flushed {
        // `export | head` closes the pipe once the reader has what it asked
        // for. That is the reader finishing, not this command failing, so it
        // must not become a non-zero exit like a real write error does.
        Err(error) if is_broken_pipe(&error) && output.is_none() => Ok(()),
        other => other,
    }
}

/// Whether an export failed only because its reader stopped reading.
fn is_broken_pipe(error: &Error) -> bool {
    matches!(error, Error::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe)
}

/// Export activity data as CSV for a date range.
pub fn export_csv_range(app: &App, range: TimeRange, output: Option<&Path>) -> Result<(), Error> {
    write_export(output, |out| write_csv_range(app, range, out))
}

fn write_csv_range(app: &App, range: TimeRange, out: &mut dyn Write) -> Result<(), Error> {
    let bounds = range.resolve(&app.config)?;
    writeln!(
        out,
        "Date,Screen Time (h:mm:ss),Productive (h:mm:ss),Unproductive (h:mm:ss),Undefined (h:mm:ss),ProdActive (h:mm:ss),ProdPassive (h:mm:ss),Productive Ratio,Productive Active %"
    )?;
    let mut date = bounds.start_date;
    let mut iterations = 0u32;
    while date <= bounds.end_date {
        iterations += 1;
        if iterations > 10_000 {
            return Err(Error::InvalidArgument(
                "date range exceeds 10,000 day limit".into(),
            ));
        }
        let start = day_start_utc(&app.config, date)?;
        let end = day_end_utc(&app.config, date)?;
        let m = super::query::metrics_between(&app.conn, &app.config, &start, &end)?;
        let prod_ratio = if m.total_ms > 0 {
            format!("{:.1}%", m.productive_ms as f64 / m.total_ms as f64 * 100.0)
        } else {
            "0.0%".to_string()
        };
        let prod_active_pct = if m.productive_ms > 0 {
            format!(
                "{:.1}%",
                m.productive_active_ms as f64 / m.productive_ms as f64 * 100.0
            )
        } else {
            "0.0%".to_string()
        };
        writeln!(
            out,
            "{},{},{},{},{},{},{},{},{}",
            date,
            fmt_hms(m.total_ms),
            fmt_hms(m.productive_ms),
            fmt_hms(m.unproductive_ms),
            fmt_hms(m.neutral_ms),
            fmt_hms(m.productive_active_ms),
            fmt_hms(m.productive_passive_ms),
            prod_ratio,
            prod_active_pct
        )?;
        date += chrono::Duration::days(1);
    }
    Ok(())
}

/// Export activity data as JSON for a date range.
pub fn export_json_range(app: &App, range: TimeRange, output: Option<&Path>) -> Result<(), Error> {
    write_export(output, |out| {
        let data = query_report_range(app, range)?;
        let json = serde_json::to_string_pretty(&data)
            .map_err(|e| Error::NiriError(format!("JSON serialization failed: {}", e)))?;
        writeln!(out, "{}", json)?;
        Ok(())
    })
}

#[derive(Serialize)]
struct HeatmapCell {
    date: String,
    hour: u32,
    productive_ms: i64,
    unproductive_ms: i64,
    neutral_ms: i64,
    total_ms: i64,
    keystrokes: i64,
}

/// Export a cron-friendly summary of productivity metrics for a date range.
pub fn export_cron_summary(
    app: &App,
    range: TimeRange,
    output: Option<&Path>,
) -> Result<(), Error> {
    write_export(output, |out| write_cron_summary(app, range, out))
}

fn write_cron_summary(app: &App, range: TimeRange, out: &mut dyn Write) -> Result<(), Error> {
    let data = query_report_range(app, range)?;
    let productive_ms = data
        .categories
        .iter()
        .find(|c| c.category == Category::Productive)
        .map_or(0, |c| c.total_ms);
    let unproductive_ms = data
        .categories
        .iter()
        .find(|c| c.category == Category::Unproductive)
        .map_or(0, |c| c.total_ms);
    let ratio = if data.total_ms > 0 {
        (productive_ms as f64 / data.total_ms as f64 * 100.0).round() as i64
    } else {
        0
    };
    let top_app = data.top_apps.first().map_or("-", |a| a.app_id.as_str());
    writeln!(
        out,
        "{}|{}|{}|{}|{}%|{}",
        data.since_str
            .split_whitespace()
            .next()
            .unwrap_or(&data.since_str),
        fmt_hms(data.total_ms),
        fmt_hms(productive_ms),
        fmt_hms(unproductive_ms),
        ratio,
        top_app
    )?;
    Ok(())
}

/// Add one minute slice to the heatmap cell that contains it.
///
/// The duration is idle-inclusive because every other report surface counts a
/// category's idle time inside that category; excluding it here would make the
/// heatmap shrink during exactly the stretches it is meant to expose.
fn accumulate_slice(cell: &mut HeatmapCell, config: &Config, slice: &EventInterval) {
    let total_ms = slice.total_ms();
    let credited = super::query::agent_credit(
        config,
        slice.category,
        slice.agent_ms.unwrap_or(0),
        total_ms,
    );
    let own_ms = total_ms.saturating_sub(credited);
    match slice.category {
        Category::Productive => {
            cell.productive_ms = cell.productive_ms.saturating_add(own_ms);
        }
        Category::Unproductive => {
            cell.unproductive_ms = cell.unproductive_ms.saturating_add(own_ms);
        }
        Category::Neutral => {
            cell.neutral_ms = cell.neutral_ms.saturating_add(own_ms);
        }
    }
    cell.productive_ms = cell.productive_ms.saturating_add(credited);
    cell.total_ms = cell.total_ms.saturating_add(total_ms);
    cell.keystrokes = cell.keystrokes.saturating_add(slice.keystrokes);
}

/// Export hourly activity heatmap as JSON for a date range.
pub fn export_heatmap_range(
    app: &App,
    range: TimeRange,
    output: Option<&Path>,
) -> Result<(), Error> {
    write_export(output, |out| write_heatmap_range(app, range, out))
}

fn write_heatmap_range(app: &App, range: TimeRange, out: &mut dyn Write) -> Result<(), Error> {
    let bounds = range.resolve(&app.config)?;
    let until_utc = bounds.until_utc.as_deref().unwrap_or(UNTIL_SENTINEL);
    let mut heatmap: std::collections::BTreeMap<(String, u32), HeatmapCell> =
        std::collections::BTreeMap::new();
    for event in load_human_intervals(&app.conn, &app.config, &bounds.since_utc, until_utc)? {
        for slice in event.minute_slices() {
            let Some(timestamp) = slice.local_start(&app.config) else {
                continue;
            };
            let date = timestamp.format("%Y-%m-%d").to_string();
            let hour = timestamp.hour();
            let cell = heatmap
                .entry((date.clone(), hour))
                .or_insert_with(|| HeatmapCell {
                    date,
                    hour,
                    productive_ms: 0,
                    unproductive_ms: 0,
                    neutral_ms: 0,
                    total_ms: 0,
                    keystrokes: 0,
                });
            accumulate_slice(cell, &app.config, &slice);
        }
    }
    let json = serde_json::to_string_pretty(&heatmap.into_values().collect::<Vec<_>>())
        .map_err(|e| Error::NiriError(format!("JSON serialization failed: {}", e)))?;
    writeln!(out, "{}", json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        HeatmapCell, accumulate_slice, export_csv_range, export_heatmap_range, export_json_range,
        write_export,
    };
    use crate::config::{Category, Config};
    use crate::db::{init_db, run_migrations};
    use crate::report::interval::EventInterval;
    use crate::report::interval::input::GranularInput;
    use crate::report::{App, TimeRange};

    /// One recorded day, in a fixed zone so the exported date never depends
    /// on the machine running the test.
    fn app_with_one_day() -> App {
        let mut conn = rusqlite::Connection::open_in_memory().expect("database");
        init_db(&conn).expect("schema");
        run_migrations(&mut conn, &Config::default()).expect("migrations");
        conn.execute(
            "INSERT INTO events (
                 timestamp, app_id, title, category, active_ms, passive_ms, idle_ms
             ) VALUES (
                 '2026-01-02T10:00:00+00:00', 'foot', 'build',
                 'productive', 60000, 0, 0
             )",
            [],
        )
        .expect("event");
        let config = Config {
            timezone: Some(chrono_tz::UTC),
            ..Config::default()
        };
        App { config, conn }
    }

    fn one_day() -> TimeRange {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).expect("valid date");
        TimeRange::DateRange(day, day)
    }

    #[test]
    fn a_csv_export_writes_the_file_output_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("report.csv");

        export_csv_range(&app_with_one_day(), one_day(), Some(&path)).expect("csv export");

        let written = std::fs::read_to_string(&path).expect("exported file");
        assert!(written.starts_with("Date,Screen Time"), "{written}");
        assert!(written.contains("2026-01-02"), "{written}");
    }

    #[test]
    fn a_json_export_writes_the_file_output_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("report.json");

        export_json_range(&app_with_one_day(), one_day(), Some(&path)).expect("json export");

        let written = std::fs::read_to_string(&path).expect("exported file");
        serde_json::from_str::<serde_json::Value>(&written).expect("valid JSON document");
    }

    #[test]
    fn a_heatmap_export_writes_the_file_output_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("heatmap.json");

        export_heatmap_range(&app_with_one_day(), one_day(), Some(&path)).expect("heatmap export");

        let written = std::fs::read_to_string(&path).expect("exported file");
        assert!(written.contains("\"2026-01-02\""), "{written}");
    }

    #[test]
    fn an_unwritable_output_path_fails_instead_of_printing() {
        let error = export_csv_range(
            &app_with_one_day(),
            one_day(),
            Some(Path::new("/nonexistent-directory-for-tests/report.csv")),
        )
        .expect_err("a file that cannot be created must not report success");

        assert!(matches!(error, crate::error::Error::Io(_)), "{error}");
    }

    #[test]
    fn a_closed_reader_ends_a_stdout_export_without_an_error() {
        // `export | head -2` closes the pipe as soon as the reader has what it
        // asked for. A file write that fails this way is a real failure; a
        // stdout write that fails this way is the reader leaving.
        let broken = || {
            Err(crate::error::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "reader closed the pipe",
            )))
        };

        write_export(None, |_| broken()).expect("a closed stdout reader is not an export failure");

        let file = tempfile::NamedTempFile::new().expect("temp file");
        write_export(Some(file.path()), |_| broken())
            .expect_err("a broken file write must still fail");
    }

    fn cell() -> HeatmapCell {
        HeatmapCell {
            date: "2026-01-02".to_string(),
            hour: 9,
            productive_ms: 0,
            unproductive_ms: 0,
            neutral_ms: 0,
            total_ms: 0,
            keystrokes: 0,
        }
    }

    fn slice(category: Category, active_ms: i64, idle_ms: i64, agent_ms: i64) -> EventInterval {
        EventInterval {
            source_start_ms: 0,
            start_ms: 0,
            app_id: "foot".to_string(),
            title: String::new(),
            category,
            project: None,
            active_ms,
            passive_ms: 0,
            idle_ms,
            agent_ms: Some(agent_ms),
            keystrokes: 0,
            mouse_clicks: 0,
            scroll_events: 0,
            mouse_distance: 0,
            granular: GranularInput::default(),
            jiggler_detected: false,
        }
    }

    #[test]
    fn heatmap_cells_keep_the_idle_time_the_report_counts() {
        let mut cell = cell();

        accumulate_slice(
            &mut cell,
            &Config::default(),
            &slice(Category::Productive, 0, 60_000, 0),
        );

        assert_eq!(cell.total_ms, 60_000);
        assert_eq!(cell.productive_ms, 60_000);
    }

    #[test]
    fn agent_overlapped_time_is_credited_to_productive_like_the_report() {
        let mut config = Config::default();
        config.agent_activity.counts_as_productive = true;
        let mut cell = cell();

        accumulate_slice(
            &mut cell,
            &config,
            &slice(Category::Neutral, 60_000, 0, 20_000),
        );

        assert_eq!(cell.neutral_ms, 40_000);
        assert_eq!(cell.productive_ms, 20_000);
        assert_eq!(cell.total_ms, 60_000);
    }

    #[test]
    fn agent_time_stays_with_its_category_when_credit_is_disabled() {
        let mut config = Config::default();
        config.agent_activity.counts_as_productive = false;
        let mut cell = cell();

        accumulate_slice(
            &mut cell,
            &config,
            &slice(Category::Neutral, 60_000, 0, 20_000),
        );

        assert_eq!(cell.neutral_ms, 60_000);
        assert_eq!(cell.productive_ms, 0);
    }
}
