//! The app's view of the logs folder (`zar_logs_core::store` has the files' format and names): listing, reading,
//! deleting and importing logs, and the things a log is shown with that belong to this machine (pictures).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::{json, Map, Value};

pub struct Store {
    pub logs: PathBuf,
    /// root of the cache: one folder per game version underneath (see gamedata.rs)
    pub cache: PathBuf,
    /// the game version's folder in use: names, pictures and tables are read from here
    game: std::sync::RwLock<PathBuf>,
    icons: Mutex<(Option<SystemTime>, HashMap<String, String>)>,
    write: Mutex<()>,
}

impl Store {
    pub fn new(logs: PathBuf, cache: PathBuf) -> Store {
        let game = std::sync::RwLock::new(cache.join("game").join("none"));
        Store { logs, cache, game, icons: Mutex::new((None, HashMap::new())), write: Mutex::new(()) }
    }

    /// The folder of the game version in use.
    pub fn game_dir(&self) -> PathBuf { self.game.read().unwrap().clone() }
    pub fn set_game_dir(&self, dir: PathBuf) {
        *self.game.write().unwrap() = dir;
        *self.icons.lock().unwrap() = (None, HashMap::new()); // another folder, another picture index
    }

    fn index_path(&self) -> PathBuf { self.logs.join("index.json") }

    /// Changes whenever the list of logs does: a log was added to or removed from the folder, or the index was
    /// written.
    pub fn version(&self) -> u64 {
        let stamp = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
        stamp(&self.index_path()).max(stamp(&self.logs))
    }

    /// The list rows, oldest first: the index, checked against the logs that are really in the folder.
    fn rows(&self) -> Vec<Value> {
        let _guard = self.write.lock().unwrap();
        zar_logs_core::store::sync_index(&self.logs)
    }

    /// List rows, newest first.
    pub fn list(&self) -> Value {
        let mut rows = self.rows();
        rows.reverse();
        for r in &mut rows {
            self.decorate(r);
        }
        Value::Array(rows)
    }

    /// The file of a log, by its id (an id is letters, digits and '-': it cannot name anything outside the folder).
    pub fn path_of(&self, id: &str) -> Option<PathBuf> { zar_logs_core::store::find_log(&self.logs, id) }

    pub fn detail(&self, id: &str) -> Option<Value> {
        let mut d = zar_logs_core::store::read_log(&self.path_of(id)?)?.get_mut("detail")?.take();
        self.decorate(&mut d);
        Some(d)
    }

    /// Removes one log (the page's Delete button). The user's own files in the logs folder; nothing of the game's.
    pub fn delete(&self, id: &str) -> bool {
        let _guard = self.write.lock().unwrap();
        let gone = match self.path_of(id) {
            Some(path) => std::fs::remove_file(&path).is_ok() || !path.exists(),
            None => zar_logs_core::store::valid_id(id),
        };
        zar_logs_core::store::sync_index(&self.logs); // drops the row of the log that is gone
        gone
    }

    /// Adds logs from files the user chose (see `zar_logs_core::store::import`): what became of each.
    pub fn import(&self, files: &[PathBuf]) -> Value {
        use zar_logs_core::store::Imported;
        let _guard = self.write.lock().unwrap();
        let (mut added, mut present, mut rejected) = (Vec::new(), 0, Vec::new());
        for file in files {
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match zar_logs_core::store::import(&self.logs, file) {
                Imported::Added(id) => added.push(id),
                Imported::Present(_) => present += 1,
                Imported::Rejected(why) => rejected.push(json!({ "file": name, "why": why })),
            }
        }
        json!({ "added": added, "present": present, "rejected": rejected })
    }

    // ---- pictures extracted from the local install: cache/icons/index.json maps a key to a file name ----
    fn icon(&self, key: &str) -> Option<String> {
        let path = self.game_dir().join("icons").join("index.json");
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut g = self.icons.lock().unwrap();
        if g.0 != mtime {
            let map = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("icons")?.as_object().cloned()).unwrap_or_default();
            *g = (mtime, map.into_iter().filter_map(|(k, v)| Some((k, v.as_str()?.to_string()))).collect());
        }
        g.1.get(key).map(|name| format!("/icons/{name}"))
    }

    /// The game's portrait of an enemy kind: the longest-reaching match of a portrait's name in the row's names.
    fn enemy_icon(&self, name: &str, asset: &str, kind: Option<&str>, boss: bool) -> String {
        if name == "Boss pod" {
            return self.icon("ui:bosspod").unwrap_or_default();
        }
        if !boss && !matches!(kind, Some("Enemy") | None) {
            return String::new();
        }
        let flat = format!("{asset} {name}").to_lowercase();
        let _ = self.icon("ui:boss"); // make sure the index is loaded
        let g = self.icons.lock().unwrap();
        let mut best: Option<(usize, &String)> = None;
        for (key, file) in g.1.iter().filter(|(k, _)| k.starts_with("enemy:")) {
            if let Some(at) = flat.rfind(&key[6..]) {
                if best.is_none_or(|(b, _)| at > b) {
                    best = Some((at, file));
                }
            }
        }
        match best {
            Some((_, file)) => format!("/icons/{file}"),
            None if boss => g.1.get("ui:boss").map(|f| format!("/icons/{f}")).unwrap_or_default(),
            None => String::new(),
        }
    }

    /// Things that depend on this machine rather than on the log: where the map's picture and the enemy portraits
    /// are, and the difficulty in the game's own two parts (difficulty, Tough). Works on a list row and a full log.
    pub fn decorate(&self, d: &mut Value) {
        if let Some(m) = d.get_mut("mission").and_then(Value::as_object_mut) {
            let diff = m.get("difficulty").and_then(Value::as_str).unwrap_or("").to_string();
            if !m.contains_key("tough") {
                m.insert("tough".into(), json!(diff.contains("Tough")));
            }
            m.insert("difficulty".into(), json!(diff.replace("Tough", "").trim_matches(|c| c == ' ' || c == '·')));
            let level = m.get("level_id").and_then(Value::as_str).unwrap_or("").replace('\\', "/").to_lowercase();
            m.insert("art".into(), json!(self.icon(&format!("scene:{level}")).unwrap_or_default()));
        }
        let target_rows = |rows: Option<&mut Value>| {
            for t in rows.and_then(Value::as_array_mut).into_iter().flatten().filter_map(Value::as_object_mut) {
                let s = |t: &Map<String, Value>, k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let icon = self.enemy_icon(&s(t, "name"), &s(t, "asset"), t.get("type").and_then(Value::as_str),
                    t.get("boss").and_then(Value::as_bool).unwrap_or(false));
                t.insert("icon".into(), json!(icon));
            }
        };
        target_rows(d.get_mut("targets"));
        for p in d.get_mut("players").and_then(Value::as_array_mut).into_iter().flatten() {
            target_rows(p.get_mut("targets"));
        }
        for b in d.get_mut("bosses").and_then(Value::as_array_mut).into_iter().flatten().filter_map(Value::as_object_mut) {
            let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let icon = self.enemy_icon(&name, "", Some("Enemy"), true);
            b.insert("icon".into(), json!(icon));
        }
    }
}
