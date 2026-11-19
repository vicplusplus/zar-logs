//! What this crate knows about the game by name, held against the data it is given. Rows, their order, the level
//! cap, enum names and texts all come from the data; but to read the data at all the code has to name fields,
//! and a few lists name enum members and classes of the game (which effect types change damage, which behaviours
//! are only presentation). A game update can rename any of these, and a name that no longer exists does not fail:
//! it just stops matching, and a cell silently changes. `problems` finds such names so that they can be reported.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::describe::{items, Obj, NAMED_MEMBERS, OWN_FIELDS, RANK_FIELDS, SILENT_BEHAVIOURS, VISUAL};
use crate::skills::{DPS_BONUS_TYPES, DPS_EFFECT_TYPES};

/// The classes whose objects the skill table describes field by field.
const DESCRIBED: [&str; 5] = [
    "PlayerClassDefinitionSO", "AbilityDefinitionSO", "PassiveSkillDefinitionSO", "StatusEffectSetDefinitionSO",
    "EntityDefinitionSO",
];

/// The field names the code reads from the described classes (at any depth), apart from those in the lists
/// checked separately. Kept by hand next to the code that uses them (`skills`, `describe`, `bonus`).
const FIELDS_READ: &[&str] = &[
    // a class and its entries
    "className", "title", "activatedAbilities", "passiveSkills", "subclasses", "ability", "skillDef", "skillType",
    "unlockLevel",
    // an ability, its upgrades and enhancements
    "description", "cooldown", "abilityDuration", "replenishTime", "chargeTime", "castTime", "triggerCapacity",
    "hasInfiniteCapacity", "replenishCapacityOverTime", "dualWield", "targeted", "autoTriggerOnLowHealth",
    "disableIfTriggerConditionsUnmet", "triggerCooldownOnUsingActiveBehaviours", "activationBehaviour",
    "triggerConditions", "behaviours", "activeBehaviours", "cancelledBehaviours", "abilityBonuses", "abilityUpgrades",
    "abilityEnhancements", "bonuses", "name", "costs", "cost", "currency", "allowedPlayerClasses",
    // a skill and its tooltip
    "upgradeDescription", "maxRankBonusDesc", "maxSkillPoints", "requiresSpentSkillPoints", "requiredAbility",
    "descriptionParameters", "bonusIndex", "paramType", "entityBonusStats", "weaponBonusStats", "abilityBonusStats",
    "maxRankEntityBonuses", "maxRankWeaponBonuses", "maxRankAbilityBonuses", "maxRankProfileBonuses",
    // a bonus and its parameters
    "bonusType", "parameters", "parameterType", "conditionType", "additionalBehaviours", "additionalAbilityBehaviours",
    "scopeFlags", "useSpecificDamageType", "specificDamageType", "newDamageType", "requiredTriggerFlags",
    "requiredEntityMask", "bonusLevelRanges", "minLevel", "maxLevel", "levelIncrementPerStatBonus",
    "statBonusPerLevelIncrement",
    // behaviours, sets and entities
    "triggerFlags", "conditions", "conditionsCheck", "setType", "isImmutable", "leveledSet", "statusEffects",
    "durationModifier", "hasLifetime", "lifetime",
];

/// Every field name that occurs anywhere inside `v`.
fn field_names<'a>(v: &'a Value, out: &mut HashSet<&'a str>) {
    match v {
        Value::Object(m) => m.iter().for_each(|(k, x)| {
            out.insert(k);
            field_names(x, out);
        }),
        Value::Array(a) => a.iter().for_each(|x| field_names(x, out)),
        _ => {}
    }
}

pub(crate) fn problems(so: &HashMap<String, Obj>, enums: &Value) -> Vec<String> {
    let mut out = Vec::new();

    // enum members named in the code
    let dps_bonus = DPS_BONUS_TYPES.iter().map(|member| ("BonusType", *member, "the damage-relevant bonus types"));
    let dps_effect = DPS_EFFECT_TYPES.iter().map(|member| ("StatusEffectType", *member, "the damage-relevant effect types"));
    let own = OWN_FIELDS.iter().map(|(member, _)| ("StatusEffectType", *member, "the effect types with fields of their own"));
    let special = NAMED_MEMBERS.iter().map(|(table, member)| (*table, *member, "the members with a special meaning"));
    let numbers = crate::enhancements::MEMBERS.iter().map(|(table, member)| (*table, *member, "the numbers of enhancement descriptions"));
    for (table, member, list) in dps_bonus.chain(dps_effect).chain(own).chain(special).chain(numbers) {
        // an enum that was not given at all is the caller's business (`skill_rows` refuses to run without it)
        if enums.get(table).is_some_and(|members| members.get(member).is_none()) {
            out.push(format!("enum {table} has no member {member} (named in {list})"));
        }
    }

    // the game's main object, when given, has to have what is taken from it
    if let Some(runner) = so.get(crate::GAME_RUNNER) {
        for name in ["playerClassDefinitions", "defaultCharacterLevelsTable"] {
            if !runner.values().any(|object| object.get(name).is_some()) {
                out.push(format!("{} has no field {name}", crate::GAME_RUNNER));
            }
        }
    }

    // field and class names, against what the dump has
    let objects = |class: &str| so.get(class).into_iter().flat_map(|objects| objects.values());
    let mut fields = HashSet::new();
    DESCRIBED.iter().flat_map(|class| objects(class)).for_each(|object| field_names(object, &mut fields));
    if fields.is_empty() {
        return out;
    }
    for name in FIELDS_READ {
        if !fields.contains(name) {
            out.push(format!("no dumped object has a field {name} (read for the skill table)"));
        }
    }
    for fragment in VISUAL {
        if !fields.iter().any(|field| field.contains(fragment)) {
            out.push(format!("no field name contains {fragment:?} (listed as presentation only)"));
        }
    }
    let mut parameter_fields = HashSet::new();
    let bonus_lists = FIELDS_READ.iter().filter(|name| name.ends_with("BonusStats") || (name.starts_with("maxRank") && name.ends_with("Bonuses")));
    for list in bonus_lists {
        for bonus in objects("PassiveSkillDefinitionSO").flat_map(|skill| items(skill.get(*list))) {
            items(bonus.get("parameters")).iter().for_each(|parameter| field_names(parameter, &mut parameter_fields));
        }
    }
    for name in RANK_FIELDS {
        if !parameter_fields.contains(name) {
            out.push(format!("no skill bonus parameter has a field {name} (one of the per-rank inputs)"));
        }
    }
    let mut effect_fields = HashSet::new();
    let mut behaviours = HashSet::new();
    for set in objects("StatusEffectSetDefinitionSO") {
        for level in items(set.get("leveledSet")) {
            for entry in items(level.get("statusEffects")) {
                effect_fields.extend(entry.get("statusEffect").and_then(Value::as_object).into_iter().flatten().map(|(k, _)| k.as_str()));
            }
        }
    }
    for object in so.values().flat_map(|objects| objects.values()) {
        let registry = items(object.get("references").and_then(|r| r.get("RefIds")));
        behaviours.extend(registry.iter().filter_map(|entry| entry.get("type")?.get("class")?.as_str()));
    }
    for (effect_type, own_fields) in OWN_FIELDS {
        for name in own_fields.iter().filter(|name| !effect_fields.contains(*name)) {
            out.push(format!("the status-effect struct has no field {name} (listed for effect type {effect_type})"));
        }
    }
    for name in ["type", "damageOverTimeElementalInfo"] {
        if !effect_fields.contains(name) {
            out.push(format!("the status-effect struct has no field {name}"));
        }
    }
    for class in SILENT_BEHAVIOURS {
        if !behaviours.contains(class) {
            out.push(format!("no dumped object has a behaviour of class {class} (listed as presentation only)"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_names_the_data_lacks() {
        let enums = json!({ "BonusType": { "ReloadTime": 1 }, "GameEntityMask": { "All": 7 } });
        let found = problems(&HashMap::new(), &enums);
        assert!(found.contains(&"enum BonusType has no member CritChance (named in the damage-relevant bonus types)".to_string()));
        assert!(!found.iter().any(|p| p.contains("member ReloadTime ") || p.contains("GameEntityMask") || p.contains("StatusEffectType")));

        let set: Obj = serde_json::from_value(json!({ "1": { "leveledSet": [{ "statusEffects": [{ "statusEffect": { "type": 1, "stunIncrease": 0 } }] }],
            "references": { "RefIds": [{ "rid": 1, "type": { "class": "SpawnFx" }, "data": { "sound": 1 } }] } } })).unwrap();
        let so = HashMap::from([("StatusEffectSetDefinitionSO".to_string(), set)]);
        let found = problems(&so, &json!({}));
        assert!(found.contains(&"no dumped object has a behaviour of class PlaySound (listed as presentation only)".to_string()));
        assert!(found.contains(&"the status-effect struct has no field knockbackIncrease (listed for effect type IncreaseKnockbacks)".to_string()));
        assert!(found.contains(&"no field name contains \"icon\" (listed as presentation only)".to_string()));
        assert!(!found.iter().any(|p| p.contains("class SpawnFx") || p.contains("stunIncrease") || p.contains("\"sound\"") || p.contains("field leveledSet")));
    }
}
