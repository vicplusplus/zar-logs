//! A weapon as the loadout screen shows it: name, category, damage type, pellets, and the sheet numbers (damage,
//! crit chance and multiplier, fire interval, magazine, reload) of the weapon at a quality and upgrade level with
//! its mods, before class skills and in-mission buffs. This is what tools/dps.py computes with
//! `bake(w, quality, upgrade, sum_feat(mods), Env(cls="none"))` after `set_enh_points(True)`, except that the
//! effect of an enhancement is derived from its definition instead of that tool's hand-made table.
//!
//! From `WeaponDefinitionSO`: base damage, crit chance and multiplier, damage type, time between shots, magazine
//! capacity, reload time, pellet count, category, flags, and the reference to the weapon's crafting table.
//! From `GearCrafting` (through that reference): the damage factor of each quality and the gain per upgrade level.
//! From `GearModDefinition` and `GearModCrafting`: the stat points of the mods, what a point is worth, and how
//! power turns into extra points.
//! From `GearModEnhancementDefinitionSO`: each enhancement's bonuses and the weapons it works on.
//!
//! The game builds a player weapon in stages, each applied to the result of the one before: quality and upgrade
//! scale the base damage, then the bonuses of all mods and their enhancements are added up per kind and applied
//! once (`BonusHelpers.ApplyBonusToWeapon`; the player's own bonuses follow and are not part of the sheet).
//!
//! An enhancement's bonuses are evaluated the way the game does (`BonusHelpers.AddBonusToBonusResult`): the rank is
//! the quality of the mod it sits on, a parameter is `perRank * rank + initial * multiplierPerRank^(rank - 1)`
//! or a listed per-rank value, a mod-point parameter is first turned into a real one, and each bonus type reads a
//! particular parameter and feeds a particular weapon number. Bonus types that feed none of the six sheet numbers
//! have no effect here; that covers everything conditional, because the game expresses conditions as behaviours,
//! status effects and weapon stat modifiers, which are bonus types of their own. A bonus that names a target
//! (an entity flag mask) is kept off the weapon by the game and so off the sheet. An enhancement on a weapon it
//! does not fit (`Gear.GearFitsEnhancementRequirements`) is skipped entirely.
//!
//! The points of a normal mod count as percent for crit and magazine and as percent off for reload, with
//! tools/dps.py's reading of the band table. A point of Damage is one point of flat damage, added after the
//! quality factor and the percent bonuses (`BonusHelpers.ApplyBonusToDamageInfo`: damage x (1 + multipliers) +
//! flat), as seen in game on the weapon card; tools/dps.py used to count it as a percent.
//! Arithmetic is in doubles as there; the game computes in single precision.

use std::collections::{HashMap, HashSet};

use crate::data::{self, at, int, num, GearTable, Obj};
use crate::enums::{Bonus, CodeValues, Enums};
use crate::{py, EnhancementEffect, ModSpec, Sheet, WeaponInfo};

/// Asset names of enhancements carry this prefix; loadouts and the reference name them without it.
const ENHANCEMENT_PREFIX: &str = "ModEnhancement_";

// The mod-stage bonuses the sheet numbers use, each the sum over the weapon's mods.
const DAMAGE: usize = 0; // percent
const DAMAGE_FLAT: usize = 1;
const CRIT: usize = 2; // percent points
const MAGAZINE: usize = 3; // percent
const MAGAZINE_FLAT: usize = 4;
const RELOAD: usize = 5; // fraction of the reload time
const INTERVAL: usize = 6; // fraction of the time between shots
const IMPACT: usize = 7; // fraction of damage
const CRIT_MULTIPLIER: usize = 8;
const PISTOL_SHARED: usize = 9; // percent of damage on pistols: the largest among the mods counts
const PISTOL_STEP: usize = 10; // percent of damage on pistols on top of it: these add up
type Features = [f64; 11];

/// A mod stat as tools/dps.py tells them apart: all resistances are one kind, and a bonus type it does not know
/// is kept with a point worth 1 instead of being refused.
#[derive(Clone, Copy)]
enum Stat {
    Damage,
    Crit,
    Accuracy,
    Reload,
    Magazine,
    Movement,
    Resistance,
    Other,
}

struct ModDefinition {
    native_quality: i64,
    stats: Vec<(Stat, i64)>,
}

/// How mod power turns into stat points (`GearModCrafting`).
struct PointRules {
    power_per_point: i64,
    points_per_quality: i64,
    offset_min: i64,
    offset_max: i64,
    enhancement_power: Vec<(i64, i64)>,
}

struct Parameter {
    kind: i64,
    initial: f64,
    per_rank: f64,
    multiplier_per_rank: f64,
    up_to_rank: i64,
    /// Values listed per rank; they replace the formula.
    manual: Option<Vec<f64>>,
    weapon_categories: i64,
    deployable_categories: i64,
    entity_flags: i64,
}

struct EnhancementBonus {
    raw_type: i64,
    kind: Option<Bonus>,
    parameters: Vec<Parameter>,
}

struct Enhancement {
    filter_flags: i64,
    damage_types: i64,
    weapon_categories: i64,
    weapon_flags: i64,
    no_weapon_flags: i64,
    /// Weapons the enhancement works on whatever the filter says.
    exceptions: Vec<String>,
    bonuses: Vec<EnhancementBonus>,
}

struct Weapon {
    name: String,
    info: WeaponInfo,
    category: i64,
    damage_type: i64,
    flags: i64,
    deployable_category: i64,
    has_charge_effects: bool,
    damage: f64,
    crit: f64,
    crit_multiplier: f64,
    time_between_shots: f64,
    magazine: i64,
    reload: f64,
    /// `None` when the weapon has no readable crafting table; it then has no sheet.
    crafting: Option<GearTable>,
    explosion: Option<Explosion>,
}

/// The explosion a weapon's projectile sets off: its `Explode` behaviour.
struct Explosion {
    /// The explosion's own damage, before quality.
    damage: f64,
    /// `ExplodeDataFlags.ScaleWithProjectileDamage`: the explosion deals the damage the projectile carries times
    /// `scale` instead of its own.
    scales_with_projectile: bool,
    scale: f64,
}

/// The data of the first behaviour of a class in one of a definition's behaviour lists (`projBehaviours`, ..).
/// The lists hold ids into the definition's `references`.
fn behaviour<'a>(d: &'a Obj, list: &str, class: &str) -> Option<&'a serde_json::Value> {
    let refs = at(d.get("references")?, &["RefIds"])?.as_array()?;
    at(d.get(list)?, &["m_List"])?.as_array()?.iter().find_map(|item| {
        let rid = item.get("rid")?;
        let r = refs.iter().find(|r| r.get("rid") == Some(rid))?;
        (at(r, &["type", "class"])?.as_str()? == class).then(|| r.get("data")).flatten()
    })
}

/// A weapon's explosion: on the projectile entity (a rocket), else on the projectile's contact (pellets that
/// each explode). An explosion that only a spawned entity's own definition carries is not looked for: entity
/// definitions are not among the tables, and no player weapon has one today.
fn explosion(d: &Obj, enums: &Enums) -> Option<Explosion> {
    let e = behaviour(d, "projEntityBehaviours", "Explode").or_else(|| behaviour(d, "projBehaviours", "Explode"))?;
    Some(Explosion {
        damage: num(at(e, &["damage", "damage"])?)?,
        scales_with_projectile: int(e.get("flags")?)? & enums.explosion_scales_with_projectile != 0,
        scale: num(e.get("damageScalingMultiplier")?)?,
    })
}

pub(crate) struct Weapons {
    weapons: HashMap<String, Weapon>,
    /// `None` when a definition cannot be decoded: the reference reads all before it answers for one.
    mods: Option<HashMap<String, ModDefinition>>,
    rules: Option<PointRules>,
    /// Percent per point by band for crit, movement, reload, magazine and resistance, as serialised.
    bands: [Option<Vec<f64>>; 5],
    enhancements: HashMap<String, Enhancement>,
    parameter: crate::enums::ParameterTypes,
    filter: crate::enums::FilterFlags,
    code: CodeValues,
    pistol: i64,
    last_quality: i64,
    mod_slots: usize,
}

fn mod_definition(d: &Obj, enums: &Enums) -> Option<ModDefinition> {
    let mut stats = Vec::new();
    for stat in d.get("bonusStats")?.as_array()? {
        let kind = match enums.bonus(int(at(stat, &["bonusType", "value"])?)?) {
            Some(Bonus::ProjectileDamage) => Stat::Damage,
            Some(Bonus::CritChance) => Stat::Crit,
            Some(Bonus::WeaponAccuracy) => Stat::Accuracy,
            Some(Bonus::ReloadTime) => Stat::Reload,
            Some(Bonus::WeaponMagazineCapacity) => Stat::Magazine,
            Some(Bonus::ReducedMovementPenalty) => Stat::Movement,
            Some(Bonus::DamageResistance) => Stat::Resistance,
            _ => Stat::Other,
        };
        let parameter = stat.get("parameters")?.as_array()?.first()?;
        stats.push((kind, py::round_int(num(parameter.get("initialBonus")?)?)?));
    }
    Some(ModDefinition { native_quality: int(d.get("minimumUnlockQuality")?)?, stats })
}

fn point_rules(crafting: &Obj) -> Option<PointRules> {
    let field = |name: &str| crafting.get(name).and_then(int);
    Some(PointRules {
        power_per_point: field("powerPerImprovementPoint").filter(|p| *p != 0)?,
        points_per_quality: field("improvementPointsPerQuality").filter(|p| *p > 0)?,
        offset_min: field("powerLevelOffsetMinimumValue")?,
        offset_max: field("powerLevelOffsetMaximumValue")?,
        enhancement_power: data::int_dict(crafting.get("enhancementPowerOffsets")?)?
            .into_iter().filter_map(|(quality, power)| Some((quality, int(power)?))).collect(),
    })
}

impl ModDefinition {
    /// Points per stat of an instance of this mod: the definition's points plus one extra point per unit of
    /// power the instance has above the definition (quality steps, the power offset, an enhancement's power),
    /// dealt one at a time over the positive stats, largest first.
    fn points(&self, rules: &PointRules, last_quality: i64, quality: i64, offset: i64, has_enh: bool) -> Vec<(Stat, i64)> {
        let steps = (quality - self.native_quality).clamp(0, last_quality);
        let offset = rules.offset_min.max(offset.min(rules.offset_max));
        let enhancement = rules.enhancement_power.iter().rev().find(|(q, _)| has_enh && *q == quality).map_or(0, |(_, p)| *p);
        let above = rules.power_per_point * (rules.points_per_quality * steps + offset) + enhancement;
        let extra = py::floor_div(above, rules.power_per_point).unwrap_or(0);
        let mut stats = self.stats.clone();
        let mut order: Vec<usize> = (0..stats.len()).filter(|i| stats[*i].1 > 0).collect();
        order.sort_by_key(|i| -stats[*i].1);
        if !order.is_empty() {
            for k in 0..extra.max(0) as usize {
                stats[order[k % order.len()]].1 += 1;
            }
        }
        stats
    }
}

fn parameter(p: &serde_json::Value) -> Option<Parameter> {
    let number = |name: &str| p.get(name).and_then(num);
    let mask = |name: &str| p.get(name).and_then(int).unwrap_or(0);
    let manual: Vec<f64> = p.get("manualRankValues").and_then(|v| v.as_array()).into_iter().flatten().filter_map(num).collect();
    Some(Parameter {
        kind: int(at(p, &["parameterType", "value"])?)?,
        initial: number("initialBonus")?,
        per_rank: number("statBonusPerRank")?,
        multiplier_per_rank: number("initialBonusMultiplierPerRank")?,
        up_to_rank: mask("upgradeUpToRank"),
        manual: (p.get("useManualRankValues").is_some_and(data::truthy) && !manual.is_empty()).then_some(manual),
        weapon_categories: mask("weaponCategoryMask"),
        deployable_categories: mask("deployableCategoryMask"),
        entity_flags: mask("entityFlagMask"),
    })
}

fn enhancement(d: &Obj, enums: &Enums) -> Option<Enhancement> {
    let field = |name: &str| d.get(name).and_then(int);
    let mut bonuses = Vec::new();
    for bonus in d.get("bonuses")?.as_array()? {
        let raw_type = int(at(bonus, &["bonusType", "value"])?)?;
        let parameters = bonus.get("parameters")?.as_array()?.iter().map(parameter).collect::<Option<_>>()?;
        bonuses.push(EnhancementBonus { raw_type, kind: enums.bonus(raw_type), parameters });
    }
    Some(Enhancement {
        filter_flags: field("filterFlags")?,
        damage_types: field("damageTypes")?,
        weapon_categories: field("weaponCategories")?,
        weapon_flags: field("weaponFlags")?,
        no_weapon_flags: field("noWeaponFlags")?,
        exceptions: d.get("filterExceptions").and_then(|v| v.as_array()).into_iter().flatten()
            .filter_map(|e| Some(e.get("ID")?.as_str()?.to_string())).collect(),
        bonuses,
    })
}

/// Whether a definition is a player weapon, for when the game's own list (`GameRunner.weapons`, which the
/// reference uses) was not given. Among the dumped definitions the listed weapons are exactly those with gear
/// type side arm or primary weapon and a description text: enemy, turret and power weapons, and unfinished
/// variants, have no description.
fn is_player_weapon(d: &Obj, enums: &Enums) -> bool {
    let gear = d.get("gearType").and_then(|g| g.get("value")).and_then(int);
    gear.is_some_and(|g| enums.weapon_gear.contains(&g))
        && d.get("description").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty())
}

fn weapon(
    name: &str,
    d: &Obj,
    names: &HashMap<String, String>,
    tables: &HashMap<&str, Option<GearTable>>,
    enums: &Enums,
) -> Option<Weapon> {
    let display = d.get("title")?.as_str().and_then(|key| names.get(key)).map_or(name, String::as_str);
    let category = int(at(d.get("category")?, &["value"])?)?;
    let info = d.get("damageInfo")?;
    let damage_type = int(at(info, &["damageType", "value"])?)?;
    let pellets = if data::truthy(d.get("projectileMultiPellet")?) { int(d.get("projectilePelletCount")?)? } else { 1 };
    Some(Weapon {
        name: name.to_string(),
        info: WeaponInfo {
            display: display.to_string(),
            catname: enums.weapon_category(category).unwrap_or("").to_string(),
            dtype: usize::try_from(damage_type).unwrap_or(usize::MAX),
            dtype_name: enums.damage_type(damage_type).unwrap_or("").to_string(),
            pellets,
        },
        category,
        damage_type,
        flags: d.get("flags").and_then(|f| f.get("value")).and_then(int).unwrap_or(0),
        deployable_category: d.get("deployableCategory").and_then(|c| c.get("value")).and_then(int).unwrap_or(0),
        has_charge_effects: at(d.get("chargeEffects")?, &["m_List"]).and_then(|l| l.as_array()).is_some_and(|l| !l.is_empty()),
        damage: num(info.get("damage")?)?,
        // a crit chance is a probability, and a crit never lowers damage: the game clamps both when it reads them
        crit: py::min(py::max(num(info.get("critChance")?)?, 0.0), 1.0),
        crit_multiplier: py::max(1.0, num(info.get("critDamageMultiplier")?)?),
        time_between_shots: num(d.get("timeBetweenShots")?)?,
        magazine: int(d.get("magazineCapacity")?)?,
        reload: num(d.get("reloadTime")?)?,
        crafting: data::path_id(d.get("craftingTable")?).and_then(|id| tables.get(id.as_str())?.clone()),
        explosion: explosion(d, enums),
    })
}

impl Weapons {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        definitions: &Obj,
        gear_crafting: &Obj,
        mod_definitions: &Obj,
        mod_crafting: &Obj,
        enhancement_definitions: &Obj,
        names: &HashMap<String, String>,
        player_weapons: Option<&HashSet<String>>,
        enums: Enums,
        code: CodeValues,
    ) -> Weapons {
        let tables = data::gear_tables(gear_crafting);
        let weapons = data::by_name(definitions).into_iter()
            .filter(|(name, d)| match player_weapons {
                Some(listed) => listed.contains(*name),
                None => is_player_weapon(d, &enums),
            })
            .filter_map(|(name, d)| Some((name.to_string(), weapon(name, d, names, &tables, &enums)?)))
            .collect();
        let mods = data::by_name(mod_definitions).into_iter()
            .map(|(name, d)| Some((name.to_string(), mod_definition(d, &enums)?)))
            .collect();
        let crafting = data::by_name(mod_crafting).get("DefaultGearModCrafting").copied();
        let bands = [
            "critChancePercentPerPointPerQualityTier",
            "movementModifierPercentPerPointPerQualityTier",
            "reloadTimePercentPerPointPerQualityTier",
            "magazineCapacityPercentPerPointPerQualityTier",
            "damageResistancePercentPerPointPerQualityTier",
        ]
        .map(|field| {
            let rates = crafting?.get(field)?.get("values")?.as_array()?;
            Some(rates.iter().map(|r| num(r).unwrap_or(f64::NAN)).collect())
        });
        let enhancements = data::by_name(enhancement_definitions).into_iter()
            .filter_map(|(name, d)| Some((name.replace(ENHANCEMENT_PREFIX, ""), enhancement(d, &enums)?)))
            .collect();
        Weapons {
            weapons,
            mods,
            rules: crafting.and_then(point_rules),
            bands,
            enhancements,
            parameter: enums.parameter,
            filter: enums.filter,
            code,
            pistol: enums.pistol,
            last_quality: enums.last_quality,
            mod_slots: usize::try_from(enums.armour_mod_type).unwrap_or(0),
        }
    }

    /// How many upgrade steps a weapon can take; `None` for a weapon without a readable crafting table.
    pub fn upgrade_steps(&self, id: &str) -> Option<i64> {
        Some(self.weapons.get(id)?.crafting.as_ref()?.max_steps)
    }

    pub fn info(&self, id: &str) -> Option<WeaponInfo> {
        self.weapons.get(id).map(|w| w.info.clone())
    }

    /// Percent that `points` points of a normal mod's stat are worth, as tools/dps.py reads the band table: the
    /// i-th point counts the rate of its band. `None` when the table is missing or too short, where the
    /// reference fails.
    fn value(&self, stat: Stat, points: i64) -> Option<f64> {
        let rates = match stat {
            Stat::Crit => self.bands[0].as_ref()?,
            Stat::Movement => self.bands[1].as_ref()?,
            Stat::Reload => self.bands[2].as_ref()?,
            Stat::Magazine => self.bands[3].as_ref()?,
            Stat::Resistance => self.bands[4].as_ref()?,
            // one point of damage is one point of flat damage; accuracy has its own factor but is not on the sheet
            Stat::Damage | Stat::Accuracy | Stat::Other => return Some(points as f64),
        };
        let width = self.rules.as_ref()?.points_per_quality;
        let band = |i: i64| (i / width).min(self.last_quality) as usize;
        let count = points.saturating_abs();
        if count > 0 && rates.len() <= band(count - 1) {
            return None;
        }
        let total = py::sum((0..count).map(|i| rates[band(i)]));
        Some(if points >= 0 { total } else { -total })
    }

    /// `Gear.GearFitsEnhancementRequirements`: every requirement the enhancement's filter names must hold.
    fn fits(&self, e: &Enhancement, w: &Weapon) -> bool {
        let has = |flag: i64| e.filter_flags & flag != 0;
        let bit = |value: i64| if (0..63).contains(&value) { 1_i64 << value } else { 0 };
        e.filter_flags == 0
            || e.exceptions.iter().any(|id| *id == w.name)
            || ((!has(self.filter.damage_type) || e.damage_types & bit(w.damage_type) != 0)
                && (!has(self.filter.weapon_category) || e.weapon_categories & bit(w.category) != 0)
                && (!has(self.filter.weapon_flags) || e.weapon_flags & w.flags != 0)
                && (!has(self.filter.no_weapon_flags) || e.no_weapon_flags & w.flags == 0)
                && (!has(self.filter.charge_effects) || w.has_charge_effects)
                && (!has(self.filter.more_than_one_ammo) || w.magazine > 1))
    }

    /// `PassiveBonusParameter.GetBonus` at a rank, for a player level of 0 (an enhancement has none). `None`
    /// where the game runs off a list of per-rank values.
    fn parameter_value(&self, p: &Parameter, rank: i64) -> Option<f64> {
        if let Some(values) = &p.manual {
            let index = usize::try_from(rank.min(values.len() as i64) - 1).ok()?;
            return values.get(index).copied();
        }
        let rank = if p.up_to_rank != 0 { rank.min(p.up_to_rank) } else { rank };
        let mut initial = p.initial;
        if p.multiplier_per_rank > 0.0 {
            for _ in 1..rank {
                initial *= p.multiplier_per_rank;
            }
        }
        Some(p.per_rank * rank as f64 + initial)
    }

    /// The bonuses of an enhancement on a mod of a quality, as mod-stage features for `weapon` (for any weapon
    /// when `None`: category-bound damage bonuses are then left out). `None` when a parameter cannot be evaluated.
    fn enhancement_features(&self, e: &Enhancement, quality: i64, weapon: Option<&Weapon>) -> Option<Features> {
        // `BonusEvaluationContext.ForModEnhancement`: the per-rank part of every mod-point parameter, as whole
        // points per bonus type
        let mut extra_points: HashMap<i64, i64> = HashMap::new();
        for bonus in &e.bonuses {
            for p in bonus.parameters.iter().filter(|p| p.kind == self.parameter.mod_point) {
                *extra_points.entry(bonus.raw_type).or_default() += p.per_rank.floor() as i64 * quality;
            }
        }
        let mut features = [0.0; 11];
        for bonus in &e.bonuses {
            let Some(kind) = bonus.kind else { continue };
            // `PassiveBonusDefinition.TranslateModPointToRealBonus` without a mod crafting table (an enhancement
            // has none): mod points become a percent or flat parameter by bonus type; elsewhere they stay points
            let translated = |p: &Parameter| -> Option<(i64, f64)> {
                if p.kind != self.parameter.mod_point {
                    return Some((p.kind, self.parameter_value(p, quality)?));
                }
                let points = (py::round_even(p.initial)? + extra_points.get(&bonus.raw_type).copied().unwrap_or(0)) as f64;
                Some(match kind {
                    Bonus::ReloadTime => (self.parameter.multiplier, -(points * self.code.reload_per_point)),
                    Bonus::CritChance => (self.parameter.multiplier, points * self.code.crit_per_point),
                    Bonus::WeaponMagazineCapacity => (self.parameter.multiplier, points * self.code.magazine_per_point),
                    Bonus::ProjectileDamage => (self.parameter.generic, points),
                    _ => (p.kind, self.parameter_value(p, quality)?),
                })
            };
            let values: Vec<(i64, f64)> = bonus.parameters.iter().map(translated).collect::<Option<_>>()?;
            // the game reads either the first parameter or the first of a wanted type, 0 when there is none
            let first = values.first().map_or(0.0, |(_, v)| *v);
            let of = |wanted: i64| values.iter().find(|(kind, _)| *kind == wanted).map_or(0.0, |(_, v)| *v);
            let mask = |field: fn(&Parameter) -> i64| bonus.parameters.iter().fold(0, |m, p| m | field(p));
            // a bonus limited to kinds of target is kept on the player and applied per hit, not to the weapon
            let on_weapon = mask(|p| p.entity_flags) == 0;
            match kind {
                Bonus::ProjectileDamage => {
                    // `BonusHelpers.ApplyBonusToDamageInfo`: a damage bonus can be bound to weapon categories
                    // and to deployable categories
                    let (categories, deployables) = (mask(|p| p.weapon_categories), mask(|p| p.deployable_categories));
                    let bit = |value: i64| if (0..63).contains(&value) { 1_i64 << value } else { 0 };
                    let applies = match weapon {
                        Some(w) => {
                            (categories == 0 || categories & bit(w.category) != 0)
                                && if deployables != 0 { deployables & bit(w.deployable_category) != 0 } else { w.deployable_category == 0 }
                        }
                        None => categories == 0 && deployables == 0,
                    };
                    if applies {
                        features[DAMAGE] += of(self.parameter.multiplier) + of(self.parameter.damage_multiplier);
                        features[DAMAGE_FLAT] += of(self.parameter.generic);
                    }
                }
                Bonus::CritChance if on_weapon => features[CRIT] += first,
                Bonus::CritDamageMultiplier if on_weapon => features[CRIT_MULTIPLIER] += first / 100.0,
                Bonus::ReloadTime => features[RELOAD] += first / 100.0,
                Bonus::WeaponTimeBetweenShots => features[INTERVAL] += of(self.parameter.generic) / 100.0,
                Bonus::WeaponMagazineCapacity => {
                    features[MAGAZINE] += of(self.parameter.multiplier);
                    features[MAGAZINE_FLAT] += of(self.parameter.generic);
                }
                Bonus::ProjectileImpactDamage => features[IMPACT] += of(self.parameter.generic) / 100.0,
                Bonus::SharedPistolDamage => features[PISTOL_SHARED] += first,
                Bonus::SharedPistolStepUpDamage => features[PISTOL_STEP] += first,
                _ => {}
            }
        }
        Some(features)
    }

    /// What an enhancement adds to the sheet on a mod of a quality, on a weapon it fits.
    pub fn enhancement_effect(&self, name: &str, quality: u8) -> Option<EnhancementEffect> {
        let f = self.enhancement_features(self.enhancements.get(name)?, i64::from(quality), None)?;
        Some(EnhancementEffect {
            damage_percent: f[DAMAGE],
            damage_flat: f[DAMAGE_FLAT],
            crit_chance_percent: f[CRIT],
            crit_multiplier: f[CRIT_MULTIPLIER],
            time_between_shots: f[INTERVAL],
            magazine_percent: f[MAGAZINE],
            magazine_flat: f[MAGAZINE_FLAT],
            reload_time: f[RELOAD],
            impact_damage: f[IMPACT],
            pistol_damage: (f[PISTOL_SHARED] + f[PISTOL_STEP]) / 100.0,
        })
    }

    fn mod_features(&self, w: &Weapon, m: &ModSpec) -> Option<Features> {
        // a weapon has one slot per weapon mod type, and a mod one of the item qualities
        let quality = i64::from(m.quality);
        if m.slot >= self.mod_slots || quality > self.last_quality {
            return None;
        }
        let mut features = [0.0; 11];
        // a mod without a definition is a bare enhancement: no stats of its own
        if let Some(id) = m.id.as_deref().filter(|id| !id.is_empty()) {
            let definition = self.mods.as_ref()?.get(id)?;
            let points = definition.points(self.rules.as_ref()?, self.last_quality, quality, i64::from(m.offset), m.enh.is_some());
            for (stat, points) in points {
                // every stat is converted, also the ones the sheet does not use: a table too short for one of
                // them makes the reference fail
                let value = self.value(stat, points)?;
                match stat {
                    Stat::Damage => features[DAMAGE_FLAT] += value,
                    Stat::Crit => features[CRIT] += value,
                    Stat::Magazine => features[MAGAZINE] += value,
                    // positive reload points shorten the reload
                    Stat::Reload => features[RELOAD] -= value / 100.0,
                    _ => {}
                }
            }
        }
        if let Some(e) = m.enh.as_deref().and_then(|name| self.enhancements.get(name)).filter(|e| self.fits(e, w)) {
            for (total, value) in features.iter_mut().zip(self.enhancement_features(e, quality, Some(w))?) {
                *total += value;
            }
        }
        Some(features)
    }

    pub fn sheet(&self, id: &str, quality: u8, level: i32, mods: &[ModSpec]) -> Option<Sheet> {
        let w = self.weapons.get(id)?;
        let mut features = [0.0; 11];
        for m in mods {
            for (index, value) in self.mod_features(w, m)?.into_iter().enumerate() {
                // `GetWeaponSharedModEnhancementBonusTotals`: of the shared pistol bonuses only the largest counts
                if index == PISTOL_SHARED {
                    features[index] = py::max(features[index], value);
                } else {
                    features[index] += value;
                }
            }
        }
        let table = w.crafting.as_ref()?;
        let factor = table.quality_factor(i64::from(quality)).filter(|_| i64::from(quality) <= self.last_quality)?;
        if table.max_steps == 0 {
            return None;
        }
        // levels past the table's last step add nothing; the reference does not stop a negative level
        let level = i64::from(level).min(table.max_steps);
        let factor = factor * (1.0 + table.max_increase * level as f64 / table.max_steps as f64);

        // `BonusHelpers.ApplyBonusToDamageInfo`, which the game runs on the weapon's damage and on the explosion's
        // alike: the percent bonuses, then the flat ones, then the pistol bonus
        let pistol = if w.category == self.pistol { 1.0 + (features[PISTOL_SHARED] + features[PISTOL_STEP]) / 100.0 } else { 1.0 };
        let with_bonuses = |base: f64| (base * factor * (1.0 + features[DAMAGE] / 100.0) + features[DAMAGE_FLAT]) * pistol;
        // the impact bonus is the weapon's alone (`ApplyBonusToWeapon`), after the flat bonus
        let damage = with_bonuses(w.damage) * (1.0 + features[IMPACT]);
        let explosion = w.explosion.as_ref().map(|e| {
            if e.scales_with_projectile { damage * e.scale } else { with_bonuses(e.damage) }
        });
        // The game truncates the product in single precision. In doubles a magazine that should be whole can
        // land a hair below; the reference nudges it up by this much before truncating (its number, not the game's).
        let magazine = (w.magazine as f64 + features[MAGAZINE_FLAT]) * (1.0 + features[MAGAZINE] / 100.0) + 1e-6;
        if !magazine.is_finite() {
            return None;
        }
        Some(Sheet {
            dmg: damage,
            crit: w.crit + features[CRIT] / 100.0,
            critmult: py::max(1.0, w.crit_multiplier + features[CRIT_MULTIPLIER]),
            tbs: w.time_between_shots * (1.0 + features[INTERVAL]),
            // a weapon keeps at least one round
            mag: (magazine as i64).max(1),
            reload: w.reload * py::max(0.0, 1.0 + features[RELOAD]),
            explosion,
        })
    }
}
