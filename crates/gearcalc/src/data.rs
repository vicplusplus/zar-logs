//! Access to the dumped ScriptableObjects (`{pathID: {"_name": .., fields}}` per class) and the pieces of data
//! more than one calculation shares: asset lookup by name, the game's serialised dictionaries, object references,
//! and the quality / upgrade table of `GearCrafting` that scales both weapon damage and armour ratings.

use std::collections::HashMap;

use serde_json::{Map, Value};

pub(crate) type Obj = Map<String, Value>;

/// The assets of a class by asset name. The reference keeps the later object when two share a name, which is why
/// the dump's order has to survive (`preserve_order` in Cargo.toml).
pub(crate) fn by_name(class: &Obj) -> HashMap<&str, &Obj> {
    let mut out = HashMap::new();
    for object in class.values() {
        if let (Some(fields), Some(name)) = (object.as_object(), object.get("_name").and_then(Value::as_str)) {
            out.insert(name, fields);
        }
    }
    out
}

pub(crate) fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, key| v.get(key))
}

/// An integer field. Integral floats count: the reference compares numbers by value, not by JSON spelling.
pub(crate) fn int(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e18).map(|f| f as i64))
}

/// A float field. The dump writes a non-finite float as `null`; which one it was is lost, so it becomes NaN and
/// every number computed from it is NaN as well.
pub(crate) fn num(value: &Value) -> Option<f64> {
    if value.is_null() { Some(f64::NAN) } else { value.as_f64() }
}

/// Truth of a serialised flag (the game writes bools as 0 / 1).
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The entries of a serialised dictionary (`{"keys": [..], "values": [..]}`) with integer keys, in order. A
/// lookup must take the last match, as a dictionary built from the pairs would.
pub(crate) fn int_dict(dict: &Value) -> Option<Vec<(i64, &Value)>> {
    let (keys, values) = (dict.get("keys")?.as_array()?, dict.get("values")?.as_array()?);
    Some(keys.iter().zip(values).filter_map(|(k, v)| Some((int(k)?, v))).collect())
}

pub(crate) fn lookup<'a>(entries: &[(i64, &'a Value)], key: i64) -> Option<&'a Value> {
    entries.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// The key under which the object a reference field points to is stored in its class dump (`None` for a value
/// that is not a reference). Whether it leads anywhere is for the caller to find out: a null reference has
/// path id 0, which no object has.
pub(crate) fn path_id(reference: &Value) -> Option<String> {
    match reference.as_object()?.get("m_PathID")? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// One `GearCrafting` asset: how item quality and upgrade level scale a weapon's damage or an armour piece's
/// defence ratings.
#[derive(Clone)]
pub(crate) struct GearTable {
    quality_factors: Vec<(i64, f64)>,
    pub max_increase: f64,
    pub max_steps: i64,
}

impl GearTable {
    /// `None` when the asset lacks a field the calculations read (the reference fails on such a table).
    pub fn parse(table: &Obj) -> Option<GearTable> {
        let factors = int_dict(table.get("baseQualityFactors")?)?;
        Some(GearTable {
            quality_factors: factors.into_iter().map(|(k, v)| (k, num(v).unwrap_or(f64::NAN))).collect(),
            max_increase: num(table.get("maximumIncreaseFactor")?)?,
            max_steps: int(table.get("maxUpgradeSteps")?)?,
        })
    }

    pub fn quality_factor(&self, quality: i64) -> Option<f64> {
        self.quality_factors.iter().rev().find(|(k, _)| *k == quality).map(|(_, v)| *v)
    }
}

/// Every `GearCrafting` asset by path id, the form weapons and armour pieces refer to them in.
pub(crate) fn gear_tables(class: &Obj) -> HashMap<&str, Option<GearTable>> {
    class.iter().filter_map(|(id, o)| Some((id.as_str(), GearTable::parse(o.as_object()?)))).collect()
}
