//! The `data` column of the class-skills table: a compact text description of what an ability, upgrade or skill
//! does, made from its game data. One line per thing, in a small notation of its own:
//!
//! ```text
//! [entity] MovementSpeed {requiredEntityMask=All}: Generic = 4/6/8/10/10        a bonus: type {own fields}: parameters
//!   behaviour: ApplyStatusEffectSet on[OnDirectAction] duration=0.1, set=Dash   a behaviour: class on[triggers] fields
//! set `Dash` (Buff)                                                              a status-effect set ...
//!   L1: BlockActions; IncreaseStuns(stunIncrease=0) durMod{customDuration=1}    ... and the effects of each level
//! ```
//!
//! The rules behind it:
//!
//! * A `key=value` listing shows a definition as the dump has it, minus what carries no information: fields at
//!   their default value, presentation-only fields (sounds, effects, icons, animations), and Addressables asset
//!   references. A reference to another dumped object shows as that object's name; a reference to anything else
//!   counts as empty.
//! * Enum and flag values are dumped as numbers; where the notation names them, the names come from the game's
//!   enums (see `ENUMS` in the crate root). Members are looked up in declaration order.
//! * Behaviours live in Unity's managed-reference registry of the owning asset (`references.RefIds`, each entry
//!   with a `type.class` and its `data`) and are pointed at by `rid`.
//! * Every status-effect set, ability and entity referenced from what has been shown is described in turn (a set:
//!   its levels and behaviours; an entity: its behaviours), which can reference more. Three rounds of this are
//!   made, so references further away than that are not followed.
//! * `StatusEffectSetDefinitionSO.leveledSet[].statusEffects[].statusEffect` is one struct used like a union:
//!   only the fields that belong to the effect's own `type` (`StatusEffectType`) mean anything, and stale values
//!   in the others are common. A known type shows its own fields only; if those are all at their default it
//!   shows the first as 0 and the stale ones under `other_fields_set`, as a hint that the data is unusual.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::bonus;
use crate::{fmt, truthy};

pub(crate) type Obj = Map<String, Value>;

/// A field whose name contains one of these is presentation only and is not shown. Which fields to hide is our
/// choice; the fragments themselves are pieces of the game's field names.
pub(crate) const VISUAL: [&str; 18] = [
    "display", "sound", "overrideSpawnFx", "shieldVfx", "spawnOffsetRotation", "healingTetherShieldDisplayData",
    "animationDefs", "icon", "Fx", "fx", "colorOverride", "overrideColor", "cameraShake", "addsCameraShake",
    "anim", "nearestEntityQuery", "postProcess", "renderingVolume",
];

/// Behaviour classes of the game that only present something. Their references are still followed; only their
/// own line is left out. Which classes to leave out is our choice; the names are the game's.
pub(crate) const SILENT_BEHAVIOURS: [&str; 5] = ["SpawnFx", "PlayUIAnimation", "QueueShaderEffect", "PlaySound", "AddCameraShake"];

/// The fields of a skill's bonus parameter that hold its per-rank numbers (the game's field names: the inputs of
/// `bonus::at_rank`); the line shows the evaluated numbers instead.
pub(crate) const RANK_FIELDS: [&str; 7] = [
    "initialBonus", "statBonusPerRank", "statBonusPerPlayerLevelPerRank", "initialBonusMultiplierPerRank",
    "upgradeUpToRank", "useManualRankValues", "manualRankValues",
];

/// Which fields of the status-effect struct belong to which effect type (`StatusEffectType` member -> field
/// names). This is knowledge about the game's code, which reads only these fields for that type; the data cannot
/// tell, because the struct always carries all of its fields. A type that is not listed shows all its fields.
pub(crate) const OWN_FIELDS: &[(&str, &[&str])] = &[
    ("DamageOverTime", &["damageOverTimeInfo", "damageOverTimeRepeatTime", "damageOverTimeElementalInfo", "damageOverTimeODEInfo"]),
    ("WeaponDamage", &["weaponDamageModifier"]),
    ("WeaponTimeBetweenShots", &["timeBetweenShotsModifier"]),
    ("WeaponAccuracy", &["weaponAccuracyModifier", "weaponMaxFiringErrorModifier"]),
    ("WeaponCritChance", &["weaponCritChanceModifier"]),
    ("WeaponCritDamage", &["weaponCritDamageModifier"]),
    ("WeaponReloadTime", &["weaponReloadTimeModifier"]),
    ("MovementSpeedModifier", &["movementModifier", "movementModifierRampRate", "movementModifierCustomRampDuration"]),
    ("IncomingDamageModifier", &["incomingDamageModifier"]),
    ("IncomingCritChance", &["incomingCritChanceModifier"]),
    ("KnockbackChance", &["additionalKnockbackChance"]),
    ("HealOverTime", &["healOverTimeInfo", "healOverTimeRepeatTime"]),
    ("PierceModifier", &["pierceModifier", "additivePierce"]),
    ("ResistanceModifier", &["resistanceModifier", "resistanceType", "takeFrom", "uncapped"]),
    ("AbilityCooldownModifier", &["abilityCooldownModifier"]),
    ("DOTTickRateModifier", &["dotTickRateModifier"]),
    ("StackableMovementSpeedModifier", &["movementModifier", "maxStackCount"]),
    ("StackableWeaponReloadTimeModifier", &["weaponReloadTimeModifier", "maxStackCount"]),
    ("StackableWeaponDamageModifier", &["weaponDamageModifier", "maxWeaponDamageModifier", "maxStackCount"]),
    ("MissingHealthBasedWeaponDamage", &["weaponDamageModifier", "maxWeaponDamageModifier", "incrementStep"]),
    ("ChanceToNotConsumeAmmo", &["chanceToNotConsumeAmmoType", "maxChanceToNotConsumeAmmo"]),
    ("ChanceToNotConsumePierce", &["chanceToNotConsumePierce"]),
    ("HealthOnKill", &["healthOnKill"]),
    ("DodgeChanceModifier", &["dodgeChanceModifier"]),
    ("AdditiveDodgeChance", &["additiveDodgeChance"]),
    ("DoBehaviours", &["onApplyBehaviours", "entityBehaviours", "onExpireBehaviours"]),
    ("IncreaseStuns", &["stunIncrease"]),
    ("IncreaseSlowdowns", &["slowdownIncrease"]),
    ("IncreaseKnockbacks", &["knockbackIncrease"]),
    ("FiringMovementReductionModifier", &["movementModifier"]),
    ("AggroMultiplier", &["aggroModifier", "aggroReset"]),
    ("AttackResistanceModifier", &["attackResistanceModifier", "resistanceType"]),
    ("ShieldRegen", &["shieldRegenPerSec"]),
    ("WeaponFalloffModifier", &["rangeFalloffRangeModifier", "rangeFalloffModifier", "pierceFalloffModifier"]),
    ("BlockActions", &[]),
    ("Invisibility", &[]),
    ("IgnoreCollisions", &[]),
    ("Immunity", &["immuneToTypes", "immuneSets", "immunityClearExisting"]),
];

fn own_fields(effect_type: &str) -> Option<&'static [&'static str]> {
    OWN_FIELDS.iter().find(|(name, _)| *name == effect_type).map(|(_, fields)| *fields)
}

pub(crate) fn items(v: Option<&Value>) -> &[Value] { v.and_then(Value::as_array).map_or(&[], Vec::as_slice) }

/// A whole number of the dump or of an enum table. Flag sets go up to 64 bits, signed or unsigned, so neither
/// `i64` nor `u64` holds them all.
pub(crate) fn whole(v: &Value) -> Option<i128> { v.as_i64().map(i128::from).or_else(|| v.as_u64().map(i128::from)) }

/// The number in an enum field, which the dump writes as `{"value": n}`; 0 when the field is absent.
fn wrapped(v: Option<&Value>) -> i128 { v.and_then(|x| x.get("value")).and_then(whole).unwrap_or(0) }

/// Nothing to show: null, zero, false, empty text or list, or an object with nothing to show in it.
pub(crate) fn is_default(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.values().all(is_default),
    }
}

/// The name in a resolved reference "REF<Class:name>"; any other text as it is.
pub(crate) fn ref_name(s: &str) -> &str { ref_parts(s).map_or(s, |(_, name)| name) }

fn ref_parts(s: &str) -> Option<(&str, &str)> {
    let mut inner = s.strip_prefix("REF<")?.chars();
    inner.next_back();
    inner.as_str().split_once(':')
}

fn number(v: &Value) -> String {
    match v {
        Value::Number(n) if n.is_f64() => fmt::num(n.as_f64().unwrap_or(0.0)),
        Value::Array(_) | Value::Object(_) => value(v),
        scalar => fmt::text(scalar),
    }
}

fn value(v: &Value) -> String {
    match v {
        Value::Object(m) if m.len() == 1 && m.contains_key("value") => number(&m["value"]),
        Value::Object(m) => format!("{{{}}}", listing(m)),
        Value::Array(a) => format!("[{}]", a.iter().map(value).collect::<Vec<_>>().join(", ")),
        Value::String(s) => ref_name(s).to_string(),
        _ => number(v),
    }
}

/// "key=value, key=value" of already stripped fields.
pub(crate) fn listing<'v>(fields: impl IntoIterator<Item = (&'v String, &'v Value)>) -> String {
    fields.into_iter().map(|(k, v)| format!("{k}={}", value(v))).collect::<Vec<_>>().join(", ")
}

/// What one row has shown so far: the objects it referenced, still to be described, and the status-effect types
/// that occurred (the table's damage-relevance column looks at those).
#[derive(Default)]
pub(crate) struct Seen {
    sets: Vec<String>,
    abilities: Vec<String>,
    entities: Vec<String>,
    pub effect_types: HashSet<String>,
}

impl Seen {
    /// Remembers every set, ability and entity referenced anywhere inside `v`, in order of first reference.
    fn note(&mut self, v: &Value) {
        match v {
            Value::String(s) => {
                let Some((class, name)) = ref_parts(s) else { return };
                let list = match class {
                    "StatusEffectSetDefinitionSO" => &mut self.sets,
                    "AbilityDefinitionSO" => &mut self.abilities,
                    "EntityDefinitionSO" => &mut self.entities,
                    _ => return,
                };
                if !list.iter().any(|n| n == name) {
                    list.push(name.to_string());
                }
            }
            Value::Object(m) => m.values().for_each(|x| self.note(x)),
            Value::Array(a) => a.iter().for_each(|x| self.note(x)),
            _ => {}
        }
    }
}

/// How a bonus's numbers are tabulated.
#[derive(Clone, Copy)]
pub(crate) enum Scale<'s> {
    /// a passive skill's bonus: one number per rank, 1 to this
    Ranks(i64),
    /// an ability's bonus: one number per listed player level
    Levels(&'s [i64]),
}

/// The game's enums as the notation needs them: value -> member name, or the members in declaration order for
/// flag sets (several names per value). An enum that was not given names nothing: every value shows as `?`.
pub(crate) struct Enums<'a> {
    bonus_type: HashMap<i128, &'a str>,
    parameter_type: HashMap<i128, &'a str>,
    condition_type: HashMap<i128, &'a str>,
    effect_type: HashMap<i128, &'a str>,
    trigger_condition_type: HashMap<i128, &'a str>,
    damage_type: HashMap<i128, &'a str>,
    set_type: HashMap<i128, &'a str>,
    pub skill_point_type: HashMap<i128, &'a str>,
    pub description_kind: HashMap<i128, &'a str>,
    /// where the number in an ability upgrade's description comes from (`AbilityDescriptionParameterSource`)
    pub description_source: HashMap<i128, &'a str>,
    pub effect_type_names: HashMap<i128, &'a str>,
    trigger_flags: Vec<(&'a str, i128)>,
    scope_flags: Vec<(&'a str, i128)>,
    entity_mask: Vec<(&'a str, i128)>,
    /// values of the members the notation treats specially (see `NAMED_MEMBERS`); `None` when not given
    any_condition: Option<i128>,
    always_active: Option<i128>,
    every_entity: Option<i128>,
}

pub(crate) const HEALTH_FACTOR: &str = "PlayerHealthFactorCalculated";
pub(crate) const HEALTH_FACTOR_PER_MINUTE: &str = "PlayerHealthFactorCalculatedPerMinute";

/// Enum members the code refers to by name, as (enum, member): conditions joined by "any of" rather than "all
/// of"; an ability that triggers itself; the entity mask that excludes nothing; and the two tooltip values that
/// are a fraction of maximum health rather than a bonus parameter.
pub(crate) const NAMED_MEMBERS: [(&str, &str); 5] = [
    ("BehaviourConditionsCheckType", "Any"),
    ("AbilityActivationBehaviour", "AlwaysActive"),
    ("GameEntityMask", "All"),
    ("PassiveSkillDescriptionBonusType", HEALTH_FACTOR),
    ("PassiveSkillDescriptionBonusType", HEALTH_FACTOR_PER_MINUTE),
];

fn members<'a>(enums: &'a Value, name: &str) -> Vec<(&'a str, i128)> {
    let table = enums.get(name).and_then(Value::as_object);
    table.into_iter().flatten().filter_map(|(member, v)| Some((member.as_str(), whole(v)?))).collect()
}

/// Two members with one value: the later one names it.
fn names<'a>(enums: &'a Value, name: &str) -> HashMap<i128, &'a str> {
    members(enums, name).into_iter().map(|(member, v)| (v, member)).collect()
}

pub(crate) fn named<'a>(table: &HashMap<i128, &'a str>, v: i128) -> &'a str { table.get(&v).copied().unwrap_or("?") }

/// Whether a flag value is a single bit (the reference counts the bits of its magnitude).
fn single_bit(flag: i128) -> bool { flag.unsigned_abs().count_ones() == 1 }

impl<'a> Enums<'a> {
    pub(crate) fn new(enums: &'a Value) -> Enums<'a> {
        let member = |(table, name): (&str, &str)| enums.get(table).and_then(|t| t.get(name)).and_then(whole);
        Enums {
            bonus_type: names(enums, "BonusType"),
            parameter_type: names(enums, "BonusParameterType"),
            condition_type: names(enums, "BehaviourConditionType"),
            effect_type: names(enums, "StatusEffectType"),
            trigger_condition_type: names(enums, "AbilityTriggerConditionType"),
            damage_type: names(enums, "DamageType"),
            set_type: names(enums, "StatusEffectSetType"),
            skill_point_type: names(enums, "SkillPointType"),
            description_kind: names(enums, "PassiveSkillDescriptionBonusType"),
            description_source: names(enums, "AbilityDescriptionParameterSource"),
            effect_type_names: names(enums, "StatusEffectType"),
            trigger_flags: members(enums, "BehaviourTriggerFlags"),
            scope_flags: members(enums, "BonusScopeFlags"),
            entity_mask: members(enums, "GameEntityMask"),
            any_condition: member(NAMED_MEMBERS[0]),
            always_active: member(NAMED_MEMBERS[1]),
            every_entity: member(NAMED_MEMBERS[2]),
        }
    }

    /// "OnHit|OnKill" for a `BehaviourTriggerFlags` value: its single-bit members; the number if none match.
    fn triggers(&self, flags: i128) -> String {
        if flags == 0 {
            return "-".into();
        }
        let set = self.trigger_flags.iter().filter(|(_, bit)| *bit != 0 && flags & bit == *bit && single_bit(*bit));
        let names: Vec<&str> = set.map(|(name, _)| *name).collect();
        if names.is_empty() { flags.to_string() } else { names.join("|") }
    }
}

/// The dumped objects the descriptions are made from.
pub(crate) struct Game<'a> {
    /// path id -> (class, asset name) of every dumped object given
    pub index: HashMap<&'a str, (&'a str, &'a str)>,
    pub abilities: HashMap<&'a str, &'a Obj>,
    pub sets: HashMap<&'a str, &'a Obj>,
    pub entities: HashMap<&'a str, &'a Obj>,
    pub enums: Enums<'a>,
    /// the highest character level: numbers that grow with the player's level are given at this level
    pub level_cap: i64,
}

impl Game<'_> {
    /// The notation's view of a dumped value: see the first rule in the module description.
    pub(crate) fn strip(&self, v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                if m.len() == 2 && m.contains_key("m_FileID") && m.contains_key("m_PathID") {
                    // a reference to another object (Unity PPtr); path id 0 is the null reference
                    let id = m["m_PathID"].to_string();
                    return match self.index.get(id.as_str()) {
                        Some((class, name)) if id != "0" => Value::String(format!("REF<{class}:{name}>")),
                        _ => Value::Null,
                    };
                }
                if m.contains_key("m_AssetGUID") {
                    return Value::Null;
                }
                let mut out = Map::new();
                for (k, x) in m {
                    if VISUAL.iter().any(|part| k.contains(part)) {
                        continue;
                    }
                    let y = self.strip(x);
                    if !is_default(&y) {
                        out.insert(k.clone(), y);
                    }
                }
                Value::Object(out)
            }
            // an empty or null element keeps its place: only fields are dropped, never list elements
            Value::Array(a) => Value::Array(a.iter().map(|x| self.strip(x)).collect()),
            // five decimals take the single-precision noise off the dumped numbers
            Value::Number(n) if n.is_f64() => Value::from(fmt::round(n.as_f64().unwrap_or(0.0), 5)),
            _ => v.clone(),
        }
    }

    pub(crate) fn strip_field(&self, o: &Obj, field: &str) -> Value { o.get(field).map_or(Value::Null, |v| self.strip(v)) }

    /// The registry of an asset, from its stripped `references`: `rid` -> entry.
    pub(crate) fn registry<'v>(&self, references: &'v Value) -> HashMap<i64, &'v Value> {
        items(references.get("RefIds")).iter().filter_map(|e| Some((e.get("rid")?.as_i64()?, e))).collect()
    }

    /// The registry entries a stripped behaviour list (`{"m_List": [{"rid": n}, ...]}`) points at.
    pub(crate) fn listed<'v>(list: &Value, registry: &HashMap<i64, &'v Value>) -> Vec<&'v Value> {
        let rids = items(list.get("m_List")).iter().filter_map(|e| e.get("rid")?.as_i64());
        rids.filter_map(|rid| registry.get(&rid).copied()).collect()
    }

    fn condition(&self, condition: &Value) -> String {
        let fields = condition.as_object().into_iter().flatten().filter(|(k, _)| *k != "conditionType");
        let (name, fields) = (named(&self.enums.condition_type, wrapped(condition.get("conditionType"))), listing(fields));
        if fields.is_empty() { name.to_string() } else { format!("{name}({fields})") }
    }

    /// One line per behaviour: `<prefix><class> on[<triggers>] if[<conditions>] <fields>`.
    fn behaviours(&self, entries: &[&Value], prefix: &str, seen: &mut Seen, out: &mut Vec<String>) {
        let empty = Obj::new();
        for entry in entries {
            let class = entry.get("type").and_then(|t| t.get("class")).and_then(Value::as_str).unwrap_or("");
            let data = entry.get("data").and_then(Value::as_object).unwrap_or(&empty);
            data.values().for_each(|v| seen.note(v));
            if SILENT_BEHAVIOURS.contains(&class) {
                continue;
            }
            let mut line = format!("{prefix}{class} on[{}]", self.enums.triggers(wrapped(data.get("triggerFlags"))));
            let conditions: Vec<String> = items(data.get("conditions")).iter().map(|c| self.condition(c)).collect();
            if !conditions.is_empty() {
                let any = Some(wrapped(data.get("conditionsCheck"))) == self.enums.any_condition;
                let joint = if any { " | " } else { " & " };
                line += &format!(" if[{}]", conditions.join(joint));
            }
            let fields = listing(data.iter().filter(|(k, _)| !matches!(k.as_str(), "triggerFlags" | "conditions" | "conditionsCheck")));
            if !fields.is_empty() {
                line += &format!(" {fields}");
            }
            out.push(line);
        }
    }

    /// One bonus definition: a line with its type, own fields and parameters, then the behaviours it adds.
    /// Returns the bonus type's name.
    pub(crate) fn bonus(
        &self,
        raw: &Value,
        scale: Scale,
        registry: &HashMap<i64, &Value>,
        prefix: &str,
        seen: &mut Seen,
        out: &mut Vec<String>,
    ) -> &str {
        let bonus_type = named(&self.enums.bonus_type, wrapped(raw.get("bonusType")));
        let stripped = self.strip(raw);
        let damage_type = String::from("specificDamageType");
        let mut own: Vec<(&String, Value)> = Vec::new();
        let mut added = Vec::new();
        for (k, v) in stripped.as_object().into_iter().flatten() {
            let flags = whole(v).unwrap_or(0);
            let shown = match k.as_str() {
                "bonusType" | "parameters" => continue,
                "additionalBehaviours" | "additionalAbilityBehaviours" => {
                    added.extend(Self::listed(v, registry));
                    continue;
                }
                "scopeFlags" => {
                    let set = self.enums.scope_flags.iter().filter(|(_, bit)| *bit != 0 && flags & bit != 0);
                    Value::from(set.map(|(name, _)| *name).collect::<Vec<_>>().join("|"))
                }
                "specificDamageType" | "newDamageType" => match whole(v).and_then(|t| self.enums.damage_type.get(&t)) {
                    Some(name) => Value::from(*name),
                    None => v.clone(),
                },
                "requiredTriggerFlags" => Value::from(self.enums.triggers(flags)),
                "requiredEntityMask" => match wrapped(Some(v)) {
                    mask if Some(mask) == self.enums.every_entity => Value::from(NAMED_MEMBERS[2].1),
                    mask => {
                        let set = self.enums.entity_mask.iter().filter(|(_, bit)| *bit != 0 && single_bit(*bit) && mask & bit != 0);
                        Value::from(set.map(|(name, _)| *name).collect::<Vec<_>>().join("|"))
                    }
                },
                _ => v.clone(),
            };
            own.push((k, shown));
        }
        // a bonus limited to one damage type whose type field is at its default, and so not listed: name that type
        if own.iter().any(|(k, _)| *k == "useSpecificDamageType") && !own.iter().any(|(k, _)| *k == "specificDamageType") {
            own.push((&damage_type, Value::from(named(&self.enums.damage_type, 0))));
        }
        own.iter().for_each(|(_, v)| seen.note(v));

        let mut parameters = Vec::new();
        for raw_parameter in items(raw.get("parameters")) {
            let parameter = self.strip(raw_parameter);
            let mut text = named(&self.enums.parameter_type, wrapped(parameter.get("parameterType"))).to_string();
            let mut tags = Vec::new();
            if let Some(condition) = parameter.get("conditionType") {
                tags.push(format!("cond {}", named(&self.enums.condition_type, wrapped(Some(condition)))));
            }
            let (numbers, evaluated): (String, &[&str]) = match scale {
                Scale::Ranks(max) => {
                    let value = |rank| fmt::num(fmt::round(bonus::at_rank(raw_parameter, rank, self.level_cap), 4));
                    let per_rank = (1..=max).map(value);
                    let mut numbers = per_rank.collect::<Vec<_>>().join("/");
                    if let Some(per_level) = raw_parameter.get("statBonusPerPlayerLevelPerRank").filter(|v| truthy(v)) {
                        let per_level = fmt::num(fmt::round(per_level.as_f64().unwrap_or(0.0), 4));
                        numbers += &format!(" (at level {}; +{per_level} per level per rank)", self.level_cap);
                    }
                    (numbers, &RANK_FIELDS)
                }
                Scale::Levels(levels) if raw_parameter.get("bonusLevelRanges").is_some_and(truthy) => {
                    let at = |level: &i64| format!("L{level}: {}", fmt::num(fmt::round(bonus::at_level(raw_parameter, *level), 4)));
                    let first = &raw_parameter["bonusLevelRanges"][0];
                    let step = fmt::signed_general(fmt::round(first.get("statBonusPerLevelIncrement").and_then(Value::as_f64).unwrap_or(0.0), 4));
                    let every = first.get("levelIncrementPerStatBonus").map_or("1".into(), fmt::whole);
                    let from = first.get("minLevel").map_or("0".into(), fmt::whole);
                    let numbers = levels.iter().map(at).collect::<Vec<_>>().join(", ");
                    (format!("{numbers} ({step} per {every} level(s) from level {from})"), &["bonusLevelRanges"])
                }
                Scale::Levels(_) => (String::new(), &[]),
            };
            if !numbers.is_empty() {
                text += &format!(" = {numbers}");
            }
            let fields: Vec<_> = parameter.as_object().into_iter().flatten()
                .filter(|(k, _)| !matches!(k.as_str(), "parameterType" | "conditionType") && !evaluated.contains(&k.as_str()))
                .collect();
            if !fields.is_empty() {
                fields.iter().for_each(|(_, v)| seen.note(v));
                tags.push(listing(fields));
            }
            if !tags.is_empty() {
                text += &format!(" [{}]", tags.join("; "));
            }
            parameters.push(text);
        }

        let mut line = format!("{prefix}{bonus_type}");
        if !own.is_empty() {
            line += &format!(" {{{}}}", listing(own.iter().map(|(k, v)| (*k, v))));
        }
        if !parameters.is_empty() {
            line += &format!(": {}", parameters.join(" | "));
        }
        out.push(line);
        self.behaviours(&added, &format!("{prefix}  behaviour: "), seen, out);
        bonus_type
    }

    /// "cooldown 15s, duration 4s, capacity 2, dualWield": the timing facts and switches of an ability that are set.
    fn ability_header(&self, ability: &Obj) -> String {
        let mut facts = Vec::new();
        let set = |field: &str| ability.get(field).filter(|v| truthy(v));
        for (field, label) in [
            ("cooldown", "cooldown"), ("abilityDuration", "duration"), ("replenishTime", "replenish"),
            ("chargeTime", "charge"), ("castTime", "cast"),
        ] {
            if let Some(seconds) = set(field).and_then(Value::as_f64) {
                facts.push(format!("{label} {}s", fmt::num(fmt::round(seconds, 3))));
            }
        }
        if let Some(capacity) = set("triggerCapacity") {
            facts.push(format!("capacity {}", fmt::whole(capacity)));
        }
        for switch in [
            "hasInfiniteCapacity", "replenishCapacityOverTime", "dualWield", "targeted", "autoTriggerOnLowHealth",
            "disableIfTriggerConditionsUnmet", "triggerCooldownOnUsingActiveBehaviours",
        ] {
            if set(switch).is_some() {
                facts.push(switch.to_string());
            }
        }
        let activation = ability.get("activationBehaviour").and_then(whole);
        if activation.is_some() && activation == self.enums.always_active {
            facts.push(format!("{} (auto-triggers when its trigger conditions hold)", NAMED_MEMBERS[1].1));
        }
        facts.join(", ")
    }

    /// An ability: heading, trigger conditions and its three behaviour lists.
    pub(crate) fn ability(&self, name: &str, seen: &mut Seen, out: &mut Vec<String>) {
        let Some(ability) = self.abilities.get(name) else {
            out.push(format!("ability {name}: (not in dump)"));
            return;
        };
        let references = self.strip_field(ability, "references");
        let registry = self.registry(&references);
        out.push(format!("ability `{name}`: {}", self.ability_header(ability)));
        for condition in items(Some(&self.strip_field(ability, "triggerConditions"))) {
            let fields: Vec<_> = condition.as_object().into_iter().flatten().filter(|(k, _)| *k != "type").collect();
            fields.iter().for_each(|(_, v)| seen.note(v));
            let kind = named(&self.enums.trigger_condition_type, condition.get("type").and_then(whole).unwrap_or(0));
            let fields = listing(fields);
            out.push(format!("  trigger condition: {kind}{}{fields}", if fields.is_empty() { "" } else { " " }));
        }
        for list in ["behaviours", "activeBehaviours", "cancelledBehaviours"] {
            let entries = Self::listed(&self.strip_field(ability, list), &registry);
            self.behaviours(&entries, &format!("  {}: ", &list[..list.len() - 1]), seen, out);
        }
    }

    fn entity(&self, name: &str, seen: &mut Seen, out: &mut Vec<String>) {
        let Some(entity) = self.entities.get(name) else {
            out.push(format!("entity {name}: (not in dump)"));
            return;
        };
        let facts: Vec<(&String, Value)> = entity.iter()
            .filter(|(k, _)| matches!(k.as_str(), "hasLifetime" | "lifetime"))
            .map(|(k, v)| (k, self.strip(v))).filter(|(_, v)| !is_default(v)).collect();
        let facts = listing(facts.iter().map(|(k, v)| (*k, v)));
        out.push(format!("entity `{name}`{}{facts}", if facts.is_empty() { "" } else { " " }));
        let references = self.strip_field(entity, "references");
        let entries: Vec<&Value> = items(references.get("RefIds")).iter().collect();
        self.behaviours(&entries, "  behaviour: ", seen, out);
    }

    fn set(&self, name: &str, seen: &mut Seen, out: &mut Vec<String>) {
        let Some(set) = self.sets.get(name) else {
            out.push(format!("set {name}: (not in dump)"));
            return;
        };
        let mut tags = Vec::new();
        let set_type = self.strip_field(set, "setType");
        if !is_default(&set_type) {
            tags.push(named(&self.enums.set_type, wrapped(Some(&set_type))));
        }
        if !is_default(&self.strip_field(set, "isImmutable")) {
            tags.push("immutable");
        }
        out.push(format!("set `{name}`{}", if tags.is_empty() { String::new() } else { format!(" ({})", tags.join(", ")) }));
        let empty = Obj::new();
        for (level, effects) in items(Some(&self.strip_field(set, "leveledSet"))).iter().enumerate() {
            let mut parts = Vec::new();
            for entry in items(effects.get("statusEffects")) {
                let effect = entry.get("statusEffect").and_then(Value::as_object).unwrap_or(&empty);
                let effect_type = named(&self.enums.effect_type, wrapped(effect.get("type")));
                seen.effect_types.insert(effect_type.to_string());
                let fields = || effect.iter().filter(|(k, _)| *k != "type");
                // every effect also carries an elemental block; at its resting value {setLevel: 1} it says nothing
                let resting = |(k, v): &(&String, &Value)| {
                    *k == "damageOverTimeElementalInfo"
                        && v.as_object().is_some_and(|m| m.len() == 1 && m.get("setLevel").and_then(Value::as_f64) == Some(1.0))
                };
                let shown = match own_fields(effect_type) {
                    None => listing(fields().filter(|f| !resting(f))),
                    Some(own) => {
                        let shown = listing(fields().filter(|(k, _)| own.contains(&k.as_str())));
                        if !shown.is_empty() || own.is_empty() {
                            shown
                        } else {
                            let other = listing(fields().filter(|(k, _)| *k != "damageOverTimeElementalInfo"));
                            match other.is_empty() {
                                true => format!("{}=0", own[0]),
                                false => format!("{}=0, other_fields_set={{{other}}}", own[0]),
                            }
                        }
                    }
                };
                let mut part = effect_type.to_string();
                if !shown.is_empty() {
                    part += &format!("({shown})");
                }
                if let Some(modifier) = entry.get("durationModifier").and_then(Value::as_object).filter(|m| !m.is_empty()) {
                    part += &format!(" durMod{{{}}}", listing(modifier));
                }
                parts.push(part);
            }
            out.push(format!("  L{}: {}", level + 1, parts.join("; ")));
        }
        let references = self.strip_field(set, "references");
        let entries: Vec<&Value> = items(references.get("RefIds")).iter().collect();
        self.behaviours(&entries, "  behaviour: ", seen, out);
    }

    /// Describes what the row referenced, and what those descriptions reference, three rounds deep. Each round
    /// works on what was known when it started, which is what bounds the depth. `own_ability` is the ability the
    /// row belongs to: it is not described again.
    pub(crate) fn follow_references(&self, seen: &mut Seen, own_ability: Option<&str>, out: &mut Vec<String>) {
        let mut abilities: HashSet<String> = own_ability.map(str::to_string).into_iter().collect();
        let (mut entities, mut sets) = (HashSet::new(), HashSet::new());
        for _ in 0..3 {
            for name in seen.abilities.clone() {
                if abilities.insert(name.clone()) {
                    self.ability(&name, seen, out);
                }
            }
            for name in seen.entities.clone() {
                if entities.insert(name.clone()) {
                    self.entity(&name, seen, out);
                }
            }
            for name in seen.sets.clone() {
                if sets.insert(name.clone()) {
                    self.set(&name, seen, out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn game(enums: &Value) -> Game<'_> {
        let (index, none) = (HashMap::from([("7", ("StatusEffectSetDefinitionSO", "Dash"))]), HashMap::new);
        Game { index, abilities: none(), sets: none(), entities: none(), enums: Enums::new(enums), level_cap: 60 }
    }

    #[test]
    fn stripping_and_listing() {
        let enums = json!({});
        let g = game(&enums);
        let v = g.strip(&json!({ "duration": 0.100000001, "set": { "m_FileID": 0, "m_PathID": 7 }, "icon": 3, "none": { "m_FileID": 0, "m_PathID": 9 },
            "asset": { "m_AssetGUID": "ab" }, "zero": { "a": 0, "b": [] }, "list": [{}, 1.5, true], "kind": { "value": 2 }, "n": 3 }));
        assert_eq!(listing(v.as_object().unwrap()), "duration=0.1, set=Dash, list=[{}, 1.5, True], kind=2, n=3");
        let mut seen = Seen::default();
        seen.note(&v);
        assert_eq!(seen.sets, ["Dash"]);
    }

    #[test]
    fn flag_names() {
        let enums = json!({ "BehaviourTriggerFlags": { "None": 0, "OnHit": 1, "OnKill": 2, "Both": 3, "OnDeath": 8 } });
        let e = Enums::new(&enums);
        assert_eq!([e.triggers(0), e.triggers(3), e.triggers(10), e.triggers(4)], ["-", "OnHit|OnKill", "OnKill|OnDeath", "4"]);
    }

    #[test]
    fn behaviour_line() {
        let enums = json!({ "BehaviourTriggerFlags": { "OnHit": 1 }, "BehaviourConditionType": { "HasSet": 4, "Always": 0 },
            "BehaviourConditionsCheckType": { "All": 1, "Any": 2 } });
        let g = game(&enums);
        let entry = json!({ "rid": 5, "type": { "class": "ApplyStatusEffectSet" }, "data": { "triggerFlags": { "value": 1 }, "conditionsCheck": { "value": 2 },
            "conditions": [{ "conditionType": { "value": 4 }, "set": "REF<StatusEffectSetDefinitionSO:Burn>" }, { "invert": 1 }], "set": "REF<StatusEffectSetDefinitionSO:Dash>" } });
        let (mut seen, mut out) = (Seen::default(), Vec::new());
        g.behaviours(&[&entry], "  behaviour: ", &mut seen, &mut out);
        assert_eq!(out, ["  behaviour: ApplyStatusEffectSet on[OnHit] if[HasSet(set=Burn) | Always(invert=1)] set=Dash"]);
        assert_eq!(seen.sets, ["Burn", "Dash"]);
    }
}
