//! The app's view of the logs folder (`zar_logs_core::store` has the files' format and names): listing, reading,
//! deleting and importing logs, and the things a log is shown with that belong to this machine (pictures).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::{json, Map, Value};
use zar_logs_core::session::{self, Counted};

pub struct Store {
    pub logs: PathBuf,
    /// root of the cache: one folder per game version underneath (see gamedata.rs)
    pub cache: PathBuf,
    /// the game version's folder in use: names, pictures and tables are read from here
    game: std::sync::RwLock<PathBuf>,
    icons: Mutex<(Option<SystemTime>, HashMap<String, String>)>,
    /// how the game sets a mission's strength, with the game version's folder it was read from
    rules: Mutex<Option<(PathBuf, Rules)>>,
    /// the missions the Records page ranks, with what they were read with
    records: Mutex<Option<(RecordsKey, std::sync::Arc<(Vec<Run>, usize)>)>>,
    /// the game versions this machine has a folder for, with the time the cache's folder list was read at
    versions: Mutex<Option<(Option<SystemTime>, Vec<Known>)>>,
    write: Mutex<()>,
}

/// What the Records page depends on: the logs folder's stamp, the game version's folder, what is counted.
type RecordsKey = (u64, PathBuf, Counted);

/// A game version the app has read: its id, Steam build, and when it was installed (the newest of its files' times).
#[derive(Clone, Debug, PartialEq)]
struct Known {
    id: String,
    build: Option<String>,
    installed: u64,
}

impl Store {
    pub fn new(logs: PathBuf, cache: PathBuf) -> Store {
        let game = std::sync::RwLock::new(cache.join("game").join("none"));
        Store { logs, cache, game, icons: Mutex::new((None, HashMap::new())), rules: Mutex::new(None),
                records: Mutex::new(None), versions: Mutex::new(None), write: Mutex::new(()) }
    }

    /// The game versions with a folder in the cache (the app keeps the last few), from their manifests.
    fn known_versions(&self) -> Vec<Known> {
        let root = self.cache.join("game");
        let stamp = std::fs::metadata(&root).and_then(|m| m.modified()).ok();
        let mut g = self.versions.lock().unwrap();
        if let Some((_, v)) = g.as_ref().filter(|(s, _)| *s == stamp) {
            return v.clone();
        }
        let mut out = Vec::new();
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let Some(m) = std::fs::read_to_string(e.path().join("manifest.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
            let installed = m["files"].as_array().into_iter().flatten().filter_map(|f| f["modified"].as_u64()).max().unwrap_or(0);
            if let (Some(id), true) = (m["game_version"].as_str(), installed > 0) {
                out.push(Known { id: id.to_string(), build: m["steam_build"].as_str().map(str::to_string), installed });
            }
        }
        *g = Some((stamp, out.clone()));
        out
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

    /// How the game version in use sets a mission's strength (see `Rules`), from its tables.
    fn rules(&self) -> Rules {
        let dir = self.game_dir();
        let mut g = self.rules.lock().unwrap();
        if let Some((_, v)) = g.as_ref().filter(|(d, _)| *d == dir) {
            return v.clone();
        }
        let tables = std::fs::read_to_string(dir.join("tables.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let v = tables.as_ref().map(|t| Rules::of(&t["definitions"])).unwrap_or_default();
        *g = Some((dir, v.clone()));
        v
    }

    /// Things that depend on this machine rather than on the log: where the map's picture and the enemy portraits
    /// are, the difficulty in the game's own two parts (difficulty, Tough), and the mission's level: `level`, the
    /// game's own value where the log has it and otherwise worked out from the players' levels (`level_from` says
    /// which). Works on a list row and a full log.
    pub fn decorate(&self, d: &mut Value) {
        // a log from before logs said which game version they were played on: the version that was installed then
        if d.is_object() && !d.get("game").is_some_and(|g| g.is_object()) {
            let start = d.get("start").and_then(Value::as_f64).unwrap_or(0.0);
            if let Some(k) = version_at(&self.known_versions(), start) {
                d["game"] = json!({ "id": k.id, "build": k.build, "inferred": true });
            }
        }
        let levels: Vec<i64> = d.get("players").and_then(Value::as_array).into_iter().flatten()
            .map(|p| p.get("lvl").and_then(Value::as_i64).unwrap_or(0)).collect();
        let scaling = d.get("scaling").cloned();
        if let Some(m) = d.get_mut("mission").and_then(Value::as_object_mut) {
            let diff = m.get("difficulty").and_then(Value::as_str).unwrap_or("").to_string();
            if !m.contains_key("tough") {
                m.insert("tough".into(), json!(diff.contains("Tough")));
            }
            m.insert("difficulty".into(), json!(diff.replace("Tough", "").trim_matches(|c| c == ' ' || c == '·')));
            let tough = m.get("tough").and_then(Value::as_bool).unwrap_or(false);
            let level = match m.get("avg_level").and_then(Value::as_i64).filter(|l| *l > 0) {
                Some(l) => Some((l, "game")),
                None => mission_level(&levels, tough, || self.rules().tough).map(|l| (l, "computed")),
            };
            if let Some((l, from)) = level {
                m.insert("level".into(), json!(l));
                m.insert("level_from".into(), json!(from));
            }
            // how strong the enemies were made: the game's own multipliers where the log has them, else worked out
            // from the mission level and the number of players
            let scaling = match scaling.as_ref().filter(|s| s.is_object()) {
                Some(s) => Some(with(s.clone(), "from", json!("game"))),
                None => level.and_then(|(l, _)| self.rules().scaling(l, levels.len())).map(|s| with(s, "from", json!("computed"))),
            };
            if let Some(s) = scaling {
                m.insert("scaling".into(), with(s, "players", json!(levels.len())));
            }
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

    /// The Records page: the best missions of this machine's own characters (see `records`). One pass over the
    /// logs, kept until the folder, the game version or what is counted changes.
    ///
    /// Records are kept per game version, so a balance patch starts them afresh: `want` is a version id, "all", or
    /// None for the version being played (or, without it, the newest one there are missions of).
    pub fn records(&self, counted: Counted, want: Option<&str>) -> Value {
        let current = self.game_dir().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let missions = self.missions(counted);
        let (runs, skipped) = &*missions;
        // the versions there are missions of, newest first
        let mut versions: Vec<(String, Value, f64, usize)> = Vec::new();
        for r in runs.iter() {
            match versions.iter_mut().find(|v| v.0 == r.version) {
                Some(v) => { v.2 = v.2.max(r.start); v.3 += 1; }
                None => versions.push((r.version.clone(), r.game.clone(), r.start, 1)),
            }
        }
        versions.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let chosen = match want {
            Some("all") => "all".to_string(),
            Some(id) if versions.iter().any(|v| v.0 == id) => id.to_string(),
            _ if versions.iter().any(|v| v.0 == current) => current.clone(),
            _ => versions.first().map(|v| v.0.clone()).unwrap_or_else(|| "all".into()),
        };
        let picked: Vec<&Run> = runs.iter().filter(|r| chosen == "all" || r.version == chosen).collect();
        let mut out = records(&picked);
        out["skipped"] = json!(skipped);
        out["version"] = json!(chosen);
        out["versions"] = versions.iter().map(|(id, game, last, n)| json!({ "id": id, "build": game["build"],
            "inferred": game["inferred"].as_bool() == Some(true), "last": last, "missions": n, "current": *id == current }))
            .collect();
        out
    }

    /// Every mission the Records page ranks, and how many logs could not be read. One pass over the logs, kept
    /// until the folder, the game version in use or what is counted changes.
    fn missions(&self, counted: Counted) -> std::sync::Arc<(Vec<Run>, usize)> {
        let key = (self.version(), self.game_dir(), counted);
        if let Some((k, v)) = self.records.lock().unwrap().as_ref() {
            if *k == key {
                return v.clone();
            }
        }
        let mut runs = Vec::new();
        let mut skipped = 0;
        for row in self.rows() {
            // another person's file: its "local" player is them
            if row["imported"].as_bool() == Some(true) || row["mission"]["hub"].as_bool() == Some(true) {
                continue;
            }
            let id = row["id"].as_str().unwrap_or("");
            let detail = self.path_of(id).and_then(|p| zar_logs_core::store::read_log(&p)).and_then(|mut l| Some(l.get_mut("detail")?.take()));
            match detail {
                Some(mut d) => {
                    self.decorate(&mut d);
                    session::view(&mut d, counted);
                    runs.extend(Run::of(&d));
                }
                None => skipped += 1,
            }
        }
        let out = std::sync::Arc::new((runs, skipped));
        *self.records.lock().unwrap() = Some((key, out.clone()));
        out
    }
}

/// The game version installed at `start` (a log's start, seconds since 1970): the known version installed last
/// before it. A version this machine no longer has a folder for cannot be told, so this is a best guess.
fn version_at(known: &[Known], start: f64) -> Option<&Known> {
    known.iter().filter(|k| (k.installed as f64) <= start).max_by_key(|k| k.installed)
}

/// `v` with one more field.
fn with(mut v: Value, key: &str, x: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.insert(key.into(), x);
    }
    v
}

/// How the game sets a mission's strength, from the definitions of one game version (`WorldDifficultySettings`
/// and what it refers to, `LevelSet`). Read in the game's code (`WorldController.PrepareWorldDataBuilder`,
/// `WorldLoadDataBuilder.BuildData`, `WorldDifficultySettings.GetDifficultyMultiplier`): the mission level is the
/// players' mean level, rounded down, plus `toughPlayerLevelOffset` with Tough; enemies are scaled by the base
/// difficulty curve at that level times a multiplier for the number of players.
#[derive(Clone, Debug, Default)]
struct Rules {
    /// what Tough adds to the mission level
    tough: Option<i64>,
    /// the multiplier for 1, 2, ... players (`numPlayersDifficultyMultiplier`)
    team: Vec<f64>,
    /// the base difficulty curve, by level (`DifficultyCurveDefinitionSO.difficultyCurve`)
    curve: Vec<f64>,
}

impl Rules {
    /// From the tables' `definitions`. A rule the tables do not have (made by an older app), or that two objects
    /// of the game disagree on, stays unknown.
    fn of(defs: &Value) -> Rules {
        let one = |xs: Vec<Value>| xs.first().cloned().filter(|x| xs.iter().all(|y| y == x));
        let tough = one(defs["LevelSet"].as_object().into_iter().flatten()
            .filter_map(|(_, set)| set.get("toughPlayerLevelOffset").cloned()).collect()).and_then(|v| v.as_i64());
        let settings = one(defs["WorldDifficultySettings"].as_object().into_iter().flatten().map(|(_, s)| s.clone()).collect())
            .unwrap_or(Value::Null);
        let floats = |v: &Value| v.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        let curve_id = settings["baseDifficultyCurveDefinition"]["m_PathID"].as_i64().map(|i| i.to_string()).unwrap_or_default();
        let curve = floats(&defs["DifficultyCurveDefinitionSO"][&curve_id]["difficultyCurve"]);
        Rules { tough, team: floats(&settings["numPlayersDifficultyMultiplier"]), curve }
    }

    /// The load header's multipliers for a mission of this level and number of players, in the shape the logs have
    /// them (`session::Encounter::scaling`); None when a rule is unknown. As `GetDifficultyMultiplier`: the curve at
    /// the level (at least 1), times the team's multiplier.
    fn scaling(&self, level: i64, players: usize) -> Option<Value> {
        let at = self.curve.get(level.max(1) as usize).copied()?;
        let team = self.team.get(players.checked_sub(1)?).copied()?;
        Some(json!({ "level": at, "team": team, "enemies": at * team }))
    }
}

/// A mission's level the way the game works it out (`WorldDataBuildParams.averagePlayerLevel`): the mean of the
/// players' levels rounded down, plus `LevelSet.toughPlayerLevelOffset` when the Tough modifier is on. Only for logs
/// without the game's own value, which every log since 0.1 records (`mission.avg_level`); checked against those.
fn mission_level(levels: &[i64], tough: bool, offset: impl FnOnce() -> Option<i64>) -> Option<i64> {
    let levels: Vec<i64> = levels.iter().copied().filter(|l| *l > 0).collect();
    if levels.is_empty() {
        return None;
    }
    let mean = levels.iter().sum::<i64>().div_euclid(levels.len() as i64);
    if tough { offset().map(|o| mean + o) } else { Some(mean) }
}

/// One mission of one of this machine's characters, as far as the Records page needs it.
struct Run {
    id: String,
    start: f64,
    /// the character, by its name: the logs hold nothing steadier (the client id is per mission, and the account
    /// id is not stored)
    name: String,
    cls: String,
    sub: String,
    /// where the mission is ranked: one entry per operation type (the game mode), one per story mission (its level)
    group: String,
    group_title: String,
    story: bool,
    mission: Value,
    duration: f64,
    party: usize,
    won: bool,
    party_dps: f64,
    dps: f64,
    xp: f64,
    /// the best boss of the mission for the party and for the character: (DPS on it, fight length, boss name)
    team_boss: Option<(f64, f64, String)>,
    own_boss: Option<(f64, f64, String)>,
    /// the game version it was played on: its id ("" when not known), and `{id, build, inferred?}` as the log has it
    version: String,
    game: Value,
}

impl Run {
    fn of(d: &Value) -> Option<Run> {
        let me = d["players"].as_array()?.iter().find(|p| p["local"].as_bool() == Some(true))?;
        let m = &d["mission"];
        let text = |v: &Value| v.as_str().unwrap_or("").to_string();
        let story = m["story"].as_bool() == Some(true);
        let (group, group_title) = if story {
            (format!("story:{}", text(&m["level_id"]).to_lowercase()), text(&m["name"]))
        } else {
            let mode = text(&m["mode_id"]);
            let title = text(&m["type"]);
            (format!("op:{}", if mode.is_empty() { title.to_lowercase() } else { mode.to_lowercase() }), title)
        };
        let cid = me["cid"].to_string();
        let mut team_boss: Option<(f64, f64, String)> = None;
        let mut own_boss: Option<(f64, f64, String)> = None;
        for b in d["bosses"].as_array().into_iter().flatten() {
            // over the time it was being hit, as the boss card's "DPS on it"
            let (Some(first), Some(end)) = (b["first_hit"].as_f64(), b["end"].as_f64()) else { continue };
            let fight = end - first;
            if fight <= 0.01 {
                continue;
            }
            let name = text(&b["name"]);
            let team = b["damage"].as_f64().unwrap_or(0.0) / fight;
            let own = b["series"][&cid].as_array().into_iter().flatten().filter_map(Value::as_f64).sum::<f64>() / fight;
            if team > 0.0 && team_boss.as_ref().is_none_or(|t| team > t.0) {
                team_boss = Some((team, fight, name.clone()));
            }
            if own > 0.0 && own_boss.as_ref().is_none_or(|t| own > t.0) {
                own_boss = Some((own, fight, name));
            }
        }
        let keep = ["name", "type", "difficulty", "tough", "level", "level_from", "scaling", "story", "art"];
        let mission = Value::Object(keep.iter().filter_map(|k| Some((k.to_string(), m.get(*k)?.clone()))).collect());
        Some(Run {
            id: text(&d["id"]), start: d["start"].as_f64().unwrap_or(0.0),
            name: text(&me["name"]), cls: text(&me["cls"]), sub: text(&me["sub"]),
            group, group_title, story, mission,
            duration: d["duration"].as_f64().unwrap_or(0.0),
            party: d["players"].as_array().map_or(0, Vec::len),
            won: d["outcome"].as_str() == Some("won"),
            party_dps: d["party_dps"].as_f64().unwrap_or(0.0),
            dps: me["dps"].as_f64().unwrap_or(0.0),
            xp: d["xp"].as_f64().unwrap_or(0.0),
            team_boss, own_boss,
            version: text(&d["game"]["id"]), game: d["game"].clone(),
        })
    }

    fn row(&self, value: f64, boss: Option<&(f64, f64, String)>) -> Value {
        let mut r = json!({ "id": self.id, "start": self.start, "name": self.name, "cls": self.cls, "sub": self.sub,
            "value": value, "mission": self.mission, "duration": self.duration, "party": self.party, "won": self.won,
            "build": self.game["build"] });
        if let Some((_, fight, name)) = boss {
            r["fight"] = json!(fight);
            r["boss"] = json!(name);
        }
        r
    }
}

/// One record: every character's best, best first. `value` gives a run's figure and its boss (None: the run does
/// not count); `low` is for a time, where less is better.
fn ranked<'a>(runs: impl Iterator<Item = &'a Run>, key: &str, title: &str, unit: &str, low: bool,
              value: impl Fn(&Run) -> Option<(f64, Option<&(f64, f64, String)>)>) -> Value {
    let mut best: Vec<(f64, Value, String)> = Vec::new();
    for run in runs {
        let Some((v, boss)) = value(run) else { continue };
        if !v.is_finite() || v <= 0.0 {
            continue;
        }
        let better = |old: f64| if low { v < old } else { v > old };
        match best.iter_mut().find(|b| b.2 == run.name) {
            Some(b) if !better(b.0) => {}
            Some(b) => *b = (v, run.row(v, boss), run.name.clone()),
            None => best.push((v, run.row(v, boss), run.name.clone())),
        }
    }
    best.sort_by(|a, b| {
        let o = a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal);
        if low { o } else { o.reverse() }
    });
    json!({ "key": key, "title": title, "unit": unit, "rows": best.into_iter().map(|b| b.1).collect::<Vec<_>>() })
}

/// The Records page from the missions of this machine's own characters: records over all missions, then the same
/// per operation type and per story mission.
fn records(runs: &[&Run]) -> Value {
    fn per_min(r: &Run) -> Option<(f64, Option<&(f64, f64, String)>)> {
        (r.xp > 0.0 && r.duration > 0.0).then(|| (r.xp / (r.duration / 60.0), None))
    }
    let global = vec![
        ranked(runs.iter().copied(), "team_dps", "Highest team DPS", "dps", false, |r| Some((r.party_dps, None))),
        ranked(runs.iter().copied(), "team_boss", "Highest team DPS on a boss", "dps", false, |r| r.team_boss.as_ref().map(|b| (b.0, Some(b)))),
        ranked(runs.iter().copied(), "own_dps", "Highest personal DPS", "dps", false, |r| Some((r.dps, None))),
        ranked(runs.iter().copied(), "own_boss", "Highest personal DPS on a boss", "dps", false, |r| r.own_boss.as_ref().map(|b| (b.0, Some(b)))),
        ranked(runs.iter().copied(), "xp", "Most XP from one mission", "xp", false, |r| Some((r.xp, None))),
        ranked(runs.iter().copied(), "xp_min", "Most XP per minute", "xp_min", false, per_min),
    ];
    let mut groups: Vec<(&str, &str, bool)> = Vec::new();
    for r in runs {
        if !groups.iter().any(|g| g.0 == r.group) {
            groups.push((&r.group, &r.group_title, r.story));
        }
    }
    // operations first, then the story missions, each by name
    groups.sort_by(|a, b| (a.2, a.1).cmp(&(b.2, b.1)));
    let missions: Vec<Value> = groups.iter().map(|(g, title, story)| {
        let of = || runs.iter().copied().filter(|r| r.group == *g);
        // the section's picture: the map of its newest mission
        let art = of().max_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal))
            .map(|r| r.mission["art"].clone()).unwrap_or(Value::Null);
        json!({ "key": g, "title": title, "story": story, "count": of().count(), "art": art, "records": [
            ranked(of(), "team_dps", "Highest team DPS", "dps", false, |r| Some((r.party_dps, None))),
            ranked(of(), "team_boss", "Highest team DPS on a boss", "dps", false, |r| r.team_boss.as_ref().map(|b| (b.0, Some(b)))),
            ranked(of(), "own_dps", "Highest personal DPS", "dps", false, |r| Some((r.dps, None))),
            ranked(of(), "own_boss", "Highest personal DPS on a boss", "dps", false, |r| r.own_boss.as_ref().map(|b| (b.0, Some(b)))),
            ranked(of(), "xp", "Most XP", "xp", false, |r| Some((r.xp, None))),
            ranked(of(), "xp_min", "Most XP per minute", "xp_min", false, per_min),
            ranked(of(), "clear", "Fastest clear", "time", true, |r| r.won.then_some((r.duration, None))),
        ] })
    }).collect();
    let mut names: Vec<&str> = runs.iter().map(|r| r.name.as_str()).collect();
    names.sort();
    names.dedup();
    json!({ "missions_counted": runs.len(), "characters": names, "global": global, "missions": missions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mission_level_is_the_rounded_down_mean_plus_the_tough_offset() {
        // the parties of recorded logs, with the game's own value they carry
        assert_eq!(mission_level(&[60, 56, 33], false, || Some(5)), Some(49));
        assert_eq!(mission_level(&[60, 56, 33], true, || Some(5)), Some(54));
        assert_eq!(mission_level(&[60, 19], false, || None), Some(39));
        assert_eq!(mission_level(&[60, 56, 33, 19], false, || None), Some(42));
        // Tough without the game's offset: unknown rather than a guess
        assert_eq!(mission_level(&[60], true, || None), None);
        assert_eq!(mission_level(&[0], false, || None), None);
    }

    #[test]
    fn mission_strength_is_the_curve_at_the_level_times_the_team_multiplier() {
        let defs = json!({
            "LevelSet": { "1": { "toughPlayerLevelOffset": 5 } },
            "WorldDifficultySettings": { "2": { "numPlayersDifficultyMultiplier": [1.0, 1.7, 2.5, 3.0],
                                                "baseDifficultyCurveDefinition": { "m_FileID": 0, "m_PathID": 9 } } },
            "DifficultyCurveDefinitionSO": { "9": { "difficultyCurve": [0.0, 1.0, 2.0, 4.0] }, "8": { "difficultyCurve": [7.0, 7.0, 7.0, 7.0] } },
        });
        let r = Rules::of(&defs);
        assert_eq!(r.tough, Some(5));
        let s = r.scaling(3, 3).unwrap();
        assert_eq!((s["level"].as_f64(), s["team"].as_f64(), s["enemies"].as_f64()), (Some(4.0), Some(2.5), Some(10.0)));
        assert_eq!(r.scaling(0, 1).unwrap()["level"], 1.0); // level 0 counts as 1, as in the game
        assert!(r.scaling(9, 1).is_none() && r.scaling(1, 5).is_none() && r.scaling(1, 0).is_none());
        assert!(Rules::of(&json!({})).scaling(1, 1).is_none());
    }

    #[test]
    fn an_old_log_gets_the_version_installed_before_it() {
        let k = |id: &str, installed| Known { id: id.into(), build: None, installed };
        let known = [k("new", 200), k("old", 100)];
        assert_eq!(version_at(&known, 150.0).map(|k| k.id.as_str()), Some("old"));
        assert_eq!(version_at(&known, 250.0).map(|k| k.id.as_str()), Some("new"));
        assert_eq!(version_at(&known, 50.0), None); // before any version this machine knows
    }

    fn log(id: &str, name: &str, mode: &str, dps: f64, xp: f64, duration: f64, won: bool, boss: Option<(f64, f64, f64)>) -> Value {
        let bosses: Vec<Value> = boss.into_iter().map(|(first, end, dmg)| json!({ "name": "Boss", "first_hit": first, "end": end,
            "damage": dmg * 2.0, "series": { "1": [dmg * 0.5, dmg * 0.5] } })).collect();
        json!({ "id": id, "start": 1.0, "duration": duration, "outcome": if won { "won" } else { "lost" }, "xp": xp,
                "party_dps": dps * 2.0, "bosses": bosses,
                "mission": { "name": "Map", "type": mode, "mode_id": format!("Mode{mode}"), "story": false, "difficulty": "Hard", "level": 54 },
                "players": [{ "cid": 1, "name": name, "cls": "Medic", "sub": "", "dps": dps, "local": true },
                            { "cid": 2, "name": "Someone else", "cls": "Heavy", "sub": "", "dps": 1e9, "local": false }] })
    }

    #[test]
    fn records_keep_each_characters_best_and_rank_them() {
        let runs: Vec<Run> = [
            log("a", "Vic", "Pods", 100.0, 6000.0, 60.0, true, Some((10.0, 12.0, 1000.0))),
            log("b", "Vic", "Pods", 300.0, 3000.0, 30.0, true, None),
            log("c", "Engineer", "Rescue", 200.0, 9000.0, 120.0, false, Some((5.0, 5.5, 100.0))),
        ].iter().filter_map(Run::of).collect();
        let r = records(&runs.iter().collect::<Vec<_>>());
        let rows = |v: &Value| v["rows"].as_array().unwrap().iter().map(|x| (x["name"].as_str().unwrap().to_string(), x["id"].as_str().unwrap().to_string())).collect::<Vec<_>>();
        let g = &r["global"];
        // one row per character, the other player's figures never count
        assert_eq!(rows(&g[2]), vec![("Vic".into(), "b".into()), ("Engineer".into(), "c".into())]);
        assert_eq!(g[2]["rows"][0]["value"], 300.0);
        // boss DPS over the time from the first hit to the end: Vic 1000 over 2 s, Engineer 100 over 0.5 s
        assert_eq!(g[3]["rows"][0]["value"], 500.0);
        assert_eq!(g[3]["rows"][1]["value"], 200.0);
        assert_eq!(g[3]["rows"][1]["fight"], 0.5);
        assert_eq!(g[1]["rows"][0]["value"], 1000.0); // the party's damage to it: twice the player's
        // XP per minute: 6000 in 1 min against 3000 in 0.5 min is a tie at 6000, 9000 in 2 min is 4500
        assert_eq!(g[5]["rows"][1]["value"], 4500.0);
        // per operation type; a lost mission is no clear
        let m = r["missions"].as_array().unwrap();
        assert_eq!(m.iter().map(|x| x["title"].as_str().unwrap()).collect::<Vec<_>>(), vec!["Pods", "Rescue"]);
        assert_eq!(m[0]["records"][6]["key"], "clear");
        assert_eq!(m[0]["records"][6]["rows"][0]["value"], 30.0);
        assert!(m[1]["records"][6]["rows"].as_array().unwrap().is_empty());
    }
}
