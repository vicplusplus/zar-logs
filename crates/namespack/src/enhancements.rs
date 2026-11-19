//! The numbers in the descriptions of the unique gear mods (enhancements): "Crit Chance improved for {0} seconds
//! after being healed." What `{0}` is depends on the quality of the mod the enhancement sits on.
//!
//! This follows the game's own routine for it (`GearModEnhancementDefinitionSO.GetDescriptionParameter`, read from
//! its code), with the rank being the mod's quality (`BonusEvaluationContext.ForModEnhancement`) and no player
//! level:
//!
//! * A description parameter with a list of `manualRankValues` is the entry for the rank, counted from 1; 0 for a
//!   rank outside the list.
//! * Otherwise it is the parameter of the wanted type (`paramType`) of the bonus it points at (`bonusIndex`), at
//!   that rank; 0 when the bonus has none of that type.
//! * When that bonus adds behaviours (`AddEntityBehaviours`, `AddWeaponBehaviours`), the first of them that
//!   applies a status-effect set or raises a temporary shield adds to it. A shield adds the time before it starts
//!   to decay. A set adds, for a `Duration` parameter, how long its first effect lasts: the behaviour's own
//!   duration, a share of it, or a duration of the effect's own, as the effect's duration mode says.
//!
//! One thing is not followed: the game first turns a bonus's mod-point parameters into parameters of another
//! type. A number that would need that is left out (`null`), and the viewer shows `?` for it.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::bonus;
use crate::describe::{items, whole};

type Obj = Map<String, Value>;

/// The enum members this routine goes by, as (enum, member).
pub(crate) const MEMBERS: [(&str, &str); 9] = [
    ("BonusType", "AddEntityBehaviours"),
    ("BonusType", "AddWeaponBehaviours"),
    ("BonusParameterType", "Duration"),
    ("BonusParameterType", "StatusEffectSetLevel"),
    ("BonusParameterType", "ModPoint"),
    ("StatusEffectSetDurationMode", "InheritDuration"),
    ("StatusEffectSetDurationMode", "PropertionalDuration"),
    ("StatusEffectSetDurationMode", "CustomDuration"),
    ("StatusEffectSetDurationMode", "MinDuration"),
];
const APPLY_SET: &str = "ApplyStatusEffectSet";
const SHIELD: &str = "ActivateTemporaryShield";

/// An enum field as the dump writes it: `{"value": n}` or the bare number.
fn enum_value(v: Option<&Value>) -> Option<i128> { v.and_then(|x| x.get("value").and_then(whole).or_else(|| whole(x))) }
fn number(o: &Value, field: &str) -> f64 { o.get(field).and_then(Value::as_f64).unwrap_or(0.0) }

struct Numbers<'a> {
    sets: Option<&'a Obj>,
    /// values of `MEMBERS`, in that order
    member: Vec<i128>,
}

impl Numbers<'_> {
    /// The bonus's parameter of type `wanted` at `rank`: 0 when it has none, None when one of its parameters is in
    /// mod points (see the module description).
    fn parameter(&self, bonus: &Value, wanted: i128, rank: i64) -> Option<f64> {
        let parameters = items(bonus.get("parameters"));
        let kind = |p: &&Value| enum_value(p.get("parameterType")).unwrap_or(0);
        if parameters.iter().any(|p| kind(&p) == self.member[4]) {
            return None;
        }
        Some(parameters.iter().find(|p| kind(p) == wanted).map_or(0.0, |p| bonus::at_rank(p, rank, 0)))
    }

    /// What the first set-applying or shield-raising behaviour of the bonus adds to a parameter of type `wanted`.
    fn from_behaviours(&self, bonus: &Value, registry: &HashMap<i64, &Value>, wanted: i128, rank: i64) -> Option<f64> {
        let listed = items(bonus.get("additionalBehaviours").and_then(|l| l.get("m_List"))).iter()
            .filter_map(|e| registry.get(&e.get("rid")?.as_i64()?).copied());
        for behaviour in listed {
            let class = behaviour.get("type").and_then(|t| t.get("class")).and_then(Value::as_str).unwrap_or("");
            let Some(data) = behaviour.get("data") else { continue };
            if class == SHIELD {
                return Some(number(data, "decayDelay"));
            }
            if class != APPLY_SET {
                continue;
            }
            if wanted != self.member[2] {
                return Some(0.0);
            }
            // the level of the set that is applied: the behaviour's, raised by the bonus's set-level parameter
            let level = (self.parameter(bonus, self.member[3], rank)? + number(data, "setLevel")).round() as i64;
            let set = self.sets?.get(&data.get("set")?.get("m_PathID")?.to_string())?;
            let leveled = items(set.get("leveledSet")).get(usize::try_from(level - 1).ok()?)?;
            let duration = items(leveled.get("statusEffects")).first()?.get("durationModifier")?;
            let (own, mode) = (number(data, "duration"), enum_value(duration.get("mode"))?);
            let custom = number(duration, "customDuration");
            return Some(match mode {
                m if m == self.member[5] => own,
                m if m == self.member[6] => number(duration, "propertionOfDuration") * own,
                m if m == self.member[7] => custom,
                m if m == self.member[8] => own.max(custom),
                _ => return None,
            });
        }
        Some(0.0)
    }

    fn value(&self, enhancement: &Obj, registry: &HashMap<i64, &Value>, parameter: &Value, rank: i64) -> Option<f64> {
        let manual = items(parameter.get("manualRankValues"));
        if !manual.is_empty() {
            return Some(usize::try_from(rank - 1).ok().and_then(|at| manual.get(at)).and_then(Value::as_f64).unwrap_or(0.0));
        }
        let index = parameter.get("bonusIndex").and_then(Value::as_i64).unwrap_or(0);
        let Some(bonus) = usize::try_from(index).ok().and_then(|i| items(enhancement.get("bonuses")).get(i)) else { return Some(0.0) };
        let wanted = enum_value(parameter.get("paramType")).unwrap_or(0);
        let adds_behaviours = enum_value(bonus.get("bonusType")).is_some_and(|t| t == self.member[0] || t == self.member[1]);
        let extra = if adds_behaviours { self.from_behaviours(bonus, registry, wanted, rank)? } else { 0.0 };
        Some(self.parameter(bonus, wanted, rank)? + extra)
    }
}

/// `{enhancement asset name: [one list per description parameter: its value at rank 0, 1, ... up to the highest
/// item quality; null where it cannot be told]}`, for the enhancements whose description has numbers.
pub(crate) fn numbers(so: &HashMap<String, Obj>, enums: &Value) -> Map<String, Value> {
    let member = |(table, name): &(&str, &str)| enums.get(*table).and_then(|t| t.get(*name)).and_then(whole);
    // a member this game version does not have matches nothing
    let n = Numbers { sets: so.get("StatusEffectSetDefinitionSO"), member: MEMBERS.iter().map(|m| member(m).unwrap_or(i128::MIN)).collect() };
    let last_quality = enums.get("ItemQuality").and_then(Value::as_object).into_iter().flatten().filter_map(|(_, v)| v.as_i64()).max().unwrap_or(0);
    let mut out = Map::new();
    for enhancement in so.get("GearModEnhancementDefinitionSO").into_iter().flat_map(Map::values).filter_map(Value::as_object) {
        let parameters = items(enhancement.get("descriptionParameters"));
        let Some(name) = enhancement.get("_name").and_then(Value::as_str).filter(|_| !parameters.is_empty()) else { continue };
        let registry: HashMap<i64, &Value> = items(enhancement.get("references").and_then(|r| r.get("RefIds"))).iter()
            .filter_map(|e| Some((e.get("rid")?.as_i64()?, e))).collect();
        let lists: Vec<Value> = parameters.iter().map(|parameter| {
            json!((0..=last_quality).map(|rank| n.value(enhancement, &registry, parameter, rank)
                // single-precision noise off: 0.1 read from the game's data is 0.100000001
                .map(|v| crate::fmt::round(v, 4))).collect::<Vec<_>>())
        }).collect();
        out.insert(name.to_string(), Value::Array(lists));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_of_a_description() {
        let enums = json!({
            "ItemQuality": { "Common": 0, "Uncommon": 1, "Rare": 2, "Epic": 3, "Last": 3 },
            "BonusType": { "AddEntityBehaviours": 9, "AddWeaponBehaviours": 8, "Other": 1 },
            "BonusParameterType": { "Generic": 0, "Duration": 6, "StatusEffectSetLevel": 7, "ModPoint": 5 },
            "StatusEffectSetDurationMode": { "InheritDuration": 0, "PropertionalDuration": 1, "CustomDuration": 2, "MinDuration": 3 },
        });
        let per_rank = |kind: i64, initial: f64, step: f64| json!({ "parameterType": { "value": kind }, "initialBonus": initial, "statBonusPerRank": step });
        let behaviour = |rid: i64, class: &str, data: Value| json!({ "rid": rid, "type": { "class": class }, "data": data });
        let set = |mode: i64| json!({ "leveledSet": [
            { "statusEffects": [{ "durationModifier": { "mode": { "value": 0 }, "propertionOfDuration": 0.0, "customDuration": 0.0 } }] },
            { "statusEffects": [{ "durationModifier": { "mode": { "value": mode }, "propertionOfDuration": 0.5, "customDuration": 9.0 } }] }] });
        let enhancement = |name: &str, parameters: Value, bonuses: Value| json!({ "_name": name, "descriptionParameters": parameters, "bonuses": bonuses,
            "references": { "RefIds": [
                behaviour(1, "SomethingElse", json!({})),
                behaviour(2, "ApplyStatusEffectSet", json!({ "duration": 4.0, "setLevel": 2, "set": { "m_FileID": 0, "m_PathID": 70 } })),
                behaviour(3, "ActivateTemporaryShield", json!({ "decayDelay": 1.5 })),
                behaviour(4, "ApplyStatusEffectSet", json!({ "duration": 4.0, "setLevel": 1, "set": { "m_FileID": 0, "m_PathID": 71 } }))] } });
        let adding = |rids: &[i64], parameters: Value| json!({ "bonusType": { "value": 9 }, "parameters": parameters,
            "additionalBehaviours": { "m_List": rids.iter().map(|r| json!({ "rid": r })).collect::<Vec<_>>() } });
        let so: HashMap<String, Obj> = HashMap::from([
            ("StatusEffectSetDefinitionSO".to_string(), json!({ "70": set(1), "71": set(3) }).as_object().unwrap().clone()),
            ("GearModEnhancementDefinitionSO".to_string(), json!({
                // listed values: the entry for the rank, counted from 1
                "1": enhancement("Listed", json!([{ "bonusIndex": 0, "paramType": 0, "manualRankValues": [10.0, 15.0, 20.0] }]), json!([])),
                // a parameter of a bonus: per rank; and one whose type the bonus does not have
                "2": enhancement("Chance", json!([{ "bonusIndex": 0, "paramType": 0, "manualRankValues": [] }, { "bonusIndex": 0, "paramType": 6 }, { "bonusIndex": 4 }]),
                    json!([{ "bonusType": { "value": 1 }, "parameters": [per_rank(0, 0.0, 5.0)] }])),
                // a set applied for a time: the behaviour's duration by the effect's mode (here half of it), plus the parameter
                "3": enhancement("Timed", json!([{ "bonusIndex": 0, "paramType": 6 }]), json!([adding(&[1, 2, 3], json!([per_rank(6, -1.0, 1.0)]))])),
                // ... the longer of the two durations, at the set level the bonus raises to (1 + 1)
                "4": enhancement("Longer", json!([{ "bonusIndex": 0, "paramType": 6 }]), json!([adding(&[4], json!([per_rank(7, 1.0, 0.0)]))])),
                "5": enhancement("Shield", json!([{ "bonusIndex": 0, "paramType": 6 }]), json!([adding(&[3, 2], json!([per_rank(6, 0.0, 1.0)]))])),
                // mod points are not turned into a parameter here: unknown rather than wrong
                "6": enhancement("Points", json!([{ "bonusIndex": 0, "paramType": 0 }]), json!([{ "bonusType": { "value": 1 }, "parameters": [per_rank(5, 1.0, 1.0)] }])),
                "7": enhancement("NoNumbers", json!([]), json!([])),
            }).as_object().unwrap().clone()),
        ]);
        let n = numbers(&so, &enums);
        assert_eq!(n.keys().collect::<Vec<_>>(), ["Listed", "Chance", "Timed", "Longer", "Shield", "Points"]);
        assert_eq!(n["Listed"], json!([[0.0, 10.0, 15.0, 20.0]]));
        assert_eq!(n["Chance"], json!([[0.0, 5.0, 10.0, 15.0], [0.0, 0.0, 0.0, 0.0], [0.0, 0.0, 0.0, 0.0]]));
        assert_eq!(n["Timed"], json!([[1.0, 2.0, 3.0, 4.0]]));
        assert_eq!(n["Longer"], json!([[9.0, 9.0, 9.0, 9.0]]));
        assert_eq!(n["Shield"], json!([[1.5, 2.5, 3.5, 4.5]]));
        assert_eq!(n["Points"], json!([[null, null, null, null]]));
    }
}
