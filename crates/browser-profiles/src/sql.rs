//! Column readers that tolerate real-world database contents.

use rusqlite::Row;
use rusqlite::types::ValueRef;

/// Read a text column, replacing invalid UTF-8 rather than failing.
///
/// Browsers store page titles as raw bytes from the network and do not
/// guarantee valid UTF-8. A single malformed title would otherwise abort the
/// whole read, losing every other row in the database.
pub fn text(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            let s = String::from_utf8_lossy(bytes).into_owned();
            (!s.is_empty()).then_some(s)
        }
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
    })
}

/// Like [`text`], but for a column that must be present.
pub fn required_text(row: &Row<'_>, idx: usize) -> rusqlite::Result<String> {
    Ok(text(row, idx)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    #[test]
    fn invalid_utf8_is_replaced_not_rejected() {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch("CREATE TABLE t (a BLOB);").expect("ddl");
        // 0xFF is never valid UTF-8.
        conn.execute("INSERT INTO t VALUES (?1)", [&b"bad\xFFtitle"[..]])
            .expect("insert");

        let got: Option<String> = conn
            .query_row("SELECT a FROM t", [], |r| text(r, 0))
            .expect("read");
        assert_eq!(got.as_deref(), Some("bad\u{FFFD}title"));
    }

    #[test]
    fn empty_string_reads_as_none() {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch("CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('');")
            .expect("seed");
        let got: Option<String> = conn
            .query_row("SELECT a FROM t", [], |r| text(r, 0))
            .expect("read");
        assert_eq!(got, None);
    }

    #[test]
    fn null_reads_as_none() {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch("CREATE TABLE t (a TEXT); INSERT INTO t VALUES (NULL);")
            .expect("seed");
        let got: Option<String> = conn
            .query_row("SELECT a FROM t", [], |r| text(r, 0))
            .expect("read");
        assert_eq!(got, None);
    }
}
