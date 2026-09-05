mod bookmarks_json;
mod chromium;
mod firefox;

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::browser::Family;
use crate::error::Result;
use crate::model::{Bookmark, Download, Engagement, Profile, SearchTerm, Visit, VisitRecord};

/// Open a history database read-only without waiting for a browser-held lock.
fn open_read_only(db: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(db, flags)?;
    conn.busy_timeout(std::time::Duration::ZERO)?;
    Ok(conn)
}

/// Copy a database and its recovery files into a private, writable directory.
///
/// The writable connection is important: SQLite may need to recover a hot
/// rollback journal or WAL before the snapshot can be read. Sidecar copy
/// failures are errors rather than permission to return a potentially stale or
/// corrupt view of the database.
fn snapshot(db: &Path) -> Result<(Connection, tempfile::TempDir)> {
    let tmp = tempfile::tempdir()?;
    let name = db.file_name().unwrap_or_else(|| "history.sqlite".as_ref());
    let copy = tmp.path().join(name);
    std::fs::copy(db, &copy)?;

    for suffix in ["-journal", "-wal", "-shm"] {
        let mut src = db.as_os_str().to_os_string();
        src.push(suffix);
        let src = std::path::PathBuf::from(src);
        match std::fs::symlink_metadata(&src) {
            Ok(_) => {
                let mut dst = copy.as_os_str().to_os_string();
                dst.push(suffix);
                std::fs::copy(&src, std::path::PathBuf::from(dst))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    let conn = Connection::open(&copy)?;
    conn.busy_timeout(std::time::Duration::ZERO)?;
    Ok((conn, tmp))
}

fn is_busy_or_locked(error: &crate::Error) -> bool {
    matches!(
        error,
        crate::Error::Database(rusqlite::Error::SqliteFailure(inner, _))
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn has_tables(conn: &Connection, tables: &[&str]) -> Result<bool> {
    let mut statement = conn
        .prepare("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)")?;
    for table in tables {
        if !statement.query_row([table], |row| row.get(0))? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Run a read against a profile and retry the actual read on a private
/// snapshot.
///
/// `optional_tables` describes a browser capability. Only their explicit
/// absence yields an empty result; malformed SQL and all other input errors are
/// preserved.
fn query<T>(
    db: &Path,
    optional_tables: &[&str],
    read: impl Fn(&Connection) -> Result<Vec<T>>,
) -> Result<Vec<T>> {
    fn attempt<T>(
        conn: &Connection,
        optional_tables: &[&str],
        read: &impl Fn(&Connection) -> Result<Vec<T>>,
    ) -> Result<Vec<T>> {
        if !has_tables(conn, optional_tables)? {
            return Ok(Vec::new());
        }
        read(conn)
    }

    let conn = open_read_only(db)?;
    match attempt(&conn, optional_tables, &read) {
        Ok(rows) => Ok(rows),
        Err(error) if is_busy_or_locked(&error) => {
            drop(conn);
            let (copy, _tmp) = snapshot(db)?;
            attempt(&copy, optional_tables, &read)
        }
        Err(error) => Err(error),
    }
}

pub fn read_visits(db: &Path, family: Family) -> Result<Vec<Visit>> {
    match family {
        Family::Firefox => query(db, &[], firefox::read_places),
        Family::Chromium => query(db, &[], chromium::read_urls),
    }
}

pub fn read_visit_records(db: &Path, family: Family) -> Result<Vec<VisitRecord>> {
    match family {
        Family::Firefox => query(db, &[], firefox::read_visit_records),
        Family::Chromium => query(db, &[], chromium::read_visit_records),
    }
}

/// Firefox-only; Chromium has no engagement table and yields an empty result.
pub fn read_engagement(db: &Path, family: Family) -> Result<Vec<Engagement>> {
    match family {
        Family::Firefox => query(db, &["moz_places_metadata"], firefox::read_engagement),
        Family::Chromium => Ok(Vec::new()),
    }
}

/// Chromium-only; Firefox records downloads as place annotations, not a table.
pub fn read_downloads(db: &Path, family: Family) -> Result<Vec<Download>> {
    match family {
        Family::Firefox => Ok(Vec::new()),
        Family::Chromium => query(
            db,
            &["downloads", "downloads_url_chains"],
            chromium::read_downloads,
        ),
    }
}

/// Chromium-only; Firefox's `moz_places_metadata_search_queries` is populated
/// only under an off-by-default feature flag and is empty in practice.
pub fn read_search_terms(db: &Path, family: Family) -> Result<Vec<SearchTerm>> {
    match family {
        Family::Firefox => Ok(Vec::new()),
        Family::Chromium => query(db, &["keyword_search_terms"], chromium::read_search_terms),
    }
}

/// Firefox keeps bookmarks in the history database; Chromium keeps them in a
/// sibling JSON file.
pub fn read_bookmarks(profile: &Profile) -> Result<Vec<Bookmark>> {
    match profile.family() {
        Family::Firefox => query(
            &profile.history_db,
            &["moz_bookmarks"],
            firefox::read_bookmarks,
        ),
        Family::Chromium => match profile.sibling("Bookmarks") {
            Some(path) => bookmarks_json::read(&path),
            None => Ok(Vec::new()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn locked_database_falls_back_to_a_copy_without_waiting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("places.sqlite");
        let writer = Connection::open(&db).expect("create");
        writer
            .execute_batch(
                "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT, title TEXT,
                    visit_count INTEGER, last_visit_date INTEGER, description TEXT,
                    site_name TEXT, frecency INTEGER, typed INTEGER);
                 INSERT INTO moz_places VALUES
                    (1,'https://example.com/','Example',1,NULL,NULL,NULL,NULL,0);",
            )
            .expect("seed");

        // Hold an exclusive lock the way a running browser does.
        writer
            .execute_batch("BEGIN EXCLUSIVE; INSERT INTO moz_places (url) VALUES ('x');")
            .expect("lock");

        let start = Instant::now();
        let visits = read_visits(&db, Family::Firefox).expect("read while locked");
        let elapsed = start.elapsed();

        assert_eq!(visits.len(), 1, "the copy must still carry the data");
        assert!(
            elapsed < Duration::from_secs(2),
            "fell back after {elapsed:?}; SQLite's default 5s busy timeout is \
             being waited out instead of skipped"
        );
    }

    #[test]
    fn retries_the_read_callback_after_busy() {
        use std::cell::Cell;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("history.sqlite");
        Connection::open(&db)
            .expect("create")
            .execute_batch(
                "CREATE TABLE values_table (value INTEGER); INSERT INTO values_table VALUES (7);",
            )
            .expect("seed");
        let calls = Cell::new(0);

        let values = query(&db, &[], |conn| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                return Err(crate::Error::Database(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    Some("injected busy".to_owned()),
                )));
            }
            Ok(vec![conn.query_row(
                "SELECT value FROM values_table",
                [],
                |row| row.get::<_, i64>(0),
            )?])
        })
        .expect("retry");

        assert_eq!(calls.get(), 2);
        assert_eq!(values, vec![7]);
    }

    #[test]
    fn absent_optional_table_is_empty_but_sql_errors_are_not_suppressed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("history.sqlite");
        Connection::open(&db).expect("create");

        let absent = query::<i64>(&db, &["optional_data"], |_| {
            panic!("callback must not run without the declared capability")
        })
        .expect("absent capability");
        assert!(absent.is_empty());

        let error = query::<i64>(&db, &[], |conn| {
            conn.prepare("SELECT FROM malformed")?;
            Ok(Vec::new())
        })
        .expect_err("malformed SQL must be reported");
        assert!(matches!(
            error,
            crate::Error::Database(rusqlite::Error::SqlInputError { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_copy_errors_fail_closed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("history.sqlite");
        Connection::open(&db).expect("create");
        symlink(
            dir.path().join("missing"),
            dir.path().join("history.sqlite-wal"),
        )
        .expect("dangling WAL");

        let error = snapshot(&db).expect_err("an unreadable sidecar must not be ignored");
        assert!(matches!(error, crate::Error::Io(_)));
    }
}
