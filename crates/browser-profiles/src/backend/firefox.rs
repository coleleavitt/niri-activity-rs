use std::time::Duration;

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{Bookmark, Engagement, Visit, VisitRecord, VisitType};
use crate::{sql, time};

const PLACES: &str = "SELECT url, title, visit_count, last_visit_date, \
                             description, site_name, frecency, typed \
                      FROM moz_places WHERE url IS NOT NULL";

const VISITS: &str = "SELECT p.url, p.title, v.visit_date, v.visit_type, r.url \
                      FROM moz_historyvisits v \
                      JOIN moz_places p ON p.id = v.place_id \
                      LEFT JOIN moz_historyvisits pv ON pv.id = v.from_visit \
                      LEFT JOIN moz_places r ON r.id = pv.place_id \
                      WHERE p.url IS NOT NULL \
                      ORDER BY v.visit_date";

const ENGAGEMENT: &str = "SELECT p.url, p.title, m.total_view_time, m.typing_time, \
                                 m.key_presses, m.scrolling_time, m.scrolling_distance, \
                                 r.url, m.document_type, m.created_at, m.updated_at \
                          FROM moz_places_metadata m \
                          JOIN moz_places p ON p.id = m.place_id \
                          LEFT JOIN moz_places r ON r.id = m.referrer_place_id \
                          WHERE p.url IS NOT NULL";

// type 1 is a bookmark; 2 and 3 are folders and separators.
const BOOKMARKS: &str = "SELECT p.url, b.title, f.title, b.dateAdded \
                         FROM moz_bookmarks b \
                         JOIN moz_places p ON p.id = b.fk \
                         LEFT JOIN moz_bookmarks f ON f.id = b.parent \
                         WHERE b.type = 1 AND p.url IS NOT NULL";

/// Durations are stored as signed integers but represent elapsed time; clamp
/// negatives rather than wrapping into an enormous `u64`.
fn millis(raw: Option<i64>) -> Duration {
    Duration::from_millis(raw.unwrap_or(0).max(0).unsigned_abs())
}

pub fn read_places(conn: &Connection) -> Result<Vec<Visit>> {
    let mut stmt = conn.prepare(PLACES)?;
    let rows = stmt.query_map([], |row| {
        Ok(Visit {
            url: sql::required_text(row, 0)?,
            title: sql::text(row, 1)?,
            visit_count: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            last_visit: row.get::<_, Option<i64>>(3)?.and_then(time::unix_micros),
            description: sql::text(row, 4)?,
            site_name: sql::text(row, 5)?,
            frecency: row.get::<_, Option<i64>>(6)?,
            typed: row.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn read_visit_records(conn: &Connection) -> Result<Vec<VisitRecord>> {
    let mut stmt = conn.prepare(VISITS)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            sql::required_text(row, 0)?,
            sql::text(row, 1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            sql::text(row, 4)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (url, title, date, kind, referrer) = row?;
        // A visit without a timestamp cannot be placed on a timeline, which is
        // the entire point of this table.
        let Some(visited_at) = date.and_then(time::unix_micros) else {
            continue;
        };
        out.push(VisitRecord {
            url,
            title,
            visited_at,
            visit_type: VisitType::from_firefox(kind),
            referrer,
            duration: None,
        });
    }
    Ok(out)
}

pub fn read_engagement(conn: &Connection) -> Result<Vec<Engagement>> {
    let mut stmt = conn.prepare(ENGAGEMENT)?;
    let rows = stmt.query_map([], |row| {
        Ok(Engagement {
            url: sql::required_text(row, 0)?,
            title: sql::text(row, 1)?,
            view_time: millis(row.get(2)?),
            typing_time: millis(row.get(3)?),
            key_presses: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            scrolling_time: millis(row.get(5)?),
            scrolling_distance: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            referrer: sql::text(row, 7)?,
            is_media: row.get::<_, Option<i64>>(8)?.unwrap_or(0) == 1,
            // This table stores milliseconds while the rest of the file uses
            // microseconds.
            created_at: row.get::<_, Option<i64>>(9)?.and_then(time::unix_millis),
            updated_at: row.get::<_, Option<i64>>(10)?.and_then(time::unix_millis),
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn read_bookmarks(conn: &Connection) -> Result<Vec<Bookmark>> {
    let mut stmt = conn.prepare(BOOKMARKS)?;
    let rows = stmt.query_map([], |row| {
        Ok(Bookmark {
            url: sql::required_text(row, 0)?,
            title: sql::text(row, 1)?,
            folder: sql::text(row, 2)?,
            added_at: row.get::<_, Option<i64>>(3)?.and_then(time::unix_micros),
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT, title TEXT,
                visit_count INTEGER, last_visit_date INTEGER, description TEXT,
                site_name TEXT, frecency INTEGER, typed INTEGER);
             INSERT INTO moz_places VALUES
                (1,'https://example.com/a','Example',3,1767225600000000,'Desc','Example',120,1),
                (2,'https://other.com/b',NULL,NULL,NULL,NULL,NULL,NULL,0);

             CREATE TABLE moz_historyvisits (id INTEGER PRIMARY KEY, from_visit INTEGER,
                place_id INTEGER, visit_date INTEGER, visit_type INTEGER);
             INSERT INTO moz_historyvisits VALUES
                (10,0,1,1767225600000000,2),
                (11,10,2,1767225700000000,1),
                (12,0,2,NULL,1);

             CREATE TABLE moz_places_metadata (id INTEGER PRIMARY KEY, place_id INTEGER,
                referrer_place_id INTEGER, created_at INTEGER, updated_at INTEGER,
                total_view_time INTEGER, typing_time INTEGER, key_presses INTEGER,
                scrolling_time INTEGER, scrolling_distance INTEGER, document_type INTEGER);
             INSERT INTO moz_places_metadata VALUES
                (1,2,1,1767225600000,1767225700000,60000,5000,42,3000,1500,1);

             CREATE TABLE moz_bookmarks (id INTEGER PRIMARY KEY, type INTEGER, fk INTEGER,
                parent INTEGER, title TEXT, dateAdded INTEGER);
             INSERT INTO moz_bookmarks VALUES
                (1,2,NULL,NULL,'Toolbar',NULL),
                (2,1,1,1,'Bookmarked',1767225600000000);",
        )
        .expect("seed");
        conn
    }

    #[test]
    fn places_include_extended_metadata() {
        let visits = read_places(&seeded()).expect("read");
        assert_eq!(visits.len(), 2);
        assert_eq!(visits[0].description.as_deref(), Some("Desc"));
        assert_eq!(visits[0].site_name.as_deref(), Some("Example"));
        assert_eq!(visits[0].frecency, Some(120));
        assert!(visits[0].typed);
        assert!(!visits[1].typed);
        assert_eq!(visits[1].visit_count, 0);
    }

    #[test]
    fn visit_records_resolve_referrer_and_skip_undated() {
        let records = read_visit_records(&seeded()).expect("read");
        // The row with a NULL visit_date is dropped.
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].visit_type, VisitType::Typed);
        assert_eq!(records[0].referrer, None);
        assert_eq!(records[1].visit_type, VisitType::Link);
        assert_eq!(
            records[1].referrer.as_deref(),
            Some("https://example.com/a"),
            "from_visit should resolve to the referring page's URL"
        );
    }

    #[test]
    fn engagement_uses_millisecond_timestamps() {
        let rows = read_engagement(&seeded()).expect("read");
        assert_eq!(rows.len(), 1);
        let e = &rows[0];
        assert_eq!(e.view_time, Duration::from_secs(60));
        assert_eq!(e.typing_time, Duration::from_secs(5));
        assert_eq!(e.key_presses, 42);
        assert_eq!(e.scrolling_distance, 1500);
        assert!(e.is_media);
        assert_eq!(e.referrer_domain().as_deref(), Some("example.com"));
        // created_at is milliseconds here but microseconds elsewhere in the
        // same database; a unit mix-up would land far in the future.
        assert_eq!(
            e.created_at.expect("created_at").to_rfc3339(),
            "2026-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn bookmarks_exclude_folders_and_resolve_parent() {
        let marks = read_bookmarks(&seeded()).expect("read");
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].url, "https://example.com/a");
        assert_eq!(marks[0].folder.as_deref(), Some("Toolbar"));
        assert!(marks[0].added_at.is_some());
    }
}
