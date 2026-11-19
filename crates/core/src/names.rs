//! Turns ids from the event stream into names (weapons, abilities, enemies, status effects, missions).
//!
//! Live side: the capture sends one `defs` record per mission with the definition tables it read (identity hash, the
//! hash of the base asset for per-player copies, and the localisation key of the title).
//! Offline side: the names pack (`names.json` in the cache folder), generated on this machine from the local game
//! data: asset names by identity hash (CRC-32 of the name), the localisation table, levels, modes, classes, and
//! which ability / upgrade / skill mentions which status-effect set. Without it everything still works and shows
//! raw ids.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{json, Value};

use crate::records::{Defs, PlayerInfo};
use crate::consts;
use crate::session::{raw_attack, Encounter};

fn is_sep(c: char) -> bool { c == '-' || c == '_' || c == ' ' }

/// 'terminate-weapon-residual-energy' -> 'Terminate Residual Energy', 'LoadedChamber_EffectSet' -> 'Loaded Chamber'.
/// A hand translation of the reference's five regular expressions; the names pack carries the reference's result for
/// every asset name, and a test checks this function against all of them.
pub fn pretty(name: &str) -> String {
    let mut s: Vec<char> = name.chars().collect();
    // 1. "_AUTOGEN_STAGE_<digits>" at the end
    {
        let digits = s.iter().rev().take_while(|c| c.is_ascii_digit()).count();
        let tag: Vec<char> = "_AUTOGEN_STAGE_".chars().collect();
        if digits > 0 && s.len() >= digits + tag.len() && s[s.len() - digits - tag.len()..s.len() - digits] == tag[..] {
            s.truncate(s.len() - digits - tag.len());
        }
    }
    // 2. elemental sets: enemy-vs-player / player-vs-enemy variants, with one optional separator before
    {
        let tail: String = s.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
        if matches!(tail.as_str(), "EvP" | "PvE" | "PvP") {
            s.truncate(s.len() - 3);
            if s.last().is_some_and(|&c| is_sep(c)) {
                s.pop();
            }
        }
    }
    // 3. a space between a lower-case letter and an upper-case letter
    let mut t = Vec::with_capacity(s.len() + 8);
    for (i, &c) in s.iter().enumerate() {
        if i > 0 && s[i - 1].is_ascii_lowercase() && c.is_ascii_uppercase() {
            t.push(' ');
        }
        t.push(c);
    }
    // 4. drop "<separators><kind word>" when the word ends there or a separator follows
    const WORDS: [&str; 8] = ["effect set", "effectset", "weapon", "ability", "entity", "set", "throw", "attack"];
    let mut u = Vec::with_capacity(t.len());
    let mut i = 0;
    while i < t.len() {
        if is_sep(t[i]) {
            let mut j = i;
            while j < t.len() && is_sep(t[j]) {
                j += 1;
            }
            let rest: String = t[j..].iter().collect::<String>().to_ascii_lowercase();
            let hit = WORDS.iter().find(|w| rest.starts_with(**w) && rest[w.len()..].chars().next().is_none_or(is_sep));
            if let Some(w) = hit {
                i = j + w.chars().count();
                continue;
            }
        }
        u.push(t[i]);
        i += 1;
    }
    // 5. '_' and most '-' become spaces; a '-' stays only between [A-Z0-9] and a digit ("T-800")
    let v: String = u.iter().enumerate().map(|(i, &c)| match c {
        '_' => ' ',
        '-' => {
            let keep = i > 0 && (u[i - 1].is_ascii_uppercase() || u[i - 1].is_ascii_digit())
                && u.get(i + 1).is_some_and(|n| n.is_ascii_digit());
            if keep { '-' } else { ' ' }
        }
        c => c,
    }).collect();
    // 6. capitalise words that have no capital or digit of their own
    let out = v.split_whitespace().map(|w| {
        if w.chars().any(|c| c.is_uppercase() || c.is_ascii_digit()) {
            w.to_string()
        } else {
            let mut cs = w.chars();
            cs.next().map(|f| f.to_uppercase().collect::<String>() + &cs.as_str().to_lowercase()).unwrap_or_default()
        }
    }).collect::<Vec<_>>().join(" ");
    if out.is_empty() { name.to_string() } else { out }
}

fn norm(s: &str) -> String { s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase() }

/// A title without the game's inline markup: "Return [r]to Regen[/r]" -> "Return to Regen". A tag is a short word
/// in square brackets, with a slash in front when it closes.
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        let tag = tail.find(']').map(|close| &tail[1..close]).filter(|tag| {
            let word = tag.strip_prefix('/').unwrap_or(tag);
            !word.is_empty() && word.len() <= 8 && word.chars().all(|c| c.is_ascii_alphabetic())
        });
        match tag {
            Some(tag) => rest = &tail[tag.len() + 2..],
            None => {
                out.push('[');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A scene path as the same text however it was written: forward slashes, lower case.
fn scene_key(path: &str) -> String { path.replace('\\', "/").to_lowercase() }

/// Who did a hit, as far as its source definition tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attacker {
    /// a weapon or an ability: a player's own
    Player,
    /// an enemy: its attack or its blast, whoever the game credits it to
    Enemy,
    /// something that is neither (a barrel, a turret, an area) or names no definition
    Other,
    /// a definition this mission's tables do not have (yet)
    Unknown,
}

#[derive(Clone, Debug, Default)]
struct Asset {
    name: String,
    pretty: String,
    title: String,
    set_type: i64,
}

#[derive(Clone, Debug)]
pub struct Info {
    pub name: String,
    pub kind: &'static str,
    pub asset: String,
    pub boss: bool,
    /// how the game's entity type reads ("Enemy", "Explosive barrel"); empty for what is not an entity
    pub etype: String,
}

#[derive(Clone, Debug)]
struct SkillRow {
    id: String,
    name: String,
    kind: String,
}

/// What an effect row belongs to: ("ability", ability asset or name, perk) | ("skill" | "mod" | "ally", name, None).
pub type Owner = (&'static str, String, Option<String>);

/// What `effect_owner` needs to know about the player the effect is on.
pub struct OwnerCtx<'a> {
    /// asset names of the abilities other players have equipped
    pub allies: HashSet<String>,
    /// asset names of this player's equipped abilities
    pub abilities: HashSet<String>,
    /// ids of this player's passive skills
    pub skills: HashSet<String>,
    /// ability identity hash -> (asset, name) of the player's ability lanes
    pub by_hash: HashMap<u32, (&'a str, &'a str)>,
    /// how many other players are in the mission
    pub others: usize,
    /// whether the loadout of any of them is known (`allies` is only complete then)
    pub ally_builds: bool,
}

#[derive(Default)]
pub struct Resolver {
    loc: HashMap<String, String>,
    off_weapons: HashMap<u32, Asset>,
    off_abilities: HashMap<u32, Asset>,
    off_entities: HashMap<u32, Asset>,
    off_sets: HashMap<u32, Asset>,
    levels: Vec<(String, String)>,
    /// scene path (see `scene_key`) -> (title key, true for a story mission)
    scenes: HashMap<String, (String, bool)>,
    modes: HashMap<String, String>,
    classes: HashMap<String, String>,
    enhancements: Vec<(String, String)>,
    effect_types: HashMap<u8, String>,
    owners: HashMap<String, Vec<usize>>,
    rows: Vec<SkillRow>,
    row_by_id: HashMap<String, usize>,
    weapons: HashMap<u32, Info>,
    abilities: HashMap<u32, Info>,
    entities: HashMap<u32, Info>,
    sets: HashMap<u32, Info>,
    enemy: HashMap<u32, bool>,
    pub pack_loaded: bool,
    /// Turns a raw loadout into the build the page shows. Set by the app once it has read the game's data; without
    /// it a log keeps the raw loadout, which can be rendered later.
    pub render_build: Option<BuildRenderer>,
}

pub type BuildRenderer = std::sync::Arc<dyn Fn(&Value) -> Value + Send + Sync>;

impl Resolver {
    /// A resolver without the pack: raw ids only.
    pub fn empty() -> Resolver { Resolver::default() }

    pub fn load(pack: &Path) -> Resolver {
        let mut r = Resolver::default();
        let Some(v) = std::fs::read_to_string(pack).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return r };
        let s = |v: &Value| v.as_str().unwrap_or("").to_string();
        let table = |v: &Value| -> HashMap<u32, Asset> {
            v.as_object().into_iter().flatten().filter_map(|(k, d)| Some((k.parse().ok()?, Asset {
                name: s(&d["name"]), pretty: s(&d["pretty"]), title: s(&d["title"]), set_type: d["setType"].as_i64().unwrap_or(0),
            }))).collect()
        };
        r.loc = v["loc"].as_object().into_iter().flatten().map(|(k, x)| (k.clone(), s(x))).collect();
        r.off_weapons = table(&v["weapons"]);
        r.off_abilities = table(&v["abilities"]);
        r.off_entities = table(&v["entities"]);
        r.off_sets = table(&v["sets"]);
        r.levels = v["levels"].as_array().into_iter().flatten().map(|d| (s(&d["key"]), s(&d["title"]))).collect();
        for (list, story) in [("levels", false), ("missions", true)] {
            for d in v[list].as_array().into_iter().flatten().filter(|d| !s(&d["scene"]).is_empty()) {
                r.scenes.insert(scene_key(&s(&d["scene"])), (s(&d["title"]), story));
            }
        }
        r.modes = v["modes"].as_object().into_iter().flatten().map(|(k, x)| (k.clone(), s(x))).collect();
        r.classes = v["classes"].as_object().into_iter().flatten().map(|(k, x)| (k.clone(), s(x))).collect();
        r.enhancements = v["enhancements"].as_array().into_iter().flatten().map(|d| (s(&d["name"]), s(&d["title"]))).collect();
        r.effect_types = v["effect_types"].as_object().into_iter().flatten().filter_map(|(k, x)| Some((k.parse().ok()?, s(x)))).collect();
        for d in v["skills"].as_array().into_iter().flatten() {
            let i = r.rows.len();
            r.rows.push(SkillRow { id: s(&d["id"]), name: s(&d["name"]), kind: s(&d["kind"]) });
            r.row_by_id.insert(s(&d["id"]), i);
            for set in d["sets"].as_array().into_iter().flatten() {
                r.owners.entry(s(set)).or_default().push(i);
            }
        }
        r.pack_loaded = true;
        r
    }

    pub fn text(&self, key: &str) -> String {
        if key.is_empty() { String::new() } else { self.loc.get(key).cloned().unwrap_or_default() }
    }
    fn text_or_pretty(&self, key: &str) -> String {
        if key.is_empty() { return String::new(); }
        let t = self.text(key);
        if t.is_empty() { pretty(key) } else { t }
    }

    // ---- records from the capture ----
    pub fn on_defs(&mut self, enc: Option<&mut Encounter>, rec: &Defs) {
        let first = |t: &HashMap<u32, Asset>, hs: &[u32]| hs.iter().find(|h| **h != 0 && t.contains_key(h)).map(|h| t[h].clone());
        for (h, own, orig, title) in &rec.weapons {
            let a = first(&self.off_weapons, &[*orig, *own, *h]);
            let mut name = self.text(title);
            if name.is_empty() { name = a.as_ref().map(|a| self.text(&a.title)).unwrap_or_default(); }
            if name.is_empty() { name = a.as_ref().map(|a| a.pretty.clone()).unwrap_or_default(); }
            let helper = title.is_empty() && !a.as_ref().is_some_and(|a| !a.title.is_empty());
            let info = Info { name: if name.is_empty() { format!("Weapon {h:08x}") } else { name },
                kind: if helper { "effect" } else { "weapon" }, asset: a.map(|a| a.name).unwrap_or_default(), boss: false, etype: String::new() };
            for k in [*h, *own] {
                if k != 0 { self.weapons.insert(k, info.clone()); }
            }
        }
        for (h, orig, title) in &rec.abilities {
            let a = first(&self.off_abilities, &[*orig, *h]);
            let mut name = self.text(title);
            if name.is_empty() { name = a.as_ref().map(|a| self.text(&a.title)).unwrap_or_default(); }
            if name.is_empty() { name = a.as_ref().map(|a| a.pretty.clone()).unwrap_or_default(); }
            if *h != 0 {
                self.abilities.insert(*h, Info { name: if name.is_empty() { format!("Ability {h:08x}") } else { name },
                    kind: "ability", asset: a.map(|a| a.name).unwrap_or_default(), boss: false, etype: String::new() });
            }
        }
        let c = consts::get();
        for (h, own, orig, etype, boss, gi) in &rec.entities {
            let a = first(&self.off_entities, &[*orig, *own, *h]);
            let (mut name, mut boss) = (String::new(), *boss != 0);
            if let Some((_sid, title, singular, gboss)) = usize::try_from(*gi).ok().and_then(|i| rec.statgroups.get(i)) {
                name = self.text(singular);
                if name.is_empty() { name = self.text(title); }
                boss = boss || *gboss != 0;
            }
            if name.is_empty() {
                // level props (pods, crates, barrels) have per-level definitions without a title: name them by kind
                name = match &a {
                    Some(a) => a.pretty.clone(),
                    None if !c.entity_type(*etype).is_empty() => c.entity_type(*etype),
                    None => format!("Entity {h:08x}"),
                };
            }
            let info = Info { name, kind: "entity", asset: a.map(|a| a.name).unwrap_or_default(), boss, etype: c.entity_type(*etype) };
            for k in [*h, *own] {
                if k != 0 {
                    self.entities.insert(k, info.clone());
                    self.enemy.insert(k, *etype == c.et_enemy);
                }
            }
        }
        for (h, own, orig) in &rec.sets {
            let a = first(&self.off_sets, &[*orig, *own, *h]);
            let info = Info {
                name: a.as_ref().map(|a| a.pretty.clone()).unwrap_or_else(|| format!("Effect {h:08x}")),
                kind: match a.as_ref().map(|a| a.set_type) { Some(1) => "Buff", Some(2) => "Debuff", _ => "" },
                asset: a.map(|a| a.name).unwrap_or_default(), boss: false, etype: String::new(),
            };
            for k in [*h, *own] {
                if k != 0 { self.sets.insert(k, info.clone()); }
            }
        }
        if let Some(enc) = enc {
            enc.mission = self.mission(rec);
            enc.tags = rec.tags.clone();
            enc.bossmods = rec.bossmods.iter().map(|k| self.text_or_pretty(k)).collect();
            enc.enemymods = rec.enemymods.iter().map(|k| self.text_or_pretty(k)).collect();
            if rec.defense.is_some() {
                enc.defense = rec.defense.clone();
            }
            enc.dirty = true;
        }
    }

    /// Attach each player's loadout: rendered as a build when the app has the game's data, else kept raw (the
    /// viewer renders it when the log is opened).
    pub fn on_builds(&self, enc: Option<&mut Encounter>, players: &[PlayerInfo]) {
        let Some(enc) = enc else { return };
        let single = players.len() == 1 && enc.players.len() == 1;
        let mut missing = false;
        for info in players {
            let slot = enc.players.values_mut().find(|p| p.name == info.name);
            let slot = match slot {
                Some(p) => Some(p),
                None if single => enc.players.values_mut().next(),
                None => None,
            };
            match slot {
                Some(p) => {
                    // the loadout as the mission was built from it, minus the account id: that never goes into a log
                    let mut raw = serde_json::to_value(info).unwrap_or(Value::Null);
                    if let Some(m) = raw.as_object_mut() {
                        m.remove("nk");
                    }
                    p.build = Some(match &self.render_build {
                        Some(render) => render(&raw),
                        None => json!({ "version": 0, "raw": raw, "note": "This build has not been rendered yet." }),
                    });
                }
                None => missing = true, // clients not sampled yet: retried from on_sample
            }
        }
        if missing {
            enc.pending_builds = Some(players.to_vec());
        }
        enc.dirty = true;
    }

    pub fn on_sample(&self, enc: &mut Encounter) {
        if enc.pending_builds.is_some() && !enc.players.is_empty() {
            let pend = enc.pending_builds.take().unwrap();
            self.on_builds(Some(enc), &pend);
        }
    }

    pub fn is_boss(&self, hash: u32) -> bool { self.entities.get(&hash).is_some_and(|d| d.boss) }

    /// Who did a hit with this attack type and source definition (see `Attacker`).
    pub fn attacker(&self, attack_type: u8, hash: u32) -> Attacker {
        let c = consts::get();
        if attack_type == c.at_weapon || attack_type == c.at_ability {
            return Attacker::Player;
        }
        if hash == 0 {
            return Attacker::Other;
        }
        match self.enemy.get(&hash) {
            Some(true) => Attacker::Enemy,
            Some(false) => Attacker::Other,
            None if self.weapons.contains_key(&hash) || self.abilities.contains_key(&hash) => Attacker::Player,
            None => Attacker::Unknown,
        }
    }
    /// False only when the target is known not to be an enemy (prop, turret, ...).
    pub fn is_enemy(&self, hash: u32) -> bool { self.enemy.get(&hash).copied().unwrap_or(true) }

    pub fn class_name(&self, cid: &str) -> String {
        self.classes.get(&cid.to_lowercase()).cloned().unwrap_or_else(|| if cid.is_empty() { String::new() } else { pretty(cid) })
    }

    /// The level's title and whether it is a story mission. First by the scene itself: the level descriptor (an
    /// operation's map) or campaign mission (story, prologue) whose scene this is. Without that (an older names
    /// pack), 'Assets/Levels/Operations/Op_Geode_Map.unity' -> 'Geode' title via the level descriptor whose name
    /// the file name contains, and last the file name made readable.
    fn level_name(&self, level_id: &str) -> (String, bool) {
        if let Some((title, story)) = self.scenes.get(&scene_key(level_id)) {
            let t = plain(&self.text(title));
            if !t.is_empty() {
                return (t, *story);
            }
        }
        (self.level_name_by_file(level_id), false)
    }

    fn level_name_by_file(&self, level_id: &str) -> String {
        let file = level_id.replace('\\', "/");
        let file = file.rsplit('/').next().unwrap_or("");
        let stem = file.strip_suffix(".unity").unwrap_or(file);
        let flat = norm(stem);
        if flat.starts_with("op") || flat.starts_with("hub") {
            for (key, title) in &self.levels {
                let k = norm(&key.replace("leveldescriptor", ""));
                if !k.is_empty() && flat.contains(&k) {
                    let t = self.text(title);
                    return if t.is_empty() { pretty(stem) } else { t };
                }
            }
        }
        if stem.is_empty() {
            return String::new();
        }
        let cut = ["Op_", "HUB_", "Hub_"].iter().find_map(|p| stem.strip_prefix(p)).unwrap_or(stem);
        pretty(cut)
    }

    fn mission(&self, rec: &Defs) -> Value {
        let c = consts::get();
        let (mut name, story) = self.level_name(&rec.level);
        if name.is_empty() { name = "Mission".into(); }
        let mut mode = self.text(&rec.mode_title);
        if mode.is_empty() {
            mode = match self.modes.get(&rec.mode.to_lowercase()) {
                Some(title) => self.text(title),
                None => if rec.mode.is_empty() { String::new() } else { pretty(&rec.mode) },
            };
        }
        // The hub (Aegis Station: crew quarters, vendors) is a level like any other and says so itself: its level
        // type. A story mission has no mode either, so "no mode" only stands in where a game build does not give
        // the level's type.
        let hub = match (rec.level_type, c.lt_hub) {
            (Some(kind), Some(hub)) => kind == hub,
            _ => rec.mode.is_empty() && !story,
        };
        if mode.is_empty() && story && !hub {
            // a story mission or prologue has no mode: the game's own word for the kind of mission
            mode = self.text("Story");
            if mode.is_empty() { mode = "Story".into(); }
        }
        // difficulty and the "Tough" modifier are two separate things in the game (LevelDifficulty, LevelModifier)
        json!({ "name": name, "type": mode, "difficulty": c.difficulty(rec.difficulty),
                "tough": rec.modifier != 0, "level_id": rec.level, "mode_id": rec.mode, "avg_level": rec.avg_level,
                "story": story, "hub": hub })
    }

    // ---- lookups used by the encounter model ----
    /// {name, kind, asset?} of a damage source key ("<attack type>:<hash>", "none", "type:<dt>:<how>").
    pub fn attack(&self, key: &str) -> Value {
        if key == "none" || key.starts_with("type:") {
            return raw_attack(key);
        }
        let Some((at, h)) = key.split_once(':') else { return raw_attack(key) };
        let at: usize = at.parse().unwrap_or(usize::MAX);
        let c = consts::get();
        let kind = c.attack_kind(at);
        let kind = kind.as_str();
        let cap = |k: &str| { let mut c = k.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() };
        if let Some(idx) = h.strip_prefix('i') {
            return json!({ "name": format!("{} #{idx}", cap(kind)), "kind": kind });
        }
        let Ok(h) = u32::from_str_radix(h, 16) else { return raw_attack(key) };
        // look first where the game says the damage came from, then in the other tables
        let (by_weapon, by_ability) = (at == c.at_weapon as usize, at == c.at_ability as usize);
        let order: [(u8, &HashMap<u32, Info>); 3] = if by_weapon {
            [(0, &self.weapons), (1, &self.abilities), (2, &self.entities)]
        } else if by_ability {
            [(1, &self.abilities), (0, &self.weapons), (2, &self.entities)]
        } else {
            [(2, &self.entities), (1, &self.abilities), (0, &self.weapons)]
        };
        for (which, table) in order {
            if let Some(d) = table.get(&h) {
                if which == 2 {
                    // damage done by an entity and credited to the player: a turret, a barrel, an area effect
                    // (an enemy's own damage is not counted for a player, but it is named when a player takes it)
                    let k = match d.etype.as_str() {
                        "Enemy" => "enemy", "Turret" | "Drone" => "turret", "Explosive barrel" | "Destructible" => "barrel",
                        "Zone" => "area", "Projectile" => "effect", _ => "entity",
                    };
                    return json!({ "name": d.name, "kind": k, "asset": d.asset });
                }
                return json!({ "name": d.name, "kind": if by_weapon { d.kind } else { kind }, "asset": d.asset });
            }
        }
        json!({ "name": format!("{} {h:08x}", cap(kind)), "kind": kind })
    }

    /// How the game itself names a damage type: its localised text ("Heat" is shown as "Thermal"), else the enum
    /// member's own name.
    pub fn damage_type_name(&self, member: &str) -> String {
        let t = self.text(&format!("Equipment_Stat_DamageType_{member}"));
        if t.is_empty() { member.to_string() } else { t }
    }
    /// An effect category's name as it reads ("SlowDown" -> "Slow Down"). The game has no text for these.
    pub fn effect_category_name(&self, member: &str) -> String { pretty(member) }

    /// {name, boss?, type?} of a target key (identity hash in hex, or "i<index>").
    pub fn entity(&self, key: &str) -> Value {
        if let Some(idx) = key.strip_prefix('i') {
            return json!({ "name": format!("Entity #{idx}") });
        }
        match u32::from_str_radix(key, 16).ok().and_then(|h| self.entities.get(&h)) {
            Some(d) => json!({ "name": d.name, "boss": d.boss, "type": d.etype }),
            None => json!({ "name": format!("Entity {key}") }),
        }
    }

    /// (name, asset) of the ability in a slot.
    pub fn ability(&self, slot: usize, hash: u32) -> (String, String) {
        match self.abilities.get(&hash) {
            Some(d) => (d.name.clone(), d.asset.clone()),
            None => (format!("Ability {}", slot + 1), String::new()),
        }
    }

    /// (name, kind, asset) of an effect key: a set hash in hex, or "t<type>[@owner]" for an effect without a set.
    pub fn effect(&self, key: &str) -> (String, &'static str, String) {
        if let Some(rest) = key.strip_prefix('t') {
            let ty: u8 = rest.split('@').next().and_then(|x| x.parse().ok()).unwrap_or(255);
            return (self.effect_types.get(&ty).cloned().unwrap_or_else(|| format!("Effect type {ty}")), "", String::new());
        }
        match u32::from_str_radix(key, 16).ok().and_then(|h| self.sets.get(&h)) {
            Some(d) => (d.name.clone(), d.kind, d.asset.clone()),
            None => (format!("Effect {key}"), "", String::new()),
        }
    }

    // ---- where an effect comes from ----
    /// Order: the ability the game itself attributes the effect to, then the game data (which ability / upgrade /
    /// skill applies this set), then a teammate's ability, then the unique mods.
    pub fn effect_owner(&self, e: &Value, ctx: &OwnerCtx) -> Option<Owner> {
        let s = |k: &str| e[k].as_str().unwrap_or("");
        let asset = s("asset").to_lowercase();
        let rows: Vec<&SkillRow> = if asset.is_empty() { Vec::new() } else {
            self.owners.get(&asset).into_iter().flatten().map(|&i| &self.rows[i]).collect()
        };
        let live = e["owner"].as_u64().filter(|o| *o != 0).and_then(|o| ctx.by_hash.get(&(o as u32)));
        for r in &rows {
            // an equipped ability, its upgrade or its enhancement
            let aid = r.id.split('/').next().unwrap_or("");
            if r.kind != "passive skill" && ctx.abilities.contains(aid) && live.is_none_or(|l| l.0 == aid) {
                return Some(("ability", aid.to_string(), r.id.contains('/').then(|| r.name.clone())));
            }
        }
        if let Some((l_asset, l_name)) = live {
            // attributed by the game, but not described in the data
            let perk = s("key").starts_with('t').then(|| s("name").to_string());
            return Some(("ability", if l_asset.is_empty() { l_name.to_string() } else { l_asset.to_string() }, perk));
        }
        let passive: Vec<&&SkillRow> = rows.iter().filter(|r| r.kind == "passive skill").collect();
        if !passive.is_empty() {
            let best = passive.iter().find(|r| norm(&r.id) == norm(&asset))
                .or_else(|| passive.iter().find(|r| ctx.skills.contains(&r.id))).unwrap_or(&passive[0]);
            return Some(("skill", best.name.clone(), None));
        }
        // An ability the player does not have: a teammate's. Only when there is a teammate, and not when the game
        // names the player's own entity as the giver (alone in a mission, the invulnerability at the end of it is
        // nobody's Cleansing Pulse).
        let teammate = ctx.others > 0 && s("from") != "self";
        for r in rows.iter().filter(|_| teammate) {
            let aid = r.id.split('/').next().unwrap_or("");
            if let Some(&bi) = self.row_by_id.get(aid) {
                if !ctx.ally_builds || ctx.allies.contains(aid) {
                    let base = &self.rows[bi].name;
                    return Some(("ally", if r.id.contains('/') { format!("{base} ({})", r.name) } else { base.clone() }, None));
                }
            }
        }
        if !asset.is_empty() {
            // "<name>_EffectSet" / "<name>_Set" against the unique mods' asset names
            let cut = ["_effect_set", "_effectset", "effect_set", "effectset", "_set", "set"].iter()
                .find_map(|suf| asset.strip_suffix(suf)).unwrap_or(&asset);
            let key = norm(cut);
            for (name, title) in &self.enhancements {
                let short = name.replace("ModEnhancement_", "");
                let nm = norm(&short);
                if !nm.is_empty() && (key.starts_with(&nm) || nm.starts_with(&key)) && key.len() >= 4 {
                    let t = self.text(title);
                    return Some(("mod", if t.is_empty() { pretty(&short) } else { t }, None));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_without_markup() {
        assert_eq!(plain("Return [r]to Regen[/r]"), "Return to Regen");
        assert_eq!(plain("[r]Ruptured[/r] Reality"), "Ruptured Reality");
        assert_eq!(plain("Wave [3] of [many more] [x"), "Wave [3] of [many more] [x");
    }

    fn resolver() -> Resolver {
        let mut r = Resolver::default();
        r.loc.insert("Story_ReturnRegen".into(), "Return [r]to Regen[/r]".into());
        r.loc.insert("Location_Hub".into(), "Aegis Station".into());
        r.loc.insert("Story".into(), "Story".into());
        r.scenes.insert(scene_key("Assets/Levels/Mission_Trusgan_ReturnRegen.unity"), ("Story_ReturnRegen".into(), true));
        r.scenes.insert(scene_key("Assets/Levels/Hubs/HUB_AegisStation.unity"), ("Location_Hub".into(), false));
        // a set that one ability's upgrade applies
        r.rows.push(SkillRow { id: "cleansing-pulse".into(), name: "Cleansing Pulse".into(), kind: "active ability".into() });
        r.rows.push(SkillRow { id: "cleansing-pulse/Pain Inhibitors".into(), name: "Pain Inhibitors".into(), kind: "ability upgrade".into() });
        r.row_by_id.insert("cleansing-pulse".into(), 0);
        r.owners.insert("invulnerability".into(), vec![1]);
        r
    }

    #[test]
    fn a_story_mission_is_a_mission_and_the_hub_is_not() {
        let c = consts::install_for_tests();
        let r = resolver();
        let (hub, mission) = (c.lt_hub.unwrap(), c.lt_hub.unwrap() + 1);
        // a story mission has no mode, like the hub: the level's own type tells them apart
        let story = r.mission(&Defs { level: "Assets\\Levels\\Mission_Trusgan_ReturnRegen.unity".into(), level_type: Some(mission), ..Default::default() });
        assert_eq!((story["name"].as_str(), story["type"].as_str(), story["hub"].as_bool()), (Some("Return to Regen"), Some("Story"), Some(false)));
        let station = r.mission(&Defs { level: "Assets/Levels/Hubs/HUB_AegisStation.unity".into(), level_type: Some(hub), ..Default::default() });
        assert_eq!((station["name"].as_str(), station["type"].as_str(), station["hub"].as_bool()), (Some("Aegis Station"), Some(""), Some(true)));
        // a level this program has no title for is still a mission, named by its file
        let prologue = r.mission(&Defs { level: "Assets/Levels/Prologue_Trusgan_1.unity".into(), level_type: Some(mission), ..Default::default() });
        assert_eq!((prologue["name"].as_str(), prologue["hub"].as_bool()), (Some("Prologue Trusgan 1"), Some(false)));
        // a game build that does not give the level's type: no mode means the hub, unless it is a known story mission
        let untyped = |level: &str| r.mission(&Defs { level: level.into(), ..Default::default() })["hub"].as_bool();
        assert_eq!((untyped("Assets/Levels/Hubs/HUB_AegisStation.unity"), untyped("Assets/Levels/Mission_Trusgan_ReturnRegen.unity")), (Some(true), Some(false)));
    }

    #[test]
    fn nobody_elses_ability_when_alone() {
        consts::install_for_tests();
        let r = resolver();
        let ctx = |others: usize| OwnerCtx { allies: HashSet::new(), abilities: HashSet::new(), skills: HashSet::new(), by_hash: HashMap::new(),
            others, ally_builds: false };
        let effect = |from: &str| json!({ "key": "4f26b399", "asset": "Invulnerability", "name": "Invulnerability", "owner": 0, "from": from });
        // alone in a mission, the invulnerability at the end of it is not a teammate's Cleansing Pulse
        assert_eq!(r.effect_owner(&effect(""), &ctx(0)), None);
        assert_eq!(r.effect_owner(&effect(""), &ctx(2)), Some(("ally", "Cleansing Pulse (Pain Inhibitors)".to_string(), None)));
        assert_eq!(r.effect_owner(&effect("self"), &ctx(2)), None);
    }
}
