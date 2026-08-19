mod bookmarks_json;
mod chromium;
mod firefox;

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::browser::Family;
use crate::error::Result;
use crate::model::{Bookmark, Download, Engagement, Profile, SearchTerm, Visit, VisitRecord};

/// Open a history database read-only, falling back to a temporary copy.
///
/// Chromium holds an exclusive lock on `History` while running, which defeats
/// even read-only opens, and Firefox can leave a WAL that a read-only handle
/// refuses to recover. Copying sidesteps both.
fn open(db: &Path) -> Result<(Connection, Option<tempfile::TempDir>)> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    if let Ok(conn) = Connection::open_with_flags(db, flags) {
        // A running browser holds this lock indefinitely, so waiting out the
        // default five-second busy timeout only delays the copy that follows.
        let _ = conn.busy_timeout(std::time::Duration::ZERO);
        // Opening succeeds lazily; touching the schema is what surfaces a lock.
        if conn
            .prepare("SELECT name FROM sqlite_master LIMIT 1")
            .is_ok()
        {
            return Ok((conn, None));
        }
    }

    let tmp = tempfile::tempdir()?;
    let name = db.file_name().unwrap_or_else(|| "history.sqlite".as_ref());
    let copy = tmp.path().join(name);
    std::fs::copy(db, &copy)?;

    // The WAL must come along or the copy reflects a stale checkpoint and
    // silently omits the most recent visits.
    for suffix in ["-wal", "-shm"] {
        let mut src = db.as_os_str().to_os_string();
        src.push(suffix);
        let src = std::path::PathBuf::from(src);
        if src.exists() {
            let mut dst = copy.as_os_str().to_os_string();
            dst.push(suffix);
            let _ = std::fs::copy(&src, std::path::PathBuf::from(dst));
        }
    }

    let conn = Connection::open_with_flags(&copy, flags)?;
    Ok((conn, Some(tmp)))
}

/// Run a read against a profile, tolerating tables the browser version lacks.
///
/// Schemas drift between releases — `moz_places_metadata` exists only in newer
/// Firefox. A missing table means "this browser does not record that", which
/// is an empty result rather than an error.
fn query<T>(db: &Path, read: impl FnOnce(&Connection) -> Result<Vec<T>>) -> Result<Vec<T>> {
    let (conn, _tmp) = open(db)?;
    match read(&conn) {
        Ok(rows) => Ok(rows),
        Err(crate::Error::Database(rusqlite::Error::SqlInputError { .. })) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

pub fn read_visits(db: &Path, family: Family) -> Result<Vec<Visit>> {
    match family {
        Family::Firefox => query(db, firefox::read_places),
        Family::Chromium => query(db, chromium::read_urls),
    }
}

pub fn read_visit_records(db: &Path, family: Family) -> Result<Vec<VisitRecord>> {
    match family {
        Family::Firefox => query(db, firefox::read_visit_records),
        Family::Chromium => query(db, chromium::read_visit_records),
    }
}

/// Firefox-only; Chromium has no engagement table and yields an empty result.
pub fn read_engagement(db: &Path, family: Family) -> Result<Vec<Engagement>> {
    match family {
        Family::Firefox => query(db, firefox::read_engagement),
        Family::Chromium => Ok(Vec::new()),
    }
}

/// Chromium-only; Firefox records downloads as place annotations, not a table.
pub fn read_downloads(db: &Path, family: Family) -> Result<Vec<Download>> {
    match family {
        Family::Firefox => Ok(Vec::new()),
        Family::Chromium => query(db, chromium::read_downloads),
    }
}

/// Chromium-only; Firefox's `moz_places_metadata_search_queries` is populated
/// only under an off-by-default feature flag and is empty in practice.
pub fn read_search_terms(db: &Path, family: Family) -> Result<Vec<SearchTerm>> {
    match family {
        Family::Firefox => Ok(Vec::new()),
        Family::Chromium => query(db, chromium::read_search_terms),
    }
}

/// Firefox keeps bookmarks in the history database; Chromium keeps them in a
/// sibling JSON file.
pub fn read_bookmarks(profile: &Profile) -> Result<Vec<Bookmark>> {
    match profile.family() {
        Family::Firefox => query(&profile.history_db, firefox::read_bookmarks),
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
}
