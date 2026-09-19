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
// Visits recorded in the same microsecond are ordered by visit id so that
// session reconstruction sees one fixed sequence instead of whatever order the
// current query plan happens to produce.
const VISITS: &str = "SELECT u.url, u.title, v.visit_time, v.transition, r.url, v.visit_duration \
                      FROM visits v \
                      JOIN urls u ON u.id = v.url \
                      LEFT JOIN visits pv ON pv.id = v.from_visit \
                      LEFT JOIN urls r ON r.id = pv.url \
                      WHERE u.url IS NOT NULL \
                      ORDER BY v.visit_time, v.id";

const DOWNLOADS: &str = "SELECT COALESCE(\
                                (SELECT c.url FROM downloads_url_chains c \
                                 WHERE c.id = d.id \
                                 ORDER BY c.chain_index DESC LIMIT 1), \
                                d.tab_url), \
                                d.target_path, d.mime_type, d.total_bytes, \
                                d.received_bytes, d.tab_url, d.start_time, d.end_time \
                         FROM downloads d";

// Chromium stores no timestamp on a search term, and `urls.last_visit_time`
// is the last time the *result page* was opened, which is a different event:
// re-reading a result months later would otherwise be reported as a search.
// The search itself is the visit that carried a KEYWORD (9) or
// KEYWORD_GENERATED (10) core transition type, so the time comes from there,
// and stays unset when no such visit survives history expiry.
const SEARCH_TERMS: &str = "SELECT k.term, u.url, \
                                   (SELECT MAX(v.visit_time) FROM visits v \
                                    WHERE v.url = k.url_id \
                                      AND (v.transition & 255) IN (9, 10)) \
                            FROM keyword_search_terms k \
                            JOIN urls u ON u.id = k.url_id";

pub fn read_urls(conn: &Connection) -> Result<Vec<Visit>> {
    let mut stmt = conn.prepare(URLS)?;
    let rows = stmt.query_map([], |row| {
        // A row with no URL cannot be attributed to a site; reporting it with
        // an empty URL would invent a page visit that never happened.
        let Some(url) = sql::text(row, 0)? else {
            return Ok(None);
        };
        Ok(Some(Visit {
            url,
            title: sql::text(row, 1)?,
            visit_count: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            last_visit: row.get::<_, Option<i64>>(3)?.and_then(time::webkit_micros),
            // Chromium has no equivalent of these Firefox columns.
            description: None,
            site_name: None,
            frecency: None,
            typed: row.get::<_, Option<i64>>(4)?.unwrap_or(0) > 0,
        }))
    })?;
    Ok(sql::collect_present(rows)?)
}

pub fn read_visit_records(conn: &Connection) -> Result<Vec<VisitRecord>> {
    let mut stmt = conn.prepare(VISITS)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            sql::text(row, 0)?,
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
        // Both the URL and the timestamp are what make a navigation event
        // usable; a row missing either is corrupt and is dropped rather than
        // padded with an empty URL or an invented time.
        let (Some(url), Some(visited_at)) = (url, visit_time.and_then(time::webkit_micros)) else {
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
        // A download with no source URL or no target path describes no file;
        // emitting it with empty strings would put a phantom transfer into
        // every report built on this data.
        let (Some(url), Some(target_path)) = (sql::text(row, 0)?, sql::text(row, 1)?) else {
            return Ok(None);
        };
        Ok(Some(Download {
            url,
            target_path: PathBuf::from(target_path),
            mime_type: sql::text(row, 2)?,
            total_bytes: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            received_bytes: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            referrer: sql::text(row, 5)?,
            started_at: row.get::<_, Option<i64>>(6)?.and_then(time::webkit_micros),
            finished_at: row.get::<_, Option<i64>>(7)?.and_then(time::webkit_micros),
        }))
    })?;
    Ok(sql::collect_present(rows)?)
}

pub fn read_search_terms(conn: &Connection) -> Result<Vec<SearchTerm>> {
    let mut stmt = conn.prepare(SEARCH_TERMS)?;
    let rows = stmt.query_map([], |row| {
        // An empty term or result URL is not a search anybody performed.
        let (Some(term), Some(url)) = (sql::text(row, 0)?, sql::text(row, 1)?) else {
            return Ok(None);
        };
        Ok(Some(SearchTerm {
            term,
            url,
            last_searched: row.get::<_, Option<i64>>(2)?.and_then(time::webkit_micros),
        }))
    })?;
    Ok(sql::collect_present(rows)?)
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
                (1,'https://example.com/a','/home/u/f.zip','application/zip',100,100,
                 'https://cdn.example.com/redirect',{t},{t});
             CREATE TABLE downloads_url_chains (id INTEGER, chain_index INTEGER, url TEXT);
             INSERT INTO downloads_url_chains VALUES
                (1,0,'https://example.com/download'),
                (1,1,'https://cdn.example.com/final.zip');

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
        assert_eq!(dls[0].url, "https://cdn.example.com/final.zip");
        assert_eq!(dls[0].referrer.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn search_terms_join_to_result_url() {
        let terms = read_search_terms(&seeded()).expect("read");
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].term, "rust sqlite");
        assert_eq!(terms[0].url, "https://example.com/a");
    }

    /// Database holding one search whose result page is visited again later.
    fn searched_then_revisited() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        let searched = WEBKIT_2026;
        let revisited = WEBKIT_2026 + 86_400_000_000;
        conn.execute_batch(&format!(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT,
                visit_count INTEGER, last_visit_time INTEGER, typed_count INTEGER);
             INSERT INTO urls VALUES (1,'https://example.com/a','Example',2,{revisited},0);

             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER,
                from_visit INTEGER, transition INTEGER, visit_duration INTEGER);
             INSERT INTO visits VALUES
                (10,1,{searched},0,{keyword},0),
                (11,1,{revisited},0,{link},0);

             CREATE TABLE keyword_search_terms (keyword_id INTEGER, url_id INTEGER,
                term TEXT, normalized_term TEXT);
             INSERT INTO keyword_search_terms VALUES (1,1,'rust sqlite','rust sqlite');",
            keyword = 0x1800_0009i64,
            link = 0x1800_0000i64,
        ))
        .expect("seed");
        conn
    }

    #[test]
    fn search_time_is_the_keyword_visit_not_a_later_visit_to_the_result() {
        let terms = read_search_terms(&searched_then_revisited()).expect("read");
        assert_eq!(terms.len(), 1);
        assert_eq!(
            terms[0].last_searched.expect("search time").to_rfc3339(),
            "2026-01-01T00:00:00+00:00",
            "the later visit to the result page is not a second search"
        );
    }

    #[test]
    fn search_without_a_surviving_keyword_visit_has_no_time() {
        let conn = searched_then_revisited();
        conn.execute("DELETE FROM visits WHERE id = 10", [])
            .expect("expire the search visit");

        let terms = read_search_terms(&conn).expect("read");
        assert_eq!(terms.len(), 1);
        assert_eq!(
            terms[0].last_searched, None,
            "an unknown search time must stay unknown"
        );
    }

    #[test]
    fn visits_sharing_a_timestamp_are_ordered_by_visit_id() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(&format!(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT,
                visit_count INTEGER, last_visit_time INTEGER, typed_count INTEGER);
             INSERT INTO urls VALUES
                (1,'https://example.com/first',NULL,1,{t},0),
                (2,'https://example.com/second',NULL,1,{t},0),
                (3,'https://example.com/third',NULL,1,{t},0);

             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER,
                from_visit INTEGER, transition INTEGER, visit_duration INTEGER);
             INSERT INTO visits VALUES
                (10,1,{t},0,0,0),
                (11,2,{t},0,0,0),
                (12,3,{t},0,0,0);",
            t = WEBKIT_2026,
        ))
        .expect("seed");

        let urls: Vec<_> = read_visit_records(&conn)
            .expect("read")
            .into_iter()
            .map(|record| record.url)
            .collect();

        assert_eq!(
            urls,
            [
                "https://example.com/first",
                "https://example.com/second",
                "https://example.com/third"
            ]
        );
    }

    #[test]
    fn rows_missing_a_required_value_are_skipped_not_emptied() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(&format!(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT,
                visit_count INTEGER, last_visit_time INTEGER, typed_count INTEGER);
             INSERT INTO urls VALUES
                (1,'https://example.com/a','Example',1,{t},0),
                (2,'',NULL,1,{t},0);

             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER,
                from_visit INTEGER, transition INTEGER, visit_duration INTEGER);
             INSERT INTO visits VALUES (10,2,{t},0,0,0);

             CREATE TABLE downloads (id INTEGER PRIMARY KEY, tab_url TEXT, target_path TEXT,
                mime_type TEXT, total_bytes INTEGER, received_bytes INTEGER, referrer TEXT,
                start_time INTEGER, end_time INTEGER);
             INSERT INTO downloads VALUES
                (1,'https://example.com/a',NULL,NULL,0,0,NULL,{t},{t});
             CREATE TABLE downloads_url_chains (id INTEGER, chain_index INTEGER, url TEXT);

             CREATE TABLE keyword_search_terms (keyword_id INTEGER, url_id INTEGER,
                term TEXT, normalized_term TEXT);
             INSERT INTO keyword_search_terms VALUES (1,1,NULL,NULL);",
            t = WEBKIT_2026,
        ))
        .expect("seed");

        let urls = read_urls(&conn).expect("read urls");
        assert_eq!(urls.len(), 1, "the empty-URL row must not become a page");
        assert!(
            read_visit_records(&conn).expect("read visits").is_empty(),
            "a navigation to nowhere must not become a visit record"
        );
        assert!(
            read_downloads(&conn).expect("read downloads").is_empty(),
            "a download with no target path must not become a file"
        );
        assert!(
            read_search_terms(&conn).expect("read searches").is_empty(),
            "a search with no term must not become a query"
        );
    }
}
