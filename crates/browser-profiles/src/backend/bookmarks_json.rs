//! Chromium's `Bookmarks` file.
//!
//! Unlike Firefox, Chromium keeps bookmarks outside the history database in a
//! JSON tree of nested folders. Timestamps are WebKit-epoch microseconds
//! serialised as strings.

use std::path::Path;

use serde_json::Value;

use crate::error::Result;
use crate::model::Bookmark;
use crate::time;

pub fn read(path: &Path) -> Result<Vec<Bookmark>> {
    let text = std::fs::read_to_string(path)?;
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        // A malformed bookmarks file should not fail a whole-profile scan.
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    if let Some(roots) = root.get("roots").and_then(Value::as_object) {
        for node in roots.values() {
            walk(node, None, &mut out);
        }
    }
    Ok(out)
}

fn walk(node: &Value, folder: Option<&str>, out: &mut Vec<Bookmark>) {
    match node.get("type").and_then(Value::as_str) {
        Some("url") => {
            let Some(url) = node.get("url").and_then(Value::as_str) else {
                return;
            };
            out.push(Bookmark {
                url: url.to_string(),
                title: node
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                folder: folder.map(str::to_string),
                added_at: node
                    .get("date_added")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
                    .and_then(time::webkit_micros),
            });
        }
        Some("folder") => {
            let name = node.get("name").and_then(Value::as_str);
            if let Some(children) = node.get("children").and_then(Value::as_array) {
                for child in children {
                    walk(child, name, out);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_folders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("Bookmarks");
        std::fs::write(
            &path,
            r#"{"roots":{"bookmark_bar":{"type":"folder","name":"Bar","children":[
                 {"type":"url","name":"Rust","url":"https://rust-lang.org",
                  "date_added":"13411699200000000"},
                 {"type":"folder","name":"Docs","children":[
                   {"type":"url","name":"Std","url":"https://doc.rust-lang.org"}]}]}}}"#,
        )
        .expect("write");

        let marks = read(&path).expect("read");
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].url, "https://rust-lang.org");
        assert_eq!(marks[0].folder.as_deref(), Some("Bar"));
        assert!(marks[0].added_at.is_some());
        assert_eq!(marks[1].folder.as_deref(), Some("Docs"));
        assert_eq!(marks[1].added_at, None);
    }

    #[test]
    fn malformed_json_yields_empty_not_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("Bookmarks");
        std::fs::write(&path, "{not json").expect("write");
        assert!(read(&path).expect("read").is_empty());
    }
}
