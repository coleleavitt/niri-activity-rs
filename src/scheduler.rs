use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::config::{Config, get_data_dir};
use crate::email;
use crate::error::Error;
use crate::report::{App, TimeRange};

const MAX_WEEKLY: u32 = 8;
const MAX_MONTHLY: u32 = 3;
// A long lease avoids a second process retrying while a slow SMTP exchange is
// still in progress. An expired lease is retried: this gives crash recovery,
// but can duplicate mail if the process died after the SMTP server accepted it
// and before `sent` was committed. SMTP has no portable idempotency primitive;
// strict exactly-once delivery needs provider support keyed by period/job ID.
const CLAIM_LEASE: Duration = Duration::from_secs(24 * 60 * 60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct Job {
    period_type: &'static str,
    period_key: String,
    start: NaiveDate,
    end: NaiveDate,
}

/// Non-blocking handle used by the watcher. Capacity one deliberately coalesces
/// repeated timer ticks while the worker is busy generating or sending mail.
pub struct Scheduler {
    tx: Option<SyncSender<()>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    worker_done: Receiver<()>,
}

impl Scheduler {
    pub fn start(quiet: bool) -> Result<Self, Error> {
        let db_path = get_data_dir()?.join("activity.db");
        let (tx, rx) = mpsc::sync_channel(1);
        let (done_tx, worker_done) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("report-scheduler".into())
            .spawn(move || {
                while rx.recv().is_ok() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Err(error) = run_worker_once(&db_path, quiet, &worker_stop) {
                        tracing::error!("[SCHEDULER] Worker failed: {error}");
                    }
                }
                let _ = done_tx.send(());
            })
            .map_err(|error| {
                Error::NiriError(format!("Failed to start report scheduler: {error}"))
            })?;
        Ok(Self {
            tx: Some(tx),
            stop,
            worker: Some(worker),
            worker_done,
        })
    }

    /// Queue a scan without waiting. A full queue means a scan is already due.
    pub fn check(&self) {
        let Some(tx) = &self.tx else {
            return;
        };
        match tx.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => {}
            Err(mpsc::TrySendError::Disconnected(())) => {
                tracing::error!("[SCHEDULER] Worker stopped; scheduled reports are disabled");
            }
        }
    }

    /// Stop accepting scans and wait a bounded time for in-flight work.
    pub fn shutdown(&mut self) {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) {
        if self.worker.is_none() {
            return;
        }
        self.stop.store(true, Ordering::Release);
        self.tx.take();
        if self.worker_done.recv_timeout(timeout).is_ok() {
            if let Some(worker) = self.worker.take() {
                if worker.join().is_err() {
                    tracing::error!("[SCHEDULER] Worker panicked during shutdown");
                }
            }
        } else {
            tracing::warn!("[SCHEDULER] Worker did not stop within {timeout:?}; detaching");
            self.worker.take();
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn current_week_saturday(today: NaiveDate) -> NaiveDate {
    let days_since_saturday = (today.weekday().num_days_from_monday() as i64 + 2) % 7;
    today - chrono::Duration::days(days_since_saturday)
}

fn candidate_jobs(today: NaiveDate) -> Vec<Job> {
    let this_saturday = current_week_saturday(today);
    let mut jobs: Vec<Job> = (1..=MAX_WEEKLY)
        .rev()
        .map(|i| {
            let start = this_saturday - chrono::Duration::weeks(i64::from(i));
            Job {
                period_type: "weekly",
                period_key: start.format("%Y-%m-%d").to_string(),
                start,
                end: start + chrono::Duration::days(6),
            }
        })
        .filter(|job| today > job.end)
        .collect();
    jobs.extend((1..=MAX_MONTHLY).rev().filter_map(|i| {
        let start = today.with_day(1)?.checked_sub_months(Months::new(i))?;
        let end = start.checked_add_months(Months::new(1))? - chrono::Duration::days(1);
        (today > end).then(|| Job {
            period_type: "monthly",
            period_key: start.format("%Y-%m").to_string(),
            start,
            end,
        })
    }));
    jobs
}

fn ensure_jobs(conn: &Connection, jobs: &[Job]) -> Result<(), Error> {
    for job in jobs {
        conn.execute(
            "INSERT OR IGNORE INTO scheduled_report_jobs
             (period_type, period_key, range_start, range_end, state)
             VALUES (?1, ?2, ?3, ?4, 'pending')",
            params![
                job.period_type,
                job.period_key,
                job.start.to_string(),
                job.end.to_string()
            ],
        )?;
    }
    Ok(())
}

fn claim_job(
    conn: &mut Connection,
    job: &Job,
    owner: &str,
    now: DateTime<Utc>,
    lease: Duration,
) -> Result<bool, Error> {
    let lease = chrono::Duration::from_std(lease)
        .map_err(|error| Error::NiriError(format!("Invalid scheduler lease: {error}")))?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let claimed = tx
        .query_row(
            "UPDATE scheduled_report_jobs
             SET state = 'claimed', owner = ?3, claimed_at = ?4,
                 lease_expires_at = ?5, attempt_count = attempt_count + 1,
                 last_attempt_at = ?4, last_error = NULL
             WHERE period_type = ?1 AND period_key = ?2
               AND (state = 'pending'
                    OR (state = 'claimed' AND lease_expires_at <= ?4))
             RETURNING 1",
            params![
                job.period_type,
                job.period_key,
                owner,
                now.to_rfc3339(),
                (now + lease).to_rfc3339(),
            ],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    tx.commit()?;
    Ok(claimed)
}

fn release_claim(conn: &Connection, job: &Job, owner: &str) -> Result<(), Error> {
    conn.execute(
        "UPDATE scheduled_report_jobs
         SET state = 'pending', owner = NULL, claimed_at = NULL, lease_expires_at = NULL
         WHERE period_type = ?1 AND period_key = ?2 AND state = 'claimed' AND owner = ?3",
        params![job.period_type, job.period_key, owner],
    )?;
    Ok(())
}

fn claim_job_unless_stopping(
    conn: &mut Connection,
    job: &Job,
    owner: &str,
    now: DateTime<Utc>,
    lease: Duration,
    stop: &AtomicBool,
) -> Result<bool, Error> {
    if stop.load(Ordering::Acquire) {
        return Ok(false);
    }
    if !claim_job(conn, job, owner, now, lease)? {
        return Ok(false);
    }
    if stop.load(Ordering::Acquire) {
        release_claim(conn, job, owner)?;
        return Ok(false);
    }
    Ok(true)
}

fn finalize_sent(
    conn: &mut Connection,
    job: &Job,
    owner: &str,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    let sent_at = now.to_rfc3339();
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE scheduled_report_jobs
         SET state = 'sent', sent_at = ?4, lease_expires_at = NULL, last_error = NULL
         WHERE period_type = ?1 AND period_key = ?2 AND state = 'claimed' AND owner = ?3",
        params![job.period_type, job.period_key, owner, sent_at],
    )?;
    if changed != 1 {
        return Err(Error::NiriError(format!(
            "Lost scheduler claim for {}:{} before finalization",
            job.period_type, job.period_key
        )));
    }
    tx.execute(
        "INSERT INTO sent_reports (period_type, period_key, sent_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(period_type, period_key) DO UPDATE SET sent_at = excluded.sent_at",
        params![job.period_type, job.period_key, sent_at],
    )?;
    tx.commit()?;
    Ok(())
}

fn record_failure(conn: &Connection, job: &Job, owner: &str, error: &Error) -> Result<(), Error> {
    // Keep the claim until its lease expires. An error can occur after an SMTP
    // server accepted the message, so immediate retry would amplify duplicates.
    conn.execute(
        "UPDATE scheduled_report_jobs SET last_error = ?4
         WHERE period_type = ?1 AND period_key = ?2 AND state = 'claimed' AND owner = ?3",
        params![job.period_type, job.period_key, owner, error.to_string()],
    )?;
    Ok(())
}

fn run_worker_once(db_path: &PathBuf, quiet: bool, stop: &AtomicBool) -> Result<(), Error> {
    let config = crate::config::load_config()?;
    if !config.email.enabled {
        return Ok(());
    }
    let today = config.local_now().date_naive();
    let jobs = candidate_jobs(today);
    let mut conn = Connection::open(db_path)?;
    ensure_jobs(&conn, &jobs)?;
    let owner = format!(
        "{}:{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );

    for job in jobs {
        if stop.load(Ordering::Acquire) {
            break;
        }
        if !claim_job_unless_stopping(&mut conn, &job, &owner, Utc::now(), CLAIM_LEASE, stop)? {
            continue;
        }
        let label = format!(
            "{} ({} to {})",
            capitalize(job.period_type),
            job.start,
            job.end
        );
        if !quiet {
            tracing::info!("[SCHEDULER] Sending {label}");
        }
        match send_report(job.start, job.end, &label) {
            Ok(()) => finalize_sent(&mut conn, &job, &owner, Utc::now())?,
            Err(error) => {
                record_failure(&conn, &job, &owner, &error)?;
                tracing::error!("[SCHEDULER] Failed to send {label}: {error}");
            }
        }
    }
    Ok(())
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

fn send_report(start: NaiveDate, end: NaiveDate, period_name: &str) -> Result<(), Error> {
    let app = App::open()?;
    email::send_report(&app, TimeRange::DateRange(start, end), period_name)
}

/// Compatibility entry point for non-watcher callers. This only queues work.
pub fn check_scheduled_reports(scheduler: &Scheduler, config: &Config) {
    if config.email.enabled {
        scheduler.check();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use super::*;

    fn schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE scheduled_report_jobs (
                period_type TEXT NOT NULL, period_key TEXT NOT NULL,
                range_start TEXT NOT NULL, range_end TEXT NOT NULL,
                state TEXT NOT NULL, owner TEXT, claimed_at TEXT,
                lease_expires_at TEXT, attempt_count INTEGER NOT NULL DEFAULT 0,
                last_attempt_at TEXT, sent_at TEXT, last_error TEXT,
                PRIMARY KEY(period_type, period_key));
             CREATE TABLE sent_reports (
                id INTEGER PRIMARY KEY,
                period_type TEXT NOT NULL, period_key TEXT NOT NULL,
                sent_at TEXT NOT NULL,
                UNIQUE(period_type, period_key));",
        )
        .unwrap();
    }

    fn job() -> Job {
        Job {
            period_type: "weekly",
            period_key: "2026-04-04".into(),
            start: NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
            end: NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(),
        }
    }

    #[test]
    fn saturday_alignment() {
        let sunday = NaiveDate::from_ymd_opt(2026, 4, 5).unwrap();
        assert_eq!(
            current_week_saturday(sunday),
            NaiveDate::from_ymd_opt(2026, 4, 4).unwrap()
        );
    }

    #[test]
    fn bounded_queue_never_blocks_caller_and_coalesces_ticks() {
        let (tx, rx) = mpsc::sync_channel(1);
        tx.try_send(()).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            assert!(matches!(tx.try_send(()), Err(mpsc::TrySendError::Full(()))));
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn blocking_fake_sender_does_not_block_scheduler_caller() {
        let (tx, rx) = mpsc::sync_channel(1);
        let gate = Arc::new(Barrier::new(2));
        let worker_gate = Arc::clone(&gate);
        let worker = thread::spawn(move || {
            rx.recv().unwrap();
            worker_gate.wait();
            worker_gate.wait();
        });
        tx.try_send(()).unwrap();
        gate.wait();
        let started = std::time::Instant::now();
        assert!(matches!(
            tx.try_send(()),
            Ok(()) | Err(mpsc::TrySendError::Full(()))
        ));
        assert!(started.elapsed() < Duration::from_millis(100));
        gate.wait();
        drop(tx);
        worker.join().unwrap();
    }

    #[test]
    fn repeated_discovery_creates_one_durable_job() {
        let conn = Connection::open_in_memory().unwrap();
        schema(&conn);
        let job = job();
        ensure_jobs(&conn, std::slice::from_ref(&job)).unwrap();
        ensure_jobs(&conn, std::slice::from_ref(&job)).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scheduled_report_jobs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn only_one_concurrent_owner_claims_job() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(file.path()).unwrap();
        schema(&conn);
        ensure_jobs(&conn, &[job()]).unwrap();
        drop(conn);
        let barrier = Arc::new(Barrier::new(3));
        let won = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for owner in ["one", "two"] {
            let path = file.path().to_owned();
            let barrier = Arc::clone(&barrier);
            let won = Arc::clone(&won);
            threads.push(thread::spawn(move || {
                let mut conn = Connection::open(path).unwrap();
                conn.busy_timeout(Duration::from_secs(2)).unwrap();
                barrier.wait();
                if claim_job(&mut conn, &job(), owner, Utc::now(), CLAIM_LEASE).unwrap() {
                    won.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(won.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stale_claim_recovers_but_live_and_sent_jobs_do_not() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema(&conn);
        let job = job();
        ensure_jobs(&conn, std::slice::from_ref(&job)).unwrap();
        let now = Utc::now();
        assert!(claim_job(&mut conn, &job, "dead", now, Duration::ZERO).unwrap());
        assert!(claim_job(&mut conn, &job, "recovery", now, CLAIM_LEASE).unwrap());
        assert!(!claim_job(&mut conn, &job, "other", now, CLAIM_LEASE).unwrap());
        finalize_sent(&mut conn, &job, "recovery", now).unwrap();
        assert!(
            !claim_job(
                &mut conn,
                &job,
                "other",
                now + chrono::Duration::days(2),
                CLAIM_LEASE
            )
            .unwrap()
        );
    }

    #[test]
    fn finalization_is_visible_to_legacy_rollback_readers() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema(&conn);
        let job = job();
        let now = DateTime::parse_from_rfc3339("2026-04-11T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        ensure_jobs(&conn, std::slice::from_ref(&job)).unwrap();
        assert!(claim_job(&mut conn, &job, "sender", now, CLAIM_LEASE).unwrap());

        finalize_sent(&mut conn, &job, "sender", now).unwrap();

        let scheduled: (String, String) = conn
            .query_row(
                "SELECT state, sent_at FROM scheduled_report_jobs
                 WHERE period_type = ?1 AND period_key = ?2",
                params![job.period_type, job.period_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let legacy_sent_at: String = conn
            .query_row(
                "SELECT sent_at FROM sent_reports
                 WHERE period_type = ?1 AND period_key = ?2",
                params![job.period_type, job.period_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(scheduled, ("sent".into(), now.to_rfc3339()));
        assert_eq!(legacy_sent_at, now.to_rfc3339());
    }

    #[test]
    fn lost_claim_finalization_writes_neither_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema(&conn);
        let job = job();
        let now = Utc::now();
        ensure_jobs(&conn, std::slice::from_ref(&job)).unwrap();
        assert!(claim_job(&mut conn, &job, "winner", now, CLAIM_LEASE).unwrap());

        let error = finalize_sent(&mut conn, &job, "loser", now).unwrap_err();

        assert!(error.to_string().contains("Lost scheduler claim"));
        let state: String = conn
            .query_row(
                "SELECT state FROM scheduled_report_jobs
                 WHERE period_type = ?1 AND period_key = ?2",
                params![job.period_type, job.period_key],
                |row| row.get(0),
            )
            .unwrap();
        let legacy_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sent_reports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "claimed");
        assert_eq!(legacy_count, 0);
    }

    #[test]
    fn legacy_mirror_failure_rolls_back_job_finalization() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema(&conn);
        conn.execute_batch(
            "CREATE TRIGGER reject_legacy_send BEFORE INSERT ON sent_reports
             BEGIN SELECT RAISE(ABORT, 'legacy write rejected'); END;",
        )
        .unwrap();
        let job = job();
        let now = Utc::now();
        ensure_jobs(&conn, std::slice::from_ref(&job)).unwrap();
        assert!(claim_job(&mut conn, &job, "sender", now, CLAIM_LEASE).unwrap());

        assert!(finalize_sent(&mut conn, &job, "sender", now).is_err());

        let state: (String, Option<String>) = conn
            .query_row(
                "SELECT state, sent_at FROM scheduled_report_jobs
                 WHERE period_type = ?1 AND period_key = ?2",
                params![job.period_type, job.period_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, ("claimed".into(), None));
        let legacy_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sent_reports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(legacy_count, 0);
    }

    #[test]
    fn stop_before_claim_leaves_job_pending_for_restart() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema(&conn);
        let job = job();
        ensure_jobs(&conn, std::slice::from_ref(&job)).unwrap();
        let stop = AtomicBool::new(true);

        assert!(
            !claim_job_unless_stopping(
                &mut conn,
                &job,
                "stopping",
                Utc::now(),
                CLAIM_LEASE,
                &stop,
            )
            .unwrap()
        );
        stop.store(false, Ordering::Release);
        assert!(
            claim_job_unless_stopping(&mut conn, &job, "restart", Utc::now(), CLAIM_LEASE, &stop,)
                .unwrap()
        );
    }

    #[test]
    fn shutdown_joins_idle_worker_cleanly() {
        let mut scheduler = Scheduler::start(true).unwrap();
        scheduler.shutdown_with_timeout(Duration::from_secs(2));
        assert!(scheduler.worker.is_none());
        scheduler.check();
    }
}
