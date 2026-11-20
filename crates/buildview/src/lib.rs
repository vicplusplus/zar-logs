//! Build viewer: turns a player's captured loadout (ids, qualities, levels, mods) into a structured build.
//!
//! Input is the loadout the mission was built from (read from the game's own player list), i.e. exactly what the
//! game simulated, for every player in the mission. Mod values have no hidden rolls: id + quality + power offset
//! (+ enhancement) determine them, so they are computed (`zar-logs-gearcalc`), and skills come from the class
//! table (`zar-logs-namespack`). Anything that cannot be decoded is shown by id rather than dropped.
//!
//! Nothing about the game is written in this file. Numbers come from the game's definitions; slot, quality, stat
//! and damage-type names come from the game's enums and its own localisation texts; and all of it is read from the
//! stored tables of the installed game version (`Tables`).
//!
//! The output mirrors the game's own loadout screens (armour / weapons / deployables with their overviews, ability
//! loadout, skill rows), so the page can lay it out the same way instead of as a wall of text. The numbers equal the
//! Python reference's `Builder.render`; the labels are the game's.

use std::collections::HashMap;

use serde_json::{json, Map, Value};
use zar_logs_core::names::pretty;
use zar_logs_gearcalc::{Gear, ModSpec, StatValue};
use zar_logs_namespack::SkillRow;

pub type Definitions = HashMap<String, Map<String, Value>>;

/// The version of the structure `render` produces; the page checks it.
pub const VERSION: u64 = 8;
/// The definition classes `BuildData` reads names, texts and icon references from.
pub const CLASSES: &[&str] = &["WeaponDefinitionSO", "ArmourPieceSO", "GearModEnhancementDefinitionSO", "AbilityDefinitionSO",
    "PassiveSkillDefinitionSO", "PlayerClassDefinitionSO"];
/// The game enums the labels are taken from.
pub const ENUMS: &[&str] = &["ItemQuality", "GearType", "GearModType", "DamageType", "WeaponCategory", "DeployableCategory",
    "AbilityDescriptionParameterSource"];
/// The member of `AbilityDescriptionParameterSource` for a number that is a parameter of one of the entry's bonuses.
const FROM_BONUS: &str = "BonusParameter";
/// How the game writes the value of a bonus as a stat line (`BonusHelpers.AppendBonusStats`), for the bonus types
/// (`BonusType` members) that the enhancements without a description text have: a percentage, or a plain number.
/// A bonus of any other type gets no line here rather than a guessed unit.
const BONUS_PERCENT: &[&str] = &["MovementSpeed", "ReloadTime", "WeaponMagazineCapacity", "LiftUpTime", "WeaponChargeSpeedMultiplier"];
const BONUS_PLAIN: &[&str] = &["ModPowerCapacity"];

/// A number with up to `digits` decimals, trailing zeros dropped: 2.50 -> "2.5", 3.00 -> "3".
fn num(v: f64, digits: usize) -> String {
    let s = format!("{v:.digits$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}
/// "+12.5%" / "-3.0%".
fn signed_pct(v: f64) -> String { format!("{v:+.1}%") }
/// A flat amount: "+12", "-3", "+2.5".
fn signed_flat(v: f64) -> String { if v.fract() == 0.0 { format!("{v:+.0}") } else { format!("{v:+.1}") } }
/// Python's `round(x, nd)`: correctly rounded on the exact binary value.
fn rnd(x: f64, nd: usize) -> f64 { format!("{x:.nd$}").parse().unwrap_or(x) }

/// The identity hash of an asset: CRC-32 of its name, the name taken as Latin-1 (a character outside it counts
/// as '?', as the reference does).
fn crc(name: &str) -> u32 {
    let bytes: Vec<u8> = name.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect();
    crc32fast::hash(&bytes)
}

/// `text.format(*vals)` for the placeholders the game's texts use: `{0}`, `{1}`, ... and `{}`. None when the text
/// has anything else in braces or asks for a value that is not there (the caller then shows the text as is).
fn fill(text: &str, vals: &[String]) -> Option<String> {
    let (mut out, mut auto, mut rest) = (String::new(), 0usize, text);
    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with('}') {
            return None;
        }
        let end = tail.find('}')?;
        let inner = &tail[1..end];
        let i = if inner.is_empty() { auto += 1; auto - 1 } else { inner.parse::<usize>().ok()? };
        out.push_str(vals.get(i)?);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str { v.get(key).and_then(Value::as_str).unwrap_or("") }
fn i(v: &Value, key: &str) -> i64 { v.get(key).and_then(Value::as_i64).unwrap_or(0) }
/// An enum-valued field as the dumps hold it: a bare number or `{"value": n}`.
fn enum_field(v: &Value, key: &str) -> Option<i64> {
    let f = v.get(key)?;
    f.as_i64().or_else(|| f.get("value")?.as_i64())
}

/// Member names of an enum by value: the first declared member wins a shared value ("Legendary" over "Last").
fn members(enums: &Value, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (member, value) in enums.get(name).and_then(Value::as_object).into_iter().flatten() {
        let Some(at) = value.as_u64().map(|v| v as usize).filter(|v| *v < 4096) else { continue };
        if out.len() <= at {
            out.resize(at + 1, String::new());
        }
        if out[at].is_empty() {
            out[at] = member.clone();
        }
    }
    out
}

/// Every value the app takes from the game's data, read in one go: definitions (weapons, mods, armour,
/// enhancements, abilities, skills, classes), the localisation text, enum values, the few factors that exist only
/// in the game's code, the class skills table and the names pack. It is made from the installed game whenever the
/// game's version changes and stored next to that version's memory layout, so nothing about the game's numbers
/// lives in this program's source.
pub struct Tables {
    /// class name -> object id -> definition, for the classes the viewer, the gear maths and the pictures read;
    /// plus the game's GameRunner component (its master lists) when it was found
    pub definitions: Definitions,
    pub loc_xml: String,
    /// {EnumName: {Member: value}}
    pub enums: Value,
    /// {"values": {name: number}, "verified": bool, ...}: factors read from the game's code (see `read_game`)
    pub code_constants: Value,
    /// Addressables GUID -> asset path, for the asset references of the GameRunner lists
    pub guids: Map<String, Value>,
    pub skills: Vec<SkillRow>,
    /// the names pack (`zar-logs-namespack`)
    pub names: Value,
    /// names this program looks for that this game version no longer has (`zar_logs_namespack::check`): empty
    /// when all is well, otherwise an update renamed or removed something and a cell may be off
    pub notes: Vec<String>,
    /// the numbers in the descriptions of the unique gear mods, per quality (`zar_logs_namespack::enhancement_numbers`)
    pub enhancement_numbers: Map<String, Value>,
}

fn collect_guids(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(g) = m.get("m_AssetGUID").and_then(Value::as_str).filter(|g| !g.is_empty()) {
                out.push(g.to_string());
            }
            m.values().for_each(|x| collect_guids(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| collect_guids(x, out)),
        _ => {}
    }
}

impl Tables {
    /// The definition classes kept in the stored tables.
    pub fn classes() -> Vec<&'static str> {
        let mut out: Vec<&str> = CLASSES.to_vec();
        for c in zar_logs_gearcalc::CLASSES.iter().chain(zar_logs_unity::pictures::CLASSES.iter()) {
            if !out.contains(c) {
                out.push(c);
            }
        }
        out
    }

    /// Reads everything from the installed game: definitions and texts from its asset bundles, enum values from
    /// its metadata, and the code-only factors from its machine code.
    ///
    /// The factors are the one thing that can fail to be read after a game update (their extraction recognises a
    /// code shape). Then the values of `previous` (the `code_constants` of the game version used before) are
    /// carried over and marked unverified, so the app keeps working and says so, rather than use a number from
    /// its own source.
    pub fn read_game(game_dir: &std::path::Path, previous: Option<&Value>) -> Result<Tables, String> {
        let keep = Tables::classes();
        let mut classes = keep.clone();
        for c in zar_logs_namespack::CLASSES {
            if !classes.contains(c) {
                classes.push(c);
            }
        }
        let mut so = zar_logs_unity::so::dump(game_dir, &classes)?;
        let catalog = zar_logs_unity::catalog::Catalog::open(&zar_logs_unity::catalog::path(game_dir))?.table();
        let guid_to_path = |guid: &str| catalog.get(guid).and_then(|l| l.first()).and_then(|l| l.internal_id.clone());
        let mut guids = Map::new();
        if let Some(runner) = zar_logs_unity::so::game_runner(game_dir)? {
            let mut found = Vec::new();
            collect_guids(&Value::Object(runner.clone()), &mut found);
            for g in found {
                if let Some(path) = guid_to_path(&g) {
                    guids.insert(g, json!(path));
                }
            }
            // same shape as the reference dump of this object: {where it was found: object}
            so.insert(zar_logs_gearcalc::GAME_RUNNER.to_string(), Map::from_iter([("game".to_string(), Value::Object(runner))]));
        }
        let loc_xml = zar_logs_unity::loc::english(game_dir)?;
        let mut names: Vec<&str> = zar_logs_namespack::ENUMS.to_vec();
        for e in zar_logs_gearcalc::ENUMS.iter().chain(ENUMS) {
            if !names.contains(e) {
                names.push(e);
            }
        }
        let enums = zar_logs_layoutgen::enums(game_dir, &names)?;
        let code_constants = match zar_logs_layoutgen::code_constants(game_dir) {
            // the generator hands back {name: number, ..., "evidence": {...}}
            Ok(read) => {
                let values: Map<String, Value> = read.as_object().into_iter().flatten()
                    .filter(|(k, _)| k.as_str() != "evidence").map(|(k, v)| (k.clone(), v.clone())).collect();
                json!({ "values": values, "evidence": read["evidence"], "verified": true })
            }
            Err(why) => match previous.filter(|p| p["values"].is_object()) {
                Some(p) => json!({ "values": p["values"], "verified": false, "problem": why,
                                   "note": "carried over from the previous game version: not read from this version's code" }),
                None => return Err(format!("the factors that exist only in the game's code could not be read: {why}")),
            },
        };
        let names_pack = zar_logs_namespack::build(&so, &loc_xml, &enums, &guid_to_path)?;
        let skills = zar_logs_namespack::skill_rows(&so, &loc_xml, &enums, &guid_to_path)?;
        let notes = zar_logs_namespack::check(&so, &enums);
        let enhancement_numbers = zar_logs_namespack::enhancement_numbers(&so, &enums);
        so.retain(|class, _| keep.contains(&class.as_str()) || class == zar_logs_gearcalc::GAME_RUNNER);
        Ok(Tables { definitions: so, loc_xml, enums, code_constants, guids, skills, names: names_pack, notes, enhancement_numbers })
    }

    /// The tables as one JSON document (the names pack is stored separately, as its own file).
    pub fn to_json(&self) -> Value {
        json!({
            "definitions": self.definitions,
            "loc_xml": self.loc_xml,
            "enums": self.enums,
            "code_constants": self.code_constants,
            "guids": self.guids,
            "notes": self.notes,
            "enhancement_numbers": self.enhancement_numbers,
            "skills": { "columns": SkillRow::COLUMNS, "rows": self.skills.iter().map(|r| r.cells().to_vec()).collect::<Vec<_>>() },
        })
    }

    pub fn from_json(mut v: Value, names: Value) -> Result<Tables, String> {
        let definitions = v["definitions"].as_object().ok_or("tables: no definitions")?.iter()
            .filter_map(|(class, objects)| Some((class.clone(), objects.as_object()?.clone()))).collect();
        if v["skills"]["columns"].as_array().map(|c| c.iter().filter_map(Value::as_str).collect::<Vec<_>>()) != Some(SkillRow::COLUMNS.to_vec()) {
            return Err("tables: the skills table has other columns than this version reads".into());
        }
        let skills = v["skills"]["rows"].as_array().ok_or("tables: no skill rows")?.iter().map(|row| {
            let c: Vec<String> = row.as_array().into_iter().flatten().map(|x| x.as_str().unwrap_or("").to_string()).collect();
            let cell = |n: usize| c.get(n).cloned().unwrap_or_default();
            SkillRow { cls: cell(0), subclass: cell(1), kind: cell(2), point_type: cell(3), id: cell(4), name: cell(5),
                unlock_level: cell(6), max_rank: cell(7), requires: cell(8), cooldown_s: cell(9), duration_s: cell(10),
                cost: cell(11), description: cell(12), per_rank: cell(13), max_rank_bonus: cell(14), dps_relevant: cell(15),
                data: cell(16) }
        }).collect();
        Ok(Tables { definitions, loc_xml: v["loc_xml"].as_str().ok_or("tables: no localisation text")?.to_string(),
            enums: v["enums"].take(), code_constants: v["code_constants"].take(),
            notes: v["notes"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(str::to_string)).collect(),
            enhancement_numbers: v["enhancement_numbers"].as_object().cloned().unwrap_or_default(),
            guids: v["guids"].as_object().cloned().unwrap_or_default(), skills, names })
    }
}

/// Everything `render` needs, derived once from the tables.
pub struct BuildData {
    gear: Gear,
    skills: Vec<SkillRow>,
    row_by_id: HashMap<String, usize>,
    loc: HashMap<String, String>,
    /// lower-case class id -> its title
    classes: HashMap<String, String>,
    /// class -> identity hash -> definition
    defs: HashMap<&'static str, HashMap<u32, Value>>,
    /// icon key (asset GUID or `ui:<name>`) -> file name in the icon cache
    icons: HashMap<String, String>,
    // enum member names by value
    qualities: Vec<String>,
    gear_types: Vec<String>,
    mod_types: Vec<String>,
    damage_types: Vec<String>,
    deployables: Vec<String>,
    /// enhancement asset name -> per description parameter -> its number by mod quality
    enhancement_numbers: Map<String, Value>,
    /// value of `AbilityDescriptionParameterSource.BonusParameter`
    from_bonus: Option<i64>,
    /// `BonusType` member names by value
    bonus_types: Vec<String>,
}

/// Adds the damage types that an armour definition carries a rating for, wherever in the object the ratings sit.
fn rated(v: &Value, out: &mut Vec<usize>) {
    match v {
        Value::Object(m) => {
            if let (Some(t), true) = (enum_field(v, "damageType"), m.contains_key("defenceRating")) {
                if t >= 0 && !out.contains(&(t as usize)) {
                    out.push(t as usize);
                }
            }
            m.values().for_each(|x| rated(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| rated(x, out)),
        _ => {}
    }
}

impl BuildData {
    /// From the stored tables of one game version (no access to the game needed). `icons_index` is the picture
    /// cache's `index.json` (pictures are optional).
    pub fn from_tables(t: &Tables, icons_index: &std::path::Path) -> Result<BuildData, String> {
        let guid_to_path = |guid: &str| t.guids.get(guid).and_then(Value::as_str).map(str::to_string);
        let gear = Gear::new(&t.definitions, &t.loc_xml, &t.enums, &t.code_constants["values"], &guid_to_path)?;
        let loc: HashMap<String, String> = t.names["loc"].as_object().into_iter().flatten()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
        let icons = std::fs::read_to_string(icons_index).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v.get("icons")?.as_object().cloned()).unwrap_or_default()
            .into_iter().filter_map(|(k, v)| Some((k, v.as_str()?.to_string()))).collect();
        let so = &t.definitions;
        let mut defs: HashMap<&'static str, HashMap<u32, Value>> = HashMap::new();
        for class in CLASSES {
            let table = so.get(*class).into_iter().flat_map(|m| m.values())
                .filter(|d| !s(d, "_name").is_empty()).map(|d| (crc(s(d, "_name")), d.clone())).collect();
            defs.insert(class, table);
        }
        let mut classes = HashMap::new();
        for d in defs["PlayerClassDefinitionSO"].values() {
            let id = s(d, "className");
            if !id.is_empty() {
                let title = loc.get(s(d, "title")).cloned().filter(|t| !t.is_empty()).unwrap_or_else(|| pretty(id));
                classes.insert(id.to_lowercase(), title);
            }
        }
        let row_by_id = t.skills.iter().enumerate().map(|(n, r)| (r.id.clone(), n)).collect();
        Ok(BuildData { gear, skills: t.skills.clone(), row_by_id, loc, classes, defs, icons,
            qualities: members(&t.enums, "ItemQuality"), gear_types: members(&t.enums, "GearType"),
            mod_types: members(&t.enums, "GearModType"), damage_types: members(&t.enums, "DamageType"),
            deployables: members(&t.enums, "DeployableCategory"), enhancement_numbers: t.enhancement_numbers.clone(),
            from_bonus: t.enums["AbilityDescriptionParameterSource"][FROM_BONUS].as_i64(), bonus_types: members(&t.enums, "BonusType") })
    }

    /// Straight from the installed game (reads its files). Returns the tables too, for a caller that stores them.
    pub fn from_game(game_dir: &std::path::Path, icons_index: &std::path::Path) -> Result<(BuildData, Tables), String> {
        let tables = Tables::read_game(game_dir, None)?;
        Ok((BuildData::from_tables(&tables, icons_index)?, tables))
    }

    fn text(&self, key: &str) -> String { if key.is_empty() { String::new() } else { self.loc.get(key).cloned().unwrap_or_default() } }
    /// The game's text for `key`, else `fallback`.
    fn text_or(&self, key: &str, fallback: impl FnOnce() -> String) -> String {
        let t = self.text(key);
        if t.is_empty() { fallback() } else { t }
    }
    fn class_name(&self, cid: &str) -> String {
        self.classes.get(&cid.to_lowercase()).cloned().unwrap_or_else(|| if cid.is_empty() { String::new() } else { pretty(cid) })
    }
    fn def(&self, class: &str, asset: &str) -> Option<&Value> {
        if asset.is_empty() { None } else { self.defs.get(class)?.get(&crc(asset)) }
    }
    /// URL of an extracted picture. `reference` = an asset reference object from the game data, or a key string.
    fn icon(&self, reference: Option<&Value>) -> Value {
        let key = match reference {
            Some(Value::String(k)) => k.as_str(),
            Some(v) => s(v, "m_AssetGUID"),
            None => "",
        };
        self.icons.get(key).map(|name| json!(format!("/icons/{name}"))).unwrap_or(Value::Null)
    }
    fn row(&self, id: &str) -> Option<&SkillRow> { self.row_by_id.get(id).map(|n| &self.skills[*n]) }

    // ---- labels: the game's enum members, shown with the game's own texts ----
    fn member(list: &[String], value: i64) -> &str {
        usize::try_from(value).ok().and_then(|n| list.get(n)).map(String::as_str).unwrap_or("")
    }
    fn quality_name(&self, q: i64) -> String {
        let m = Self::member(&self.qualities, q);
        if m.is_empty() { format!("Q{q}") } else { self.text_or(&format!("Equipment_Quality_{m}"), || m.to_string()) }
    }
    fn damage_type_name(&self, member: &str) -> String {
        if member.is_empty() { String::new() } else { self.text_or(&format!("Equipment_Stat_DamageType_{member}"), || member.to_string()) }
    }
    /// The name of a gear slot from its GearType: the game's armour slot text, else the member's own words.
    fn gear_slot(&self, gear_type: i64) -> String {
        let m = Self::member(&self.gear_types, gear_type);
        if m.is_empty() { "Gear".into() } else { self.text_or(&format!("Equipment_ArmourType_{m}"), || pretty(m)) }
    }
    /// (id, label, picture) of a mod slot from its GearModType: "WeaponOptic" -> ("Optic", "Optic Mod", ...).
    fn mod_slot(&self, mod_type: usize) -> (String, String, Value) {
        let m = self.mod_types.get(mod_type).map(String::as_str).unwrap_or("");
        let id = m.strip_prefix("Weapon").unwrap_or(m).to_string();
        let label = self.text_or(&format!("Mod_{id}"), || id.clone());
        // the slot glyphs are stored under the slot's id, or under the first word of its label
        let mut icon = self.icon(Some(&json!(format!("ui:{id}"))));
        if icon.is_null() {
            icon = self.icon(Some(&json!(format!("ui:{}", label.split(' ').next().unwrap_or("")))));
        }
        (id, label, icon)
    }
    /// How the game names a stat on a mod card: "<type> Resistance" for a resistance, else the bonus type's text.
    fn stat_label(&self, v: &StatValue) -> String {
        let bonus = self.text_or(&format!("BonusStat_{}", v.bonus), || pretty(&v.bonus));
        match &v.damage_type {
            Some(dt) => {
                let name = self.damage_type_name(dt);
                fill(&self.text("Equipment_Stat_PlayerResistance"), std::slice::from_ref(&name)).filter(|t| !t.is_empty())
                    .unwrap_or_else(|| format!("{name} {bonus}"))
            }
            None => bonus,
        }
    }
    fn is_deployable(&self, gear_type: i64) -> bool { Self::member(&self.gear_types, gear_type) == "Deployable" }
    fn is_armour(&self, gear_type: i64) -> bool {
        let m = Self::member(&self.gear_types, gear_type);
        !m.is_empty() && !self.text(&format!("Equipment_ArmourType_{m}")).is_empty()
    }
    /// A slot that holds a weapon: its GearType is neither an armour slot (those have armour texts) nor deployable.
    fn is_weapon(&self, gear_type: i64) -> bool {
        let m = Self::member(&self.gear_types, gear_type);
        !m.is_empty() && m != "None" && !self.is_deployable(gear_type) && !self.is_armour(gear_type)
    }
    /// Number of mod slots of a weapon: the mod types before "Armour".
    fn weapon_mod_slots(&self) -> usize { self.mod_types.iter().position(|m| m == "Armour").unwrap_or(0) }

    // ---- mods ----
    /// One mod slot, as the game's mod card shows it.
    fn mod_card(&self, m: Option<&Value>, mod_type: usize) -> Value {
        let (slot, slot_label, icon) = self.mod_slot(mod_type);
        let Some(m) = m.filter(|m| !s(m, "id").is_empty() || !s(m, "enh").is_empty()) else {
            return json!({ "slot": slot, "slot_label": slot_label, "empty": true });
        };
        let (q, id, enh) = (i(m, "quality"), s(m, "id"), s(m, "enh"));
        let mut out = match json!({ "slot": slot, "slot_label": slot_label, "empty": false, "quality": q, "qname": self.quality_name(q),
            "power": null, "stats": [], "values": {}, "resists": [], "enh": null, "id": id, "icon": icon }) { Value::Object(o) => o, _ => unreachable!() };
        if !id.is_empty() {
            // an enhanced mod also gets the enhancement's extra stat points (checked against the in-game mod card
            // and the armour overview of a real loadout)
            if let Some(st) = self.gear.mod_stats(id, q as u8, i(m, "offset") as i32, !enh.is_empty()) {
                out.insert("power".into(), json!(st.power));
                out.insert("values".into(), Value::Object(st.values.iter().map(|v| (v.code.clone(), json!(v.value))).collect()));
                out.insert("stats".into(), json!(st.values.iter().map(|v| json!([self.stat_label(v), if v.flat { signed_flat(v.value) } else { signed_pct(v.value) }])).collect::<Vec<_>>()));
                // what each value resists, for the armour overview
                out.insert("resists".into(), json!(st.values.iter().filter_map(|v| Some(json!([v.damage_type.clone()?, v.value]))).collect::<Vec<_>>()));
            }
        }
        if !enh.is_empty() {
            let d = self.def("GearModEnhancementDefinitionSO", enh);
            let mut name = d.map(|d| self.text(s(d, "title"))).unwrap_or_default();
            if name.is_empty() {
                name = pretty(&enh.replace("ModEnhancement_", ""));
            }
            let mut desc = d.map(|d| self.text(s(d, "description"))).unwrap_or_default();
            // "{0}% damage bonus ...": the number for the quality of the mod it sits on
            let vals: Vec<String> = self.enhancement_numbers.get(enh).and_then(Value::as_array).into_iter().flatten().map(|by_quality| {
                usize::try_from(q).ok().and_then(|at| by_quality.get(at)).and_then(Value::as_f64).map(|v| num(v, 2)).unwrap_or_else(|| "?".into())
            }).collect();
            if !vals.is_empty() {
                if let Some(filled) = fill(&desc, &vals) {
                    desc = filled;
                }
            }
            // Some enhancements have no description in the game: what they do is a plain bonus (Weapon Sling: lift-up
            // time), which the game shows as a stat line. The same lines here: the bonus's own label, and its first
            // parameter at the rank the mod's quality gives.
            let mut stats: Vec<Value> = Vec::new();
            for bonus in d.filter(|_| desc.is_empty()).and_then(|d| d.get("bonuses")).and_then(Value::as_array).into_iter().flatten() {
                let member = enum_field(bonus, "bonusType").and_then(|v| usize::try_from(v).ok()).and_then(|v| self.bonus_types.get(v)).map(String::as_str).unwrap_or("");
                let Some(p) = bonus.get("parameters").and_then(Value::as_array).and_then(|list| list.first()) else { continue };
                let value = zar_logs_namespack::parameter_at_rank(p, q, 0);
                let shown = if BONUS_PERCENT.contains(&member) {
                    format!("{}{}%", if value >= 0.0 { "+" } else { "-" }, num(value.abs(), 2))
                } else if BONUS_PLAIN.contains(&member) {
                    signed_flat(value)
                } else {
                    continue;
                };
                stats.push(json!([self.text_or(&format!("BonusStat_{member}"), || pretty(member)), shown]));
            }
            out.insert("enh".into(), json!({ "name": name, "desc": desc, "stats": stats,
                                             "icon": d.map(|d| self.icon(d.get("icon"))).unwrap_or(Value::Null) }));
        }
        Value::Object(out)
    }

    /// The mod cards of a gear item, one per mod type given.
    fn mods(&self, mods: Option<&Value>, types: impl Iterator<Item = usize>) -> Vec<Value> {
        let list = mods.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        types.enumerate().map(|(n, mod_type)| self.mod_card(list.get(n), mod_type)).collect()
    }
    fn mod_power(mods: &[Value]) -> Value {
        let total: f64 = mods.iter().filter_map(|m| m.get("power").and_then(Value::as_f64)).sum();
        if total.fract() == 0.0 { json!(total as i64) } else { json!(total) }
    }

    // ---- gear ----
    fn weapon(&self, g: &Value) -> Value {
        let (q, lvl, id) = (i(g, "quality"), i(g, "level"), s(g, "id"));
        let slot = self.gear_slot(i(g, "type"));
        if id.is_empty() {
            return json!({ "slot": slot, "empty": true });
        }
        let mods = self.mods(g.get("mods"), 0..self.weapon_mod_slots());
        let wd = self.def("WeaponDefinitionSO", id);
        let mut icon = self.icon(wd.and_then(|d| d.get("menuIcon")));
        if icon.is_null() {
            icon = self.icon(wd.and_then(|d| d.get("ingameIcon")));
        }
        let mut out = match json!({ "slot": slot, "empty": false, "name": pretty(id), "category": "", "dtype": "", "quality": q,
            "qname": self.quality_name(q), "level": lvl, "max_level": self.gear.weapon_upgrade_steps(id),
            "mod_power": Self::mod_power(&mods), "mods": mods, "stats": [], "icon": icon }) {
            Value::Object(o) => o, _ => unreachable!() };
        let Some(w) = self.gear.weapon(id) else { return Value::Object(out) };
        out.insert("name".into(), json!(w.display));
        out.insert("category".into(), json!(self.text_or(&format!("Equipment_WeaponCategory_{}", w.catname), || w.catname.clone())));
        out.insert("dtype".into(), json!(self.damage_type_name(&w.dtype_name)));
        let specs: Vec<ModSpec> = g.get("mods").and_then(Value::as_array).into_iter().flatten().enumerate()
            .filter_map(|(n, m)| ModSpec::from_loadout(n, s(m, "id"), i(m, "quality") as u8, s(m, "enh"), i(m, "offset") as i32)).collect();
        if let Some(f) = self.gear.weapon_sheet(id, q as u8, lvl as i32, &specs) {
            let pellets = if w.pellets > 1 { format!(" × {}", w.pellets) } else { String::new() };
            // the rows of the weapon card, named by the game's own stat texts
            let label = |key: &str, ours: &str| self.text_or(key, || ours.to_string());
            let mut rows = vec![json!([label("Equipment_Stat_Damage", "Damage"), num(f.dmg, 1) + &pellets])];
            if let Some(explosion) = f.explosion {
                rows.push(json!([label("Equipment_Stat_ExplosionDamage", "Expl. Damage"), num(explosion, 1)]));
            }
            rows.extend([
                json!([label("Equipment_Stat_CritChance", "Crit Chance"), format!("{:.1}%", f.crit * 100.0)]),
                json!([label("BonusStat_WeaponStatModifier_CritDamage", "Crit Damage"), format!("×{}", num(f.critmult, 2))]),
                json!([label("BonusStat_WeaponStatModifier_TimeBetweenShots", "Time Between Shots"), num(f.tbs, 3) + " s"]),
                json!([label("Equipment_Stat_ClipSize", "Clip Size"), f.mag.to_string()]),
                json!([label("Equipment_Stat_ReloadTime", "Reload Time"), num(f.reload, 2) + " s"]),
            ]);
            out.insert("stats".into(), json!(rows));
        }
        Value::Object(out)
    }

    fn armour(&self, info: &Value) -> Value {
        let n = self.damage_types.len().max(zar_logs_gearcalc::DAMAGE_TYPES);
        let (mut res, mut mov, mut known) = (vec![0.0f64; n], 0.0f64, false);
        // the damage types the worn pieces carry a rating for, or a mod adds to: the rows of the overview
        let mut shown: Vec<usize> = Vec::new();
        let dtype = |t: usize| self.damage_type_name(self.damage_types.get(t).map(String::as_str).unwrap_or(""));
        let mut pieces = Vec::new();
        for g in info.get("gear").and_then(Value::as_array).into_iter().flatten().filter(|g| self.is_armour(i(g, "type"))) {
            let slot = self.gear_slot(i(g, "type"));
            let id = s(g, "id");
            if id.is_empty() {
                // the game does not show the backpack slot when it is empty
                if Self::member(&self.gear_types, i(g, "type")) != "Backpack" {
                    pieces.push(json!({ "slot": slot, "empty": true }));
                }
                continue;
            }
            let (q, lvl) = (i(g, "quality"), i(g, "level"));
            let ad = self.def("ArmourPieceSO", id);
            let mut card = match json!({ "slot": slot, "empty": false, "name": pretty(id), "quality": q, "qname": self.quality_name(q), "level": lvl,
                "max_level": self.gear.armour_upgrade_steps(id),
                "stats": [], "icon": self.icon(ad.and_then(|d| d.get("menuIcon"))) }) { Value::Object(o) => o, _ => unreachable!() };
            if let (Some(pc), Some(r)) = (self.gear.armour_piece(id), self.gear.armour_resistance(id, q as u8, lvl as i32)) {
                if let Some(d) = ad {
                    rated(d, &mut shown);
                }
                if !pc.display.is_empty() {
                    card.insert("name".into(), json!(pc.display));
                }
                for (a, b) in res.iter_mut().zip(r) {
                    *a += b;
                }
                mov -= pc.mov * 100.0;
                known = true;
                let mut stats: Vec<Value> = r.iter().enumerate().filter(|(_, v)| v.abs() >= 0.05).map(|(t, v)| json!([dtype(t), signed_pct(*v)])).collect();
                if pc.mov != 0.0 {
                    stats.push(json!([self.text_or("BonusStat_MovementSpeed", || "Movement Speed".into()), signed_pct(-pc.mov * 100.0)]));
                }
                card.insert("stats".into(), json!(stats));
            }
            pieces.push(Value::Object(card));
        }
        // armour mod slots: as many as the loadout has, all of the "Armour" mod type
        let armour_type = self.weapon_mod_slots();
        let slots = info.get("armour_mods").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        let mods = self.mods(info.get("armour_mods"), std::iter::repeat_n(armour_type, slots));
        for m in &mods {
            for pair in m.get("resists").and_then(Value::as_array).into_iter().flatten() {
                let at = pair[0].as_str().and_then(|member| self.damage_types.iter().position(|t| t == member));
                if let (Some(at), Some(v)) = (at, pair[1].as_f64()) {
                    res[at] += v;
                    if !shown.contains(&at) {
                        shown.push(at);
                    }
                }
            }
        }
        let totals = if known {
            shown.sort();
            let resist: Vec<Value> = shown.iter().filter(|t| **t < n).map(|t| json!([dtype(*t), rnd(res[*t], 1)])).collect();
            json!({ "resist": resist, "move": rnd(mov, 2), "note": "pieces + mods, before skills" })
        } else {
            Value::Null
        };
        json!({ "pieces": pieces, "mod_power": Self::mod_power(&mods), "mods": mods, "totals": totals })
    }

    fn deployable(&self, g: &Value) -> Value {
        let id = s(g, "id");
        if id.is_empty() {
            return json!({ "slot": self.gear_slot(i(g, "type")), "empty": true });
        }
        let q = i(g, "quality");
        let d = self.def("WeaponDefinitionSO", id);
        let mut name = d.map(|d| self.text(s(d, "title"))).unwrap_or_default();
        if name.is_empty() {
            name = pretty(id);
        }
        // "Turret" / "Grenade": the definition's deployable category, in the game's words
        let cat = d.and_then(|d| enum_field(d, "deployableCategory")).map(|c| Self::member(&self.deployables, c)).filter(|m| !m.is_empty() && *m != "None");
        let kind = match cat {
            Some(m) => self.text_or(&format!("Equipment_DeployableType_{m}"), || m.to_string()),
            None => self.gear_slot(i(g, "type")),
        };
        let mut icon = self.icon(d.and_then(|d| d.get("menuIcon")));
        if icon.is_null() {
            icon = self.icon(d.and_then(|d| d.get("ingameIcon")));
        }
        // The damage type belongs to the deployed entity, not the throw item. And a deployable has no upgrade level:
        // the game only upgrades weapons and armour (`Gear.IsUpgradable` goes by the gear type), although a
        // deployable's definition points at a crafting table like theirs.
        json!({ "slot": kind, "empty": false, "name": name, "dtype": "", "quality": q, "qname": self.quality_name(q), "level": null,
                "max_level": null, "icon": icon })
    }

    // ---- abilities and skills ----
    fn ability_name(&self, aid: &str) -> String {
        let t = self.def("AbilityDefinitionSO", aid).map(|d| self.text(s(d, "title"))).unwrap_or_default();
        if t.is_empty() { pretty(aid) } else { t }
    }

    fn abilities(&self, info: &Value) -> Vec<Value> {
        let list = |key: &str| info.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        let upgrades: HashMap<&str, &Value> = list("upgrades").iter().map(|x| (s(x, "ability"), &x["ids"])).collect();
        let enh: HashMap<&str, &str> = list("enhancements").iter().filter(|x| !s(x, "id").is_empty()).map(|x| (s(x, "ability"), s(x, "id"))).collect();
        list("abilities").iter().map(|a| {
            let aid = a.as_str().unwrap_or("");
            if aid.is_empty() {
                return json!({ "empty": true });
            }
            let row = self.row(aid);
            let ad = self.def("AbilityDefinitionSO", aid);
            // upgrades and enhancements carry their own icons
            let picons: HashMap<&str, &Value> = ["abilityUpgrades", "abilityEnhancements"].iter()
                .flat_map(|k| ad.and_then(|d| d.get(*k)).and_then(Value::as_array).into_iter().flatten())
                .filter_map(|x| Some((x.get("name")?.as_str()?, x.get("icon")?))).collect();
            let level = i(info, "level");
            let perk = |pid: &str, kind: &str| {
                let r = self.row(&format!("{aid}/{pid}"));
                let mut desc = r.map(|r| r.description.clone()).unwrap_or_default();
                // A number that grows with the player's level was left open in the table: `{n}`. It is a parameter of
                // one of the entry's bonuses at this player's level, times `multiplier`, plus `offset`.
                let entry = ["abilityUpgrades", "abilityEnhancements"].iter()
                    .flat_map(|k| ad.and_then(|d| d.get(*k)).and_then(Value::as_array).into_iter().flatten()).find(|x| s(x, "name") == pid);
                if let (true, Some(entry)) = (desc.contains('{'), entry) {
                    let number = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64).unwrap_or(0.0);
                    let vals: Vec<String> = entry.get("descriptionParameters").and_then(Value::as_array).into_iter().flatten().map(|prm| {
                        let value = || {
                            if enum_field(prm, "source") != self.from_bonus {
                                return None;
                            }
                            let bonus = entry.get("bonuses")?.as_array()?.get(usize::try_from(i(prm, "bonusIndex")).ok()?)?;
                            let wanted = enum_field(prm, "paramType");
                            // the game takes 0 for a bonus without a parameter of the wanted type
                            let of_bonus = bonus.get("parameters")?.as_array()?.iter().find(|p| enum_field(p, "parameterType") == wanted)
                                .map_or(0.0, |p| zar_logs_namespack::upgrade_parameter_at_level(p, level));
                            Some(of_bonus * number(prm, "multiplier") + number(prm, "offset"))
                        };
                        value().map(|v| num(v, 2)).unwrap_or_else(|| "?".into())
                    }).collect();
                    if let Some(filled) = fill(&desc, &vals) {
                        desc = filled;
                    }
                }
                json!({ "name": r.map(|r| r.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| pretty(pid)), "kind": kind,
                        "desc": desc, "icon": self.icon(picons.get(pid).copied()) })
            };
            let mut perks: Vec<Value> = upgrades.get(aid).and_then(|v| v.as_array()).into_iter().flatten()
                .filter_map(Value::as_str).filter(|x| !x.is_empty()).map(|x| perk(x, "Upgrade")).collect();
            if let Some(e) = enh.get(aid) {
                perks.push(perk(e, "Enhancement"));
            }
            json!({ "empty": false, "name": row.map(|r| r.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| self.ability_name(aid)),
                    "icon": self.icon(ad.and_then(|d| d.get("ingameIcon"))), "desc": row.map(|r| r.description.as_str()).unwrap_or(""),
                    "cooldown": row.map(|r| r.cooldown_s.as_str()).unwrap_or(""), "perks": perks })
        }).collect()
    }

    fn skills(&self, info: &Value) -> Vec<Value> {
        let (cls, sub) = (self.class_name(s(info, "cls")), self.class_name(s(info, "sub")));
        // (skill id, points), in the loadout's order
        let ranks: Vec<(&str, i64)> = info.get("skills").and_then(Value::as_array).into_iter().flatten()
            .filter_map(|x| Some((x.get(0)?.as_str()?, x.get(1)?.as_i64()?))).collect();
        let rank_of = |id: &str| ranks.iter().rev().find(|(sid, _)| *sid == id).map(|x| x.1).unwrap_or(0);
        // the three skill trees of a character, by the table's own point types: shared, class, subclass
        let groups: [(String, Box<dyn Fn(&SkillRow) -> bool>); 3] = [
            ("Core skills".into(), Box::new(|r: &SkillRow| r.cls == cls && r.subclass.is_empty() && r.point_type == "basic")),
            (format!("{cls} skills"), Box::new(|r: &SkillRow| r.cls == cls && r.subclass.is_empty() && r.point_type == "class")),
            (format!("{sub} skills"), Box::new(|r: &SkillRow| !sub.is_empty() && r.subclass == sub)),
        ];
        let (mut out, mut seen) = (Vec::new(), std::collections::HashSet::new());
        for (title, pick) in &groups {
            let items: Vec<Value> = self.skills.iter().filter(|r| r.kind == "passive skill" && pick(r)).map(|r| {
                seen.insert(r.id.as_str());
                let sd = self.def("PassiveSkillDefinitionSO", &r.id);
                json!({ "name": r.name, "rank": rank_of(&r.id), "max": r.max_rank.trim().parse::<i64>().unwrap_or(0), "desc": r.description,
                        "icon": self.icon(sd.and_then(|d| d.get("menuIconRef"))),
                        "ranks": r.per_rank.split("||").map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>(), "bonus": r.max_rank_bonus })
            }).collect();
            if !items.is_empty() {
                out.push(json!({ "title": title, "items": items }));
            }
        }
        // one entry per skill id, as a dict built from the list would keep them (first position, last value)
        let mut ids: Vec<&str> = Vec::new();
        for (sid, _) in &ranks {
            if !ids.contains(sid) {
                ids.push(sid);
            }
        }
        let other: Vec<Value> = ids.into_iter().filter(|sid| !seen.contains(sid) && rank_of(sid) != 0).map(|sid| {
            let pts = rank_of(sid);
            let title = self.def("PassiveSkillDefinitionSO", sid).map(|d| self.text(s(d, "title"))).unwrap_or_default();
            json!({ "name": if title.is_empty() { pretty(sid) } else { title }, "rank": pts, "max": pts, "desc": "", "ranks": [], "bonus": "" })
        }).collect();
        if !other.is_empty() {
            out.push(json!({ "title": "Other skills", "items": other }));
        }
        out
    }

    // ---- whole build ----
    /// `info`: one player's loadout as the capture reads it (name, level, cls, sub, gear, armour_mods, abilities,
    /// upgrades, enhancements, skills). Each gear entry carries its slot's GearType, which says what it is.
    pub fn render(&self, info: &Value) -> Value {
        let gear: Vec<&Value> = info.get("gear").and_then(Value::as_array).into_iter().flatten().collect();
        let mut raw = info.as_object().cloned().unwrap_or_default();
        raw.remove("nk"); // the account id never goes into a log
        let name = s(info, "name");
        json!({
            "version": VERSION,
            "character": { "name": if name.is_empty() { "Player" } else { name }, "cls": self.class_name(s(info, "cls")),
                           "sub": self.class_name(s(info, "sub")), "level": i(info, "level") },
            "weapons": gear.iter().filter(|g| self.is_weapon(i(g, "type"))).map(|g| self.weapon(g)).collect::<Vec<_>>(),
            "armour": self.armour(info),
            "deployables": gear.iter().filter(|g| self.is_deployable(i(g, "type"))).map(|g| self.deployable(g)).collect::<Vec<_>>(),
            "abilities": self.abilities(info),
            "skills": self.skills(info),
            "raw": raw,
            "note": "Loadout the mission was built from, read from the game's own player list at mission start. Weapon \
                     numbers are weapon + mods before class skills and in-mission buffs; resistances are pieces + mods.",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_like_the_reference() {
        assert_eq!(num(2.5, 2), "2.5");
        assert_eq!(num(3.0, 2), "3");
        assert_eq!(num(0.125, 3), "0.125");
        assert_eq!(num(100.0, 1), "100");
        assert_eq!(signed_pct(12.34), "+12.3%");
        assert_eq!(signed_pct(-3.0), "-3.0%");
    }

    #[test]
    fn descriptions_take_positional_values() {
        let v = vec!["15".to_string(), "2".to_string()];
        assert_eq!(fill("{0}% for {1} s", &v).as_deref(), Some("15% for 2 s"));
        assert_eq!(fill("{} then {}", &v).as_deref(), Some("15 then 2"));
        assert_eq!(fill("{2}", &v), None);
        assert_eq!(fill("{name}", &v), None);
        assert_eq!(fill("plain", &v).as_deref(), Some("plain"));
    }

    #[test]
    fn enum_members_are_read_by_value() {
        let enums = json!({ "ItemQuality": { "Common": 0, "Uncommon": 1, "Legendary": 4, "Last": 4 } });
        assert_eq!(members(&enums, "ItemQuality"), vec!["Common", "Uncommon", "", "", "Legendary"]);
        assert!(members(&enums, "NoSuchEnum").is_empty());
    }
}
