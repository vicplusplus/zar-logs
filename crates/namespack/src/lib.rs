//! Generates the names pack: the one local file (`names.json`) from which the meter turns the ids it reads out of
//! the game into names. The pack holds the game's own names and texts, so it is built on the user's machine from
//! their own install and never shipped. From the same inputs it also makes the class-skills table the build
//! viewer shows ([`skill_rows`]: every ability, upgrade, enhancement and passive skill with its numbers and texts).
//!
//! Inputs, all read-only and all already extracted from the install by other tools:
//!
//! * the ScriptableObject dump: per script class one JSON object `{path id: {field: value, ..., "_name": asset
//!   name, "_bundle": ...}}` with the fields in the game's serialisation order ([`CLASSES`] lists the classes used,
//!   [`parse_dump`] reads a class file);
//! * the English localisation XML (see the `loc` module);
//! * the game's enums as `{enum name: {member name: value}}` ([`ENUMS`] lists the ones used; the pack needs
//!   `StatusEffectType` only, which the layout file has);
//! * the Addressables catalog, as a function from asset GUID to asset path (classes point at their abilities and
//!   skills by GUID).
//!
//! Modules: `skills` makes the rows of the table, `describe` the description of a row's game data (its `data`
//! column), `bonus` evaluates the per-rank and per-level numbers, `fmt` prints numbers the table's way.
//!
//! Nothing that the data can say is written down here: which classes there are and in which order, the level
//! cap, the names of enum values and all texts are read from the inputs, so a game update is followed without a
//! change to this crate. What the code does have to name are the fields it reads, and a few lists of enum
//! members and behaviour classes that express a judgement the data does not contain (for example which effect
//! types change damage). Those names can go stale with an update; [`check`] holds them against the data.
//!
//! Sections of the pack and what each is derived from:
//!
//! * `loc`: localisation key -> English text.
//! * `weapons`, `abilities`, `entities`, `sets`: one entry per asset of `WeaponDefinitionSO`,
//!   `AbilityDefinitionSO`, `EntityDefinitionSO`, `StatusEffectSetDefinitionSO`, keyed by identity hash. The game
//!   identifies a definition at run time by the CRC-32 of its asset name, and that hash is what the meter reads
//!   from memory. An entry has the asset `name`, a readable `pretty` name made from it, the localisation key of
//!   the `title` where the class has one, and for sets the `setType` (1 buff, 2 debuff).
//! * `levels` (`LevelDescriptorSO`) and `modes` (`LevelMode`): lower-case asset name -> title key, to name a
//!   mission from the scene and mode the game reports.
//! * `classes` (`PlayerClassDefinitionSO`): lower-case `className` -> the class's English title.
//! * `enhancements` (`GearModEnhancementDefinitionSO`): asset name and title key of the unique gear mods, the last
//!   place an effect is looked up when nothing else claims it.
//! * `effect_types`: `StatusEffectType` value -> readable name, for effects that belong to no set.
//! * `skills`: one row per class ability, upgrade, enhancement and passive skill with the status-effect sets its
//!   data mentions (see the `skills` module).
//!
//! Order matters in two places and comes straight from the dump: lists (`levels`, `enhancements`, `skills`) follow
//! the order of objects in their class file, and the sets of a skill row follow the order of fields inside the
//! dumped objects. `serde_json` only keeps that order with its `preserve_order` feature, which this crate turns on.
//!
//! When two assets share a key (the same name, or names with the same hash), the entry stays where the first one
//! was and holds the later one. Nothing in today's data does; the rule is fixed so that the pack is the same as
//! the reference generator's in that case too.

mod bonus;
mod check;
mod describe;
mod dump;
mod fmt;
mod loc;
mod skills;

pub use dump::parse_dump;
pub use skills::SkillRow;

use std::collections::HashMap;
use std::hash::Hash;

use serde_json::{json, Map, Value};
use zar_logs_core::names::pretty;

type Obj = Map<String, Value>;

/// Version of the pack's format, written as its `version`.
pub const VERSION: u64 = 1;

/// Every ScriptableObject class [`build`] and [`skill_rows`] need: the nine the pack's sections and the skill
/// rows are made from, `AdditionalMovementContactDefinitionSO` and `Currency` because skill data refers to their
/// objects (a dash upgrade's contact effect, the price of an upgrade), and `CharacterLevelsSO` for the level cap.
/// A reference to an object that is not given counts as an empty field, so a class that skill data refers to has
/// to be here for the description of that data to come out as the reference tooling wrote it.
pub const CLASSES: &[&str] = &[
    "WeaponDefinitionSO",
    "AbilityDefinitionSO",
    "EntityDefinitionSO",
    "StatusEffectSetDefinitionSO",
    "LevelDescriptorSO",
    "LevelMode",
    "GearModEnhancementDefinitionSO",
    "PlayerClassDefinitionSO",
    "PassiveSkillDefinitionSO",
    "AdditionalMovementContactDefinitionSO",
    "Currency",
    "CharacterLevelsSO",
];

/// An optional further entry of `so`: the dump of the game's main object (`GameRunner.json`, one object under its
/// bundle's name). It holds the game's own list of base classes and points at the level table in use. With it
/// the skill rows follow the game's class order; without it base classes go by asset name and the level cap is
/// the highest of the dumped level tables, which gives the same result for today's game.
pub const GAME_RUNNER: &str = "GameRunner";

/// Every enum of the game [`skill_rows`] needs, by type name: the skill table names bonus types, parameters,
/// conditions, effect types, damage types, flags, skill trees and set types by their member names. Each is
/// expected as `{member name: value}` with the members in declaration order: flag sets are spelled out in that
/// order, and of two members with one value the later one names it. [`build`] needs `StatusEffectType` only.
pub const ENUMS: &[&str] = &[
    "BonusType",
    "BonusParameterType",
    "BehaviourConditionType",
    "StatusEffectType",
    "AbilityTriggerConditionType",
    "DamageType",
    "BehaviourTriggerFlags",
    "BonusScopeFlags",
    "GameEntityMask",
    "StatusEffectSetType",
    "SkillPointType",
    "AbilityActivationBehaviour",
    "BehaviourConditionsCheckType",
    "PassiveSkillDescriptionBonusType",
];

/// The game's identity hash of an asset: CRC-32 of its name. Names are ASCII in practice; the reference hashes the
/// Latin-1 bytes with `?` for any character outside Latin-1, and so does this.
fn identity_hash(name: &str) -> u32 {
    let bytes: Vec<u8> = name.chars().map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')).collect();
    crc32fast::hash(&bytes)
}

/// Python's idea of a value that counts: not null, zero, false or empty.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Entries by key the way the reference's tables fill up: a key seen again keeps its place and takes the new value.
fn keyed<K: Clone + Eq + Hash, V>(entries: impl IntoIterator<Item = (K, V)>) -> Vec<(K, V)> {
    let mut place: HashMap<K, usize> = HashMap::new();
    let mut out: Vec<(K, V)> = Vec::new();
    for (k, v) in entries {
        match place.get(&k) {
            Some(&i) => out[i].1 = v,
            None => {
                place.insert(k.clone(), out.len());
                out.push((k, v));
            }
        }
    }
    out
}

fn name_of(o: &Obj) -> &str { o.get("_name").and_then(Value::as_str).unwrap_or("") }

/// The named assets of a class by identity hash, in file order.
fn assets<'a>(so: &'a HashMap<String, Obj>, class: &str) -> Vec<(u32, &'a Obj)> {
    let objects = so.get(class).into_iter().flat_map(Map::values).filter_map(Value::as_object);
    keyed(objects.filter(|o| !name_of(o).is_empty()).map(|o| (identity_hash(name_of(o)), o)))
}

/// The assets of a class by lower-case asset name.
fn by_lower_name<'a>(so: &'a HashMap<String, Obj>, class: &str) -> Vec<(String, &'a Obj)> {
    keyed(assets(so, class).into_iter().map(|(_, o)| (name_of(o).to_lowercase(), o)))
}

fn title_key(o: &Obj) -> Value { o.get("title").filter(|t| truthy(t)).cloned().unwrap_or_else(|| json!("")) }

/// A section keyed by identity hash; `fields` are copied when they hold something (an enum's `{"value": n}` as n).
fn table(so: &HashMap<String, Obj>, class: &str, fields: &[&str]) -> Value {
    let mut out = Map::new();
    for (hash, o) in assets(so, class) {
        let mut row = Map::new();
        row.insert("name".into(), json!(name_of(o)));
        row.insert("pretty".into(), json!(pretty(name_of(o))));
        for field in fields {
            let v = match o.get(*field) {
                Some(Value::Object(wrapped)) => wrapped.get("value"),
                other => other,
            };
            if let Some(v) = v.filter(|v| truthy(v)) {
                row.insert((*field).into(), v.clone());
            }
        }
        out.insert(hash.to_string(), Value::Object(row));
    }
    Value::Object(out)
}

/// "DamageOverTime" -> "Damage over time".
fn sentence(member: &str) -> String {
    let mut spaced = String::with_capacity(member.len() + 4);
    let mut previous = ' ';
    for c in member.chars() {
        if previous.is_ascii_lowercase() && c.is_ascii_uppercase() {
            spaced.push(' ');
        }
        spaced.push(c);
        previous = c;
    }
    let mut rest = spaced.chars();
    rest.next().map(|first| first.to_uppercase().collect::<String>() + &rest.as_str().to_lowercase()).unwrap_or_default()
}

fn require(so: &HashMap<String, Obj>, enums: &Value, enum_names: &[&str]) -> Result<(), String> {
    if let Some(missing) = CLASSES.iter().find(|class| !so.contains_key(**class)) {
        return Err(format!("the dump of class {missing} was not given"));
    }
    match enum_names.iter().find(|name| !enums.get(**name).is_some_and(Value::is_object)) {
        Some(missing) => Err(format!("the enum {missing} was not given")),
        None => Ok(()),
    }
}

/// The class-skills table: every class ability, ability upgrade, ability enhancement and passive skill, in the
/// game's order (base class, then its subclasses; abilities before skills), with the cells the build viewer shows.
///
/// * `so`: class name -> the content of that class's dump file (see [`parse_dump`]), for every class in
///   [`CLASSES`]. Objects of any further class given are also known by name when skill data refers to them.
/// * `loc_xml`: the content of the English localisation file.
/// * `enums`: `{enum name: {member name: value}}` with every enum in [`ENUMS`], members in declaration order.
/// * `guid_to_path`: Addressables asset GUID -> path of the asset (the first location the catalog lists for it).
///
/// Fails when a class of [`CLASSES`] or an enum of [`ENUMS`] is missing, when no level table has a level cap, or
/// when a subclass reference does not lead to a dumped object. A skill or ability whose GUID or definition cannot
/// be found is left out. Names in this crate that the data no longer has do not fail: see [`check`].
pub fn skill_rows(
    so: &HashMap<String, Obj>,
    loc_xml: &str,
    enums: &Value,
    guid_to_path: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<SkillRow>, String> {
    require(so, enums, ENUMS)?;
    skills::rows(so, &loc::read(loc_xml).raw, enums, guid_to_path)
}

/// The names this crate uses for things in the game that the given data does not have: an enum member in one of
/// the crate's lists that the enum no longer has, a field the code reads that no dumped object has, a behaviour
/// class or field-name fragment listed as presentation that occurs nowhere. One sentence each; empty when all is
/// well, as it is for today's game. Such a name is not an error for [`skill_rows`] or [`build`] (the rest of the
/// table is still right), but it means an update renamed or removed something and a cell may have changed
/// silently, so a caller should show these to whoever maintains the tool.
///
/// `so` and `enums` are those of [`skill_rows`]; what is missing from them altogether is not reported here.
pub fn check(so: &HashMap<String, Obj>, enums: &Value) -> Vec<String> { check::problems(so, enums) }

/// Builds the pack.
///
/// The arguments are those of [`skill_rows`], except that of `enums` only `StatusEffectType` is needed (the
/// `enums` object of the layout file will do): the pack's skill rows keep the sets a row's data mentions, and
/// which sets those are does not depend on how the other enums' values are named.
///
/// Fails as [`skill_rows`] does.
pub fn build(
    so: &HashMap<String, Obj>,
    loc_xml: &str,
    enums: &Value,
    guid_to_path: &dyn Fn(&str) -> Option<String>,
) -> Result<Value, String> {
    require(so, enums, &["StatusEffectType"])?;
    let effect_types = enums["StatusEffectType"].as_object().ok_or("StatusEffectType is not an enum table")?;
    let texts = loc::read(loc_xml);
    let text = |key: Option<&Value>| key.and_then(Value::as_str).and_then(|k| texts.display.get(k)).and_then(Value::as_str)
        .unwrap_or("");
    let skills = skills::rows(so, &texts.raw, enums, guid_to_path)?;

    let levels: Vec<Value> = by_lower_name(so, "LevelDescriptorSO").into_iter()
        .map(|(key, o)| json!({ "key": key, "name": name_of(o), "title": title_key(o) })).collect();
    let modes: Obj = by_lower_name(so, "LevelMode").into_iter().map(|(key, o)| (key, title_key(o))).collect();
    let mut classes = Map::new();
    for (_, o) in assets(so, "PlayerClassDefinitionSO") {
        let Some(class_name) = o.get("className").and_then(Value::as_str).filter(|n| !n.is_empty()) else { continue };
        let title = text(o.get("title"));
        classes.insert(class_name.to_lowercase(), json!(if title.is_empty() { pretty(class_name) } else { title.to_string() }));
    }
    let enhancements: Vec<Value> = assets(so, "GearModEnhancementDefinitionSO").into_iter()
        .map(|(_, o)| json!({ "name": name_of(o), "title": title_key(o) })).collect();
    let effect_names: Obj = keyed(effect_types.iter().filter_map(|(member, v)| Some((v.as_i64()?, sentence(member)))))
        .into_iter().map(|(value, name)| (value.to_string(), json!(name))).collect();
    let skills: Vec<Value> = skills.iter()
        .map(|r| json!({ "id": r.id, "name": r.name, "kind": r.kind, "sets": r.sets() })).collect();

    let mut pack = Map::new();
    pack.insert("version".into(), json!(VERSION));
    pack.insert("loc".into(), Value::Object(texts.display));
    pack.insert("weapons".into(), table(so, "WeaponDefinitionSO", &["title"]));
    pack.insert("abilities".into(), table(so, "AbilityDefinitionSO", &["title"]));
    pack.insert("entities".into(), table(so, "EntityDefinitionSO", &[]));
    pack.insert("sets".into(), table(so, "StatusEffectSetDefinitionSO", &["setType"]));
    pack.insert("levels".into(), Value::Array(levels));
    pack.insert("modes".into(), Value::Object(modes));
    pack.insert("classes".into(), Value::Object(classes));
    pack.insert("enhancements".into(), Value::Array(enhancements));
    pack.insert("effect_types".into(), Value::Object(effect_names));
    pack.insert("skills".into(), Value::Array(skills));
    Ok(Value::Object(pack))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_names() {
        // CRC-32 check value of the standard test vector
        assert_eq!(identity_hash("123456789"), 0xCBF4_3926);
        assert_eq!(identity_hash("a\u{20ac}"), identity_hash("a?"));
        assert_eq!(sentence("DOTTickRateModifier"), "Dottick rate modifier");
        assert_eq!(sentence("WeaponTimeBetweenShots"), "Weapon time between shots");
    }

    #[test]
    fn repeated_keys_keep_their_place() {
        assert_eq!(keyed([("a", 1), ("b", 2), ("a", 3)]), [("a", 3), ("b", 2)]);
    }

    #[test]
    fn object_order_is_kept() {
        // the pack's lists and set order rely on this (serde_json's preserve_order)
        let o: Obj = serde_json::from_str(r#"{"b":0,"a":0,"c":0}"#).unwrap();
        assert_eq!(o.keys().collect::<Vec<_>>(), ["b", "a", "c"]);
    }
}
