//! Encounter logs on disk: one file per mission (summary, full detail, raw hit rows) and `index.json` (list rows).
//!
//! A log is called `enc-<id>_<map>_<mode>_<difficulty>.json.gz`: the id is what the program goes by (the local
//! time the mission started), the rest is there for whoever looks at the folder or is sent the file. Logs written
//! before that are called `enc-<id>.json.gz`; they are found just the same and renamed when the list is next read.
//!
//! The logs are the truth and the index is a cache of their summaries, so that listing does not have to open every
//! log. It is checked against the folder each time it is read: a log that is there but not listed is added, a row
//! whose log is gone is dropped. So a failed or interrupted index update can never hide a saved mission.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::names::Resolver;
use crate::session::Encounter;

/// Encounters without a single player hit (menus, a mission left at once) are not saved. Nor is the hub: it is
/// not a mission (`Encounter::is_mission`).
const MIN_HITS: u64 = 1;

const LOG_PREFIX: &str = "enc-";
const LOG_SUFFIX: &str = ".json.gz";
/// Between the id and the readable part of a file name, and between the parts of that.
const NAME_SEP: char = '_';
/// Longest readable part of a file name, in characters.
const NAME_MAX: usize = 80;
/// The most a log may hold once unpacked. A real one is a few megabytes; this is against a file made to exhaust
/// memory.
const MAX_LOG_BYTES: u64 = 256 << 20;

/// True for text that can be the id of a log: letters, digits and '-' only, so it can stand in a file name and in
/// an address, and never contains the separator of the readable part.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The id in a log's file name, or None when the file is not a log.
fn id_of(file_name: &str) -> Option<&str> {
    let middle = file_name.strip_prefix(LOG_PREFIX)?.strip_suffix(LOG_SUFFIX)?;
    let id = middle.split(NAME_SEP).next()?;
    valid_id(id).then_some(id)
}

/// A word of the readable part: letters and digits kept, anything else a '-'.
fn word(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// The file name of the log with this summary: `enc-20261118-143424_Geode-Cavern_Rescue_Easy.json.gz`, the map,
/// the mode and the difficulty ("Hard-Tough" with the modifier) as far as the summary names them.
pub fn file_name(summary: &Value) -> String {
    let id = summary["id"].as_str().unwrap_or("");
    let m = &summary["mission"];
    let text = |key: &str| m[key].as_str().unwrap_or("");
    let difficulty = [text("difficulty"), if m["tough"].as_bool() == Some(true) { "Tough" } else { "" }].join(" ");
    let parts: Vec<String> = [text("name"), text("type"), difficulty.as_str()].iter().map(|p| word(p)).filter(|p| !p.is_empty()).collect();
    let readable: String = parts.join(&NAME_SEP.to_string()).chars().take(NAME_MAX).collect();
    if readable.is_empty() { format!("{LOG_PREFIX}{id}{LOG_SUFFIX}") } else { format!("{LOG_PREFIX}{id}{NAME_SEP}{readable}{LOG_SUFFIX}") }
}

/// The logs in the folder, id -> file, or None when the folder cannot be listed. Of two files with one id the
/// one that sorts last is taken.
fn log_files(logs: &Path) -> Option<BTreeMap<String, PathBuf>> {
    let mut names: Vec<String> = std::fs::read_dir(logs).ok()?.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    Some(names.into_iter().filter_map(|name| Some((id_of(&name)?.to_string(), logs.join(&name)))).collect())
}

/// The file of the log with this id.
pub fn find_log(logs: &Path, id: &str) -> Option<PathBuf> {
    if !valid_id(id) {
        return None;
    }
    log_files(logs)?.remove(id)
}

/// A whole log: `{summary, detail, hits}`. None when the file is not one (not gzip, not JSON, too large).
pub fn read_log(path: &Path) -> Option<Value> {
    let mut text = String::new();
    let file = std::fs::File::open(path).ok()?;
    flate2::read::GzDecoder::new(file).take(MAX_LOG_BYTES + 1).read_to_string(&mut text).ok()?;
    if text.len() as u64 > MAX_LOG_BYTES {
        return None;
    }
    serde_json::from_str::<Value>(&text).ok().filter(Value::is_object)
}

/// Writes a log under another name first, so a file in the folder under a log's name is always a whole one.
fn write_log(logs: &Path, doc: &Value) -> std::io::Result<PathBuf> {
    let id = doc["summary"]["id"].as_str().unwrap_or("");
    let (part, path) = (logs.join(format!("{id}.part")), logs.join(file_name(&doc["summary"])));
    let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(&part)?, flate2::Compression::new(5));
    gz.write_all(doc.to_string().as_bytes())?;
    gz.finish()?.sync_all()?;
    std::fs::rename(&part, &path)?;
    // an earlier file of this id under another name (the mission saved before its map was known, or an old name)
    for name in std::fs::read_dir(logs).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()) {
        if id_of(&name) == Some(id) && logs.join(&name) != path {
            let _ = std::fs::remove_file(logs.join(&name));
        }
    }
    Ok(path)
}

/// List rows, oldest first, and whether they differ from what `index.json` holds (a log was missing from it, a
/// row had no log, or there is no index). The rows always match the logs that are in the folder.
pub fn read_index_checked(logs: &Path) -> (Vec<Value>, bool) {
    let stored = std::fs::read_to_string(logs.join("index.json")).ok().and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok());
    let Some(files) = log_files(logs) else { return (stored.unwrap_or_default(), false) };
    let mut changed = stored.is_none() && !files.is_empty();
    let mut rows = stored.unwrap_or_default();
    let before = rows.len();
    let mut listed = std::collections::BTreeSet::new();
    rows.retain(|r| r["id"].as_str().is_some_and(|id| files.contains_key(id) && listed.insert(id.to_string())));
    changed |= rows.len() != before;
    for (id, path) in files.iter().filter(|(id, _)| !listed.contains(*id)) {
        // the row of a log is the summary it carries; the id is the one in its file name
        if let Some(mut row) = read_log(path).and_then(|mut doc| doc.get_mut("summary").map(Value::take)).filter(Value::is_object) {
            row["id"] = json!(id);
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

/// List rows, oldest first; when the stored index was out of step with the folder it is written again, and a log
/// still under a name without its mission gets its readable name.
pub fn sync_index(logs: &Path) -> Vec<Value> {
    let (rows, changed) = read_index_checked(logs);
    if changed {
        let _ = write_index(logs, &rows); // the rows are right either way; the next read tries again
    }
    if let Some(files) = log_files(logs) {
        for row in &rows {
            let Some(old) = row["id"].as_str().and_then(|id| files.get(id)) else { continue };
            let new = logs.join(file_name(row));
            if *old != new && !new.exists() {
                let _ = std::fs::rename(old, &new); // a file that is open elsewhere keeps its name until next time
            }
        }
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
    write_log(logs, &doc)?;
    // The mission is saved. The index is a cache (see the top of this file): failing to update it is not an
    // error, the log is found in the folder the next time the list is read.
    let _ = sync_index(logs);
    Ok(Some(summary))
}

/// What became of a file given to `import`.
#[derive(Debug, PartialEq, Eq)]
pub enum Imported {
    /// added under this id
    Added(String),
    /// the folder already has this very log
    Present(String),
    /// not a log this program can read, and why
    Rejected(String),
}

/// Adds a log file from somewhere else (one that another player sent) to the folder.
///
/// The file is somebody else's data and is treated as such. It has to be a whole log with a usable id. A build in
/// it is cut back to the loadout it was made from, so that what is shown is rendered here from this machine's own
/// game data, never taken from the file. The log is marked as imported. An id that is taken by a different
/// mission gets a number added.
pub fn import(logs: &Path, file: &Path) -> Imported {
    let reject = |why: &str| Imported::Rejected(why.to_string());
    let Some(mut doc) = read_log(file) else { return reject("not a zar-logs log (a gzip-compressed JSON file)") };
    let id = doc["summary"]["id"].as_str().unwrap_or("").to_string();
    let players = doc["detail"]["players"].as_array();
    if !valid_id(&id) || !doc["summary"]["start"].is_number() || !doc["detail"].is_object()
        || !players.is_some_and(|list| list.iter().all(Value::is_object)) {
        return reject("not a zar-logs log (it has no summary or no detail)");
    }
    match doc["detail"]["format"].as_u64() {
        Some(format) if format <= crate::session::FORMAT as u64 => {}
        Some(_) => return reject("recorded by a newer version of zar-logs than this one"),
        None => return reject("not a zar-logs log (it does not say which format it is in)"),
    }
    if std::fs::create_dir_all(logs).is_err() {
        return reject("the logs folder cannot be written");
    }
    // the same mission (start time and mission time) is already here: nothing to do
    let same = |path: &Path| read_log(path).is_some_and(|mine| {
        mine["summary"]["start"] == doc["summary"]["start"] && mine["summary"]["duration"] == doc["summary"]["duration"]
    });
    let mut new_id = id.clone();
    let mut n = 1;
    while let Some(existing) = find_log(logs, &new_id) {
        if same(&existing) {
            return Imported::Present(new_id);
        }
        n += 1;
        new_id = format!("{id}-{n}");
    }
    for part in ["summary", "detail"] {
        doc[part]["id"] = json!(new_id);
        doc[part]["imported"] = json!(true);
    }
    for p in doc["detail"]["players"].as_array_mut().into_iter().flatten().filter_map(Value::as_object_mut) {
        let raw = p.get_mut("build").and_then(|b| b.get_mut("raw")).map(Value::take).filter(Value::is_object);
        p.insert("build".into(), raw.map_or(Value::Null, |raw| json!({ "version": 0, "raw": raw })));
    }
    match write_log(logs, &doc) {
        Ok(_) => {
            let _ = sync_index(logs);
            Imported::Added(new_id)
        }
        Err(e) => reject(&format!("could not be written to the logs folder: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let logs = std::env::temp_dir().join(format!("zar-logs-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&logs);
        std::fs::create_dir_all(&logs).unwrap();
        logs
    }

    fn write_plain(path: &Path, doc: &Value) {
        let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(path).unwrap(), flate2::Compression::new(1));
        gz.write_all(doc.to_string().as_bytes()).unwrap();
        gz.finish().unwrap();
    }

    /// A log under its old name, without a mission in its summary.
    fn old_log(logs: &Path, id: &str, start: f64) -> PathBuf {
        let path = logs.join(format!("enc-{id}.json.gz"));
        write_plain(&path, &json!({ "summary": { "id": id, "start": start }, "detail": {}, "hits": {} }));
        path
    }

    fn ids(rows: &[Value]) -> Vec<&str> { rows.iter().map(|r| r["id"].as_str().unwrap()).collect() }

    #[test]
    fn the_list_follows_the_logs_in_the_folder() {
        let logs = folder("list");

        // no index yet: the list is built from the logs, oldest first, and the index is written
        old_log(&logs, "b", 2.0);
        old_log(&logs, "a", 1.0);
        assert_eq!(ids(&sync_index(&logs)), ["a", "b"]);
        assert_eq!(read_index_checked(&logs).1, false, "the index was written and now agrees with the folder");

        // a log saved without its index update (the update failed, or the app was closed in between) is listed
        old_log(&logs, "c", 3.0);
        let (rows, changed) = read_index_checked(&logs);
        assert_eq!((ids(&rows), changed), (vec!["a", "b", "c"], true));

        // a row whose log was deleted by hand is dropped; a file that is not a log is ignored
        sync_index(&logs);
        std::fs::remove_file(find_log(&logs, "a").unwrap()).unwrap();
        std::fs::write(logs.join("notes.txt"), "mine").unwrap();
        std::fs::write(logs.join("enc-not_a_log.txt"), "mine").unwrap();
        assert_eq!(ids(&sync_index(&logs)), ["b", "c"]);

        // a damaged index is rebuilt
        std::fs::write(logs.join("index.json"), "{ not json").unwrap();
        assert_eq!(ids(&sync_index(&logs)), ["b", "c"]);
        assert_eq!(ids(&serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(logs.join("index.json")).unwrap()).unwrap()), ["b", "c"]);

        std::fs::remove_dir_all(&logs).unwrap();
    }

    fn summary(id: &str, start: f64, map: &str) -> Value {
        json!({ "id": id, "start": start, "duration": 60.0,
                "mission": { "name": map, "type": "Close The Breach", "difficulty": "Hard", "tough": true } })
    }

    #[test]
    fn a_log_is_named_after_its_mission() {
        assert_eq!(file_name(&summary("20261118-143424", 1.0, "Ymir Station")), "enc-20261118-143424_Ymir-Station_Close-The-Breach_Hard-Tough.json.gz");
        assert_eq!(file_name(&json!({ "id": "x1" })), "enc-x1.json.gz");
        // nothing of a name can leave the folder or break the pattern the id is read back from
        let odd = file_name(&json!({ "id": "x1", "mission": { "name": "..\\..\\a/b_c", "type": "", "difficulty": "" } }));
        assert_eq!(odd, "enc-x1_a-b-c.json.gz");
        assert_eq!(id_of(&odd), Some("x1"));
        assert_eq!(id_of("enc-20261118-143424.json.gz"), Some("20261118-143424"));
        assert_eq!(id_of("enc-.json.gz"), None);
        assert_eq!(id_of("index.json"), None);

        // a log under its old name is found by id, and gets its readable name when the list is read
        let logs = folder("names");
        let old = logs.join("enc-20261118-143424.json.gz");
        write_plain(&old, &json!({ "summary": summary("20261118-143424", 1.0, "Geode Cavern"), "detail": {}, "hits": {} }));
        assert_eq!(find_log(&logs, "20261118-143424").as_deref(), Some(old.as_path()));
        assert_eq!(ids(&sync_index(&logs)), ["20261118-143424"]);
        let new = logs.join("enc-20261118-143424_Geode-Cavern_Close-The-Breach_Hard-Tough.json.gz");
        assert!(new.is_file() && !old.exists());
        assert_eq!(find_log(&logs, "20261118-143424"), Some(new));
        assert_eq!(find_log(&logs, "../20261118-143424"), None);
        assert_eq!(ids(&sync_index(&logs)), ["20261118-143424"], "renaming a log does not change the list");
        std::fs::remove_dir_all(&logs).unwrap();
    }

    #[test]
    fn importing_a_log_from_someone_else() {
        let (logs, theirs) = (folder("import"), folder("import-from"));
        let doc = |id: &str, start: f64| json!({
            "summary": summary(id, start, "Terraforming Site"),
            "detail": { "id": id, "format": crate::session::FORMAT, "players": [
                { "name": "A", "build": { "version": 6, "raw": { "cls": "assault" }, "weapons": [{ "icon": "http://elsewhere/x.png" }] } },
                { "name": "B", "build": null }] },
            "hits": { "columns": [], "rows": [] } });
        let file = theirs.join("a.json.gz");
        write_plain(&file, &doc("20261118-145143", 5.0));

        assert_eq!(import(&logs, &file), Imported::Added("20261118-145143".into()));
        let mine = read_log(&find_log(&logs, "20261118-145143").unwrap()).unwrap();
        assert_eq!(mine["summary"]["imported"], true);
        assert_eq!(mine["detail"]["imported"], true);
        // only the loadout is kept of a build: what is shown is rendered here, not taken from the file
        assert_eq!(mine["detail"]["players"][0]["build"], json!({ "version": 0, "raw": { "cls": "assault" } }));
        assert!(mine["detail"]["players"][1]["build"].is_null());
        assert_eq!(ids(&read_index(&logs)), ["20261118-145143"]);

        // the same file again: already there. Another mission with the same id: added under a new one.
        assert_eq!(import(&logs, &file), Imported::Present("20261118-145143".into()));
        write_plain(&file, &doc("20261118-145143", 6.0));
        assert_eq!(import(&logs, &file), Imported::Added("20261118-145143-2".into()));
        assert_eq!(read_log(&find_log(&logs, "20261118-145143-2").unwrap()).unwrap()["detail"]["id"], "20261118-145143-2");

        // files that are not logs
        std::fs::write(&file, "plain text").unwrap();
        assert!(matches!(import(&logs, &file), Imported::Rejected(_)));
        write_plain(&file, &json!({ "summary": { "id": "../x", "start": 1.0 }, "detail": { "format": 8, "players": [] } }));
        assert!(matches!(import(&logs, &file), Imported::Rejected(_)));
        write_plain(&file, &json!({ "summary": { "id": "x", "start": 1.0 }, "detail": { "format": 8, "players": [1] } }));
        assert!(matches!(import(&logs, &file), Imported::Rejected(_)));
        write_plain(&file, &json!({ "summary": { "id": "x", "start": 1.0 }, "detail": { "format": 999, "players": [] } }));
        assert!(matches!(import(&logs, &file), Imported::Rejected(_)));
        write_plain(&file, &json!([1, 2]));
        assert!(matches!(import(&logs, &file), Imported::Rejected(_)));
        assert_eq!(ids(&read_index(&logs)), ["20261118-145143", "20261118-145143-2"]);

        std::fs::remove_dir_all(&logs).unwrap();
        std::fs::remove_dir_all(&theirs).unwrap();
    }
}
