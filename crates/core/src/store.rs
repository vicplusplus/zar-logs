//! Writing encounter logs: `enc-<id>.json.gz` (summary, full detail, raw hit rows) and `index.json` (list rows).
//!
//! The logs are the truth and the index is a cache of their summaries, so that listing does not have to open every
//! log. It is checked against the folder each time it is read: a log that is there but not listed is added, a row
//! whose log is gone is dropped. So a failed or interrupted index update can never hide a saved mission.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

use crate::names::Resolver;
use crate::session::Encounter;

/// Encounters without a single player hit (menus, a mission left at once) are not saved. Nor is the hub: it is
/// not a mission (`Encounter::is_mission`).
const MIN_HITS: u64 = 1;

const LOG_PREFIX: &str = "enc-";
const LOG_SUFFIX: &str = ".json.gz";

pub fn log_path(logs: &Path, id: &str) -> std::path::PathBuf { logs.join(format!("{LOG_PREFIX}{id}{LOG_SUFFIX}")) }

/// The summary row stored inside a log.
fn summary_of(path: &Path) -> Option<Value> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(std::fs::File::open(path).ok()?), &mut text).ok()?;
    serde_json::from_str::<Value>(&text).ok()?.get_mut("summary").map(Value::take)
}

/// The ids of the logs in the folder, or None when the folder cannot be listed.
fn log_ids(logs: &Path) -> Option<BTreeSet<String>> {
    Some(std::fs::read_dir(logs).ok()?.flatten()
        .filter_map(|e| Some(e.file_name().to_str()?.strip_prefix(LOG_PREFIX)?.strip_suffix(LOG_SUFFIX)?.to_string()))
        .collect())
}

/// List rows, oldest first, and whether they differ from what `index.json` holds (a log was missing from it, a
/// row had no log, or there is no index). The rows always match the logs that are in the folder.
pub fn read_index_checked(logs: &Path) -> (Vec<Value>, bool) {
    let stored = std::fs::read_to_string(logs.join("index.json")).ok().and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok());
    let Some(ids) = log_ids(logs) else { return (stored.unwrap_or_default(), false) };
    let mut changed = stored.is_none() && !ids.is_empty();
    let mut rows = stored.unwrap_or_default();
    let before = rows.len();
    let mut listed = BTreeSet::new();
    rows.retain(|r| r["id"].as_str().is_some_and(|id| ids.contains(id) && listed.insert(id.to_string())));
    changed |= rows.len() != before;
    for id in ids.difference(&listed) {
        if let Some(row) = summary_of(&log_path(logs, id)) {
            rows.push(row);
            changed = true;
        }
    }
    if changed {
        rows.sort_by(|a, b| a["start"].as_f64().partial_cmp(&b["start"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    }
    (rows, changed)
}

/// List rows, oldest first (see `read_index_checked`).
pub fn read_index(logs: &Path) -> Vec<Value> { read_index_checked(logs).0 }

/// List rows, oldest first; when the stored index was out of step with the folder it is written again.
pub fn sync_index(logs: &Path) -> Vec<Value> {
    let (rows, changed) = read_index_checked(logs);
    if changed {
        let _ = write_index(logs, &rows); // the rows are right either way; the next read tries again
    }
    rows
}

pub fn write_index(logs: &Path, rows: &[Value]) -> std::io::Result<()> {
    let (tmp, index) = (logs.join("index.tmp"), logs.join("index.json"));
    let text = serde_json::to_string(rows).unwrap_or_else(|_| "[]".into());
    std::fs::write(&tmp, &text)?;
    // Replacing a file can fail for a moment on Windows while another program (a virus scanner, the search
    // indexer) has the old or the new one open: try again briefly, then write in place.
    for _ in 0..20 {
        if std::fs::rename(&tmp, &index).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&index, text)
}

/// Persist a closed encounter. Returns its summary row, or None when it had no damage.
pub fn save(logs: &Path, enc: &Encounter, resolver: &Resolver) -> std::io::Result<Option<Value>> {
    if enc.server_hits < MIN_HITS || enc.hub_name().is_some() {
        return Ok(None);
    }
    let summary = enc.summary();
    let doc = json!({
        "summary": summary,
        "detail": enc.detail(resolver),
        "hits": { "columns": ["t", "client", "source", "target", "health", "shield", "flags", "type", "entity"], "rows": enc.hits },
    });
    std::fs::create_dir_all(logs)?;
    // written under another name first, so a log that is in the folder is always a whole one
    let (part, path) = (logs.join(format!("{}.part", enc.id)), log_path(logs, &enc.id));
    let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(&part)?, flate2::Compression::new(5));
    gz.write_all(doc.to_string().as_bytes())?;
    gz.finish()?.sync_all()?;
    let _ = std::fs::remove_file(&path);
    std::fs::rename(&part, &path)?;
    // The mission is saved. The index is a cache (see the top of this file): failing to update it is not an
    // error, the log is found in the folder the next time the list is read.
    let _ = sync_index(logs);
    Ok(Some(summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_log(logs: &Path, id: &str, start: f64) {
        let doc = json!({ "summary": { "id": id, "start": start }, "detail": {}, "hits": {} });
        let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(log_path(logs, id)).unwrap(), flate2::Compression::new(1));
        gz.write_all(doc.to_string().as_bytes()).unwrap();
        gz.finish().unwrap();
    }

    fn ids(rows: &[Value]) -> Vec<&str> { rows.iter().map(|r| r["id"].as_str().unwrap()).collect() }

    #[test]
    fn the_list_follows_the_logs_in_the_folder() {
        let logs = std::env::temp_dir().join(format!("zar-logs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&logs);
        std::fs::create_dir_all(&logs).unwrap();

        // no index yet: the list is built from the logs, oldest first, and the index is written
        write_log(&logs, "b", 2.0);
        write_log(&logs, "a", 1.0);
        assert_eq!(ids(&sync_index(&logs)), ["a", "b"]);
        assert_eq!(read_index_checked(&logs).1, false, "the index was written and now agrees with the folder");

        // a log saved without its index update (the update failed, or the app was closed in between) is listed
        write_log(&logs, "c", 3.0);
        let (rows, changed) = read_index_checked(&logs);
        assert_eq!((ids(&rows), changed), (vec!["a", "b", "c"], true));

        // a row whose log was deleted by hand is dropped; a file that is not a log is ignored
        sync_index(&logs);
        std::fs::remove_file(log_path(&logs, "a")).unwrap();
        std::fs::write(logs.join("notes.txt"), "mine").unwrap();
        assert_eq!(ids(&sync_index(&logs)), ["b", "c"]);

        // a damaged index is rebuilt
        std::fs::write(logs.join("index.json"), "{ not json").unwrap();
        assert_eq!(ids(&sync_index(&logs)), ["b", "c"]);
        assert_eq!(ids(&serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(logs.join("index.json")).unwrap()).unwrap()), ["b", "c"]);

        std::fs::remove_dir_all(&logs).unwrap();
    }
}
