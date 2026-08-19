use std::path::PathBuf;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{Download, SearchTerm, Visit, VisitRecord, VisitType};
use crate::{sql, time};

const URLS: &str = "SELECT url, title, visit_count, last_visit_time, typed_count \
                    FROM urls WHERE url IS NOT NULL";

// `from_visit` chains within the visits table, so the referring URL needs a
// second hop through `urls`.
const VISITS: &str = "SELECT u.url, u.title, v.visit_time, v.transition, r.url, v.visit_duration \
                      FROM visits v \
                      JOIN urls u ON u.id = v.url \
                      LEFT JOIN visits pv ON pv.id = v.from_visit \
                      LEFT JOIN urls r ON r.id = pv.url \
                      WHERE u.url IS NOT NULL \
                      ORDER BY v.visit_time";

const DOWNLOADS: &str = "SELECT tab_url, target_path, mime_type, total_bytes, \
                                received_bytes, referrer, start_time, end_time \
                         FROM downloads";

const SEARCH_TERMS: &str = "SELECT k.term, u.url, u.last_visit_time \
                            FROM keyword_search_terms k \
                            JOIN urls u ON u.id = k.url_id";

pub fn read_urls(conn: &Connection) -> Result<Vec<Visit>> {
    let mut stmt = conn.prepare(URLS)?;
    let rows = stmt.query_map([], |row| {
        Ok(Visit {
            url: sql::required_text(row, 0)?,
            title: sql::text(row, 1)?,
            visit_count: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            last_visit: row.get::<_, Option<i64>>(3)?.and_then(time::webkit_micros),
            // Chromium has no equivalent of these Firefox columns.
            description: None,
            site_name: None,
            frecency: None,
            typed: row.get::<_, Option<i64>>(4)?.unwrap_or(0) > 0,
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
            row.get::<_, Option<i64>>(5)?.unwrap_or(0),
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (url, title, visit_time, transition, referrer, duration_micros) = row?;
        let Some(visited_at) = visit_time.and_then(time::webkit_micros) else {
            continue;
        };
        out.push(VisitRecord {
            url,
            title,
            visited_at,
            visit_type: VisitType::from_chromium(transition),
            referrer,
            duration: (duration_micros > 0)
                .then(|| Duration::from_micros(duration_micros.unsigned_abs())),
        });
    }
    Ok(out)
}

pub fn read_downloads(conn: &Connection) -> Result<Vec<Download>> {
    let mut stmt = conn.prepare(DOWNLOADS)?;
    let rows = stmt.query_map([], |row| {
        Ok(Download {
            url: sql::required_text(row, 0)?,
            target_path: PathBuf::from(sql::required_text(row, 1)?),
            mime_type: sql::text(row, 2)?,
            total_bytes: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            received_bytes: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            referrer: sql::text(row, 5)?,
            started_at: row.get::<_, Option<i64>>(6)?.and_then(time::webkit_micros),
            finished_at: row.get::<_, Option<i64>>(7)?.and_then(time::webkit_micros),
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn read_search_terms(conn: &Connection) -> Result<Vec<SearchTerm>> {
    let mut stmt = conn.prepare(SEARCH_TERMS)?;
    let rows = stmt.query_map([], |row| {
        Ok(SearchTerm {
            term: sql::required_text(row, 0)?,
            url: sql::required_text(row, 1)?,
            last_searched: row.get::<_, Option<i64>>(2)?.and_then(time::webkit_micros),
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEBKIT_2026: i64 = 11_644_473_600_000_000 + 1_767_225_600_000_000;

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(&format!(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT,
                visit_count INTEGER, last_visit_time INTEGER, typed_count INTEGER);
             INSERT INTO urls VALUES
                (1,'https://example.com/a','Example',5,{t},2),
                (2,'https://other.com/b',NULL,1,0,0);

             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER,
                from_visit INTEGER, transition INTEGER, visit_duration INTEGER);
             INSERT INTO visits VALUES
                (10,1,{t},0,{typed},5000000),
                (11,2,{t},10,0,0);

             CREATE TABLE downloads (id INTEGER PRIMARY KEY, tab_url TEXT, target_path TEXT,
                mime_type TEXT, total_bytes INTEGER, received_bytes INTEGER, referrer TEXT,
                start_time INTEGER, end_time INTEGER);
             INSERT INTO downloads VALUES
                (1,'https://example.com/dl','/home/u/f.zip','application/zip',100,100,
                 'https://example.com/a',{t},{t});

             CREATE TABLE keyword_search_terms (keyword_id INTEGER, url_id INTEGER,
                term TEXT, normalized_term TEXT);
             INSERT INTO keyword_search_terms VALUES (1,1,'rust sqlite','rust sqlite');",
            t = WEBKIT_2026,
            typed = 0x1800_0001i64,
        ))
        .expect("seed");
        conn
    }

    #[test]
    fn urls_map_webkit_epoch_and_typed_flag() {
        let visits = read_urls(&seeded()).expect("read");
        assert_eq!(visits.len(), 2);
        assert_eq!(
            visits[0].last_visit.expect("ts").to_rfc3339(),
            "2026-01-01T00:00:00+00:00"
        );
        assert!(visits[0].typed);
        assert!(!visits[1].typed);
        // Chromium has no frecency/description equivalents.
        assert_eq!(visits[0].frecency, None);
        assert_eq!(visits[1].last_visit, None);
    }

    #[test]
    fn visit_records_carry_duration_and_referrer() {
        let records = read_visit_records(&seeded()).expect("read");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].visit_type, VisitType::Typed);
        assert_eq!(records[0].duration, Some(Duration::from_secs(5)));
        assert_eq!(
            records[1].referrer.as_deref(),
            Some("https://example.com/a")
        );
        // Zero duration means "unknown", not "instantaneous".
        assert_eq!(records[1].duration, None);
    }

    #[test]
    fn downloads_include_provenance() {
        let dls = read_downloads(&seeded()).expect("read");
        assert_eq!(dls.len(), 1);
        assert_eq!(dls[0].mime_type.as_deref(), Some("application/zip"));
        assert_eq!(dls[0].target_path, PathBuf::from("/home/u/f.zip"));
        assert_eq!(dls[0].referrer.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn search_terms_join_to_result_url() {
        let terms = read_search_terms(&seeded()).expect("read");
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].term, "rust sqlite");
        assert_eq!(terms[0].url, "https://example.com/a");
    }
}
