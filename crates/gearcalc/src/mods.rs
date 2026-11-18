//! A mod as the game's mod card shows it: its power (the capacity it uses) and the percentage each stat gives.
//! Port of `get_mod` and `mod_stats` of tools/zmods.py.
//!
//! From `GearModDefinition` (one asset per normal mod): the slot type, the quality the mod starts at, a baseline
//! power and the stats, each a bonus type (plus a damage type for a resistance) with a number of points.
//! From `GearModCrafting` (asset `DefaultGearModCrafting`): power per point, points gained per quality step, the
//! allowed range of the power offset an upgrade can leave behind, the extra power of an enhancement, and for each
//! banded stat the percent one point is worth in each band of points.
//!
//! A mod instance is a definition at a quality with a power offset and possibly an enhancement. Its power above
//! the definition's own is paid out as extra points, one at a time over the positive stats, largest first; an
//! enhancement's power counts towards those points too (confirmed in game against a mod card). A point of damage
//! or range is worth 1, a point of accuracy a factor that is in the game's code (a `CODE_CONSTANTS` input), and the
//! other stats go band by band through the crafting table. The number of bands and the quality range are those of
//! the `ItemQuality` enum.
//!
//! The stat codes in the result (`Dmg`, `Crt`, .. and the first three letters of a damage type for a resistance)
//! are the reference tooling's names, not the game's.

use std::collections::HashMap;

use crate::data::{self, at, int, num, Obj};
use crate::enums::{Bonus, CodeValues, Enums};
use crate::{py, ModStats, StatValue};

/// The `GearModCrafting` table holding the bands of a stat (tools/zmods.py `BANDED`).
const BAND_FIELDS: [(&str, &[&str]); 5] = [
    ("critChancePercentPerPointPerQualityTier", &["Crt"]),
    ("movementModifierPercentPerPointPerQualityTier", &["Mvp"]),
    ("reloadTimePercentPerPointPerQualityTier", &["Rld"]),
    ("magazineCapacityPercentPerPointPerQualityTier", &["Mag"]),
    ("damageResistancePercentPerPointPerQualityTier", &["Phy", "Hea", "Cry", "Ene", "Cor"]),
];

const RESISTANCE_CODES: &[&str] = BAND_FIELDS[4].1;

/// The reference's code of a resistance: the first three letters of the damage type (`Phy`, `Hea`, ..). `None`
/// for a damage type no mod resists today (Void), where the reference fails as well.
fn resistance_code(damage_type: &str) -> Option<&'static str> {
    RESISTANCE_CODES.iter().copied().find(|code| damage_type.starts_with(code))
}

struct Definition {
    name: String,
    type_id: i64,
    base_quality: i64,
    baseline_power: i64,
    /// (stat code, points) in the definition's order; a negative stat has negative points.
    stats: Vec<(&'static str, i64)>,
    /// What each code stands for in the game: (code, `BonusType` member, `DamageType` member of a resistance).
    labels: Vec<(&'static str, &'static str, Option<String>)>,
}

struct Crafting {
    points_per_quality: i64,
    power_per_point: i64,
    power_per_penalty_point: i64,
    offset_min: i64,
    offset_max: i64,
    /// Mod quality -> extra power of an enhancement on a mod of that quality.
    enhancement_power: Option<Vec<(i64, i64)>>,
    /// Percent per point in each band (one per item quality), per table of `BAND_FIELDS`.
    bands: [Option<Vec<f64>>; 5],
    /// `ItemQuality.Last`.
    last_quality: i64,
    /// Percent of accuracy per point, from the game's code.
    accuracy_per_point: f64,
}

pub(crate) struct Table {
    definitions: Vec<Definition>,
    exact: HashMap<String, usize>,
    lower: HashMap<String, usize>,
    crafting: Option<Crafting>,
    last_quality: i64,
}

fn definition(name: &str, d: &Obj, enums: &Enums) -> Option<Definition> {
    let (mut stats, mut labels) = (Vec::new(), Vec::new());
    for stat in d.get("bonusStats")?.as_array()? {
        let parameters = stat.get("parameters")?.as_array()?;
        let [parameter] = parameters.as_slice() else { return None };
        if int(at(parameter, &["parameterType", "value"])?)? != enums.parameter.mod_point {
            return None;
        }
        let kind = enums.bonus(int(at(stat, &["bonusType", "value"])?)?)?;
        let mut damage_type = None;
        let code = match kind {
            Bonus::ProjectileDamage => "Dmg",
            Bonus::CritChance => "Crt",
            Bonus::WeaponAccuracy => "Acc",
            Bonus::WeaponRange => "Rng",
            Bonus::ReloadTime => "Rld",
            Bonus::WeaponMagazineCapacity => "Mag",
            Bonus::ReducedMovementPenalty => "Mvp",
            Bonus::DamageResistance => {
                let name = enums.damage_type(int(stat.get("specificDamageType")?)?)?;
                damage_type = Some(name.to_string());
                resistance_code(name)?
            }
            // bonus types a normal mod does not carry; the reference fails on them
            _ => return None,
        };
        stats.push((code, py::round_even(num(parameter.get("initialBonus")?)?)?));
        if !labels.iter().any(|(c, _, _)| *c == code) {
            labels.push((code, crate::enums::bonus_name(kind), damage_type));
        }
    }
    Some(Definition {
        name: name.to_string(),
        type_id: int(d.get("type")?)?,
        base_quality: int(d.get("minimumUnlockQuality")?)?,
        baseline_power: int(d.get("baselinePower")?)?,
        stats,
        labels,
    })
}

fn crafting(c: &Obj, enums: &Enums, code: &CodeValues) -> Option<Crafting> {
    let field = |name: &str| c.get(name).and_then(int);
    let bands = |name: &str| -> Option<Vec<f64>> {
        let entries = data::int_dict(c.get(name)?)?;
        // the table is serialised from C# floats; the reference takes the float's value and tidies its tail
        (0..=enums.last_quality)
            .map(|band| Some(py::round_to(py::f32_of(num(data::lookup(&entries, band)?)?)?, 6)))
            .collect()
    };
    Some(Crafting {
        points_per_quality: field("improvementPointsPerQuality")?,
        power_per_point: field("powerPerImprovementPoint")?,
        power_per_penalty_point: field("powerPerPenaltyPoint")?,
        offset_min: field("powerLevelOffsetMinimumValue")?,
        offset_max: field("powerLevelOffsetMaximumValue")?,
        enhancement_power: c.get("enhancementPowerOffsets").and_then(data::int_dict)
            .map(|entries| entries.into_iter().filter_map(|(quality, power)| Some((quality, int(power)?))).collect()),
        bands: BAND_FIELDS.map(|(name, _)| bands(name)),
        last_quality: enums.last_quality,
        accuracy_per_point: code.accuracy_per_point,
    })
}

impl Table {
    /// `None` when any definition is not a plain mod the reference can decode: it reads all of them before it
    /// answers for one, so a single odd definition leaves it without any.
    pub fn build(definitions: &Obj, crafting_class: &Obj, enums: &Enums, code: &CodeValues) -> Option<Table> {
        let mut list = Vec::new();
        for (name, d) in data::by_name(definitions) {
            list.push(definition(name, d, enums)?);
        }
        // the reference's order; it decides which definition a name differing only in case resolves to
        list.sort_by(|a, b| (a.type_id, a.base_quality, &a.name).cmp(&(b.type_id, b.base_quality, &b.name)));
        let exact = list.iter().enumerate().map(|(i, d)| (d.name.clone(), i)).collect();
        let lower = list.iter().enumerate().map(|(i, d)| (d.name.to_lowercase(), i)).collect();
        let crafting = data::by_name(crafting_class).get("DefaultGearModCrafting").and_then(|c| crafting(c, enums, code));
        Some(Table { definitions: list, exact, lower, crafting, last_quality: enums.last_quality })
    }

    fn get(&self, id: &str) -> Option<&Definition> {
        let index = self.exact.get(id).or_else(|| self.lower.get(&id.to_lowercase()))?;
        Some(&self.definitions[*index])
    }

    pub fn stats(&self, id: &str, quality: u8, offset: i32, has_enh: bool) -> Option<ModStats> {
        let def = self.get(id)?;
        let quality = i64::from(quality);
        // a mod cannot be below the quality its definition starts at
        if quality > self.last_quality || quality < def.base_quality {
            return None;
        }
        let c = self.crafting.as_ref()?;

        let positive: i64 = def.stats.iter().filter(|(_, p)| *p > 0).map(|(_, p)| p).sum();
        let negative: i64 = def.stats.iter().filter(|(_, p)| *p < 0).map(|(_, p)| -p).sum();
        let base_power =
            (def.baseline_power + c.power_per_point * positive + c.power_per_penalty_point * negative).max(0);
        let steps = (quality - def.base_quality).clamp(0, self.last_quality);
        let offset = c.offset_min.max(i64::from(offset).min(c.offset_max));
        let mut power = base_power + (c.points_per_quality * steps + offset) * c.power_per_point;
        if has_enh {
            let by_quality = c.enhancement_power.as_ref()?;
            power += by_quality.iter().rev().find(|(q, _)| *q == quality).map_or(0, |(_, p)| *p);
        }

        // Points per stat code. The reference keys them by code, so a code listed twice is one entry holding
        // the later points.
        let mut points: Vec<(&'static str, i64)> = Vec::new();
        for (code, p) in &def.stats {
            match points.iter_mut().find(|(c, _)| c == code) {
                Some(entry) => entry.1 = *p,
                None => points.push((code, *p)),
            }
        }
        let extra = py::floor_div(power - base_power, c.power_per_point)?;
        let mut order: Vec<(&str, i64)> = def.stats.iter().filter(|(_, p)| *p > 0).copied().collect();
        order.sort_by_key(|(_, p)| -p);
        if !order.is_empty() {
            for i in 0..extra.max(0) as usize {
                let code = order[i % order.len()].0;
                points.iter_mut().find(|(c, _)| *c == code)?.1 += 1;
            }
        }

        let mut values = Vec::with_capacity(points.len());
        for (code, p) in points {
            let (_, bonus, damage_type) = def.labels.iter().find(|(c, _, _)| *c == code)?;
            values.push(StatValue {
                code: code.to_string(),
                bonus: bonus.to_string(),
                damage_type: damage_type.clone(),
                flat: *bonus == "ProjectileDamage",
                value: c.value(code, p)?,
            });
        }
        Some(ModStats { power, values })
    }
}

impl Crafting {
    /// What `points` points of a stat give: flat damage for Damage, percent for the others; the sign follows
    /// the points.
    fn value(&self, code: &str, points: i64) -> Option<f64> {
        let (count, sign) = (points.saturating_abs(), if points < 0 { -1.0 } else { 1.0 });
        match code {
            "Dmg" | "Rng" => return Some(sign * count as f64),
            "Acc" => return Some(sign * count as f64 * self.accuracy_per_point),
            _ => {}
        }
        let table = BAND_FIELDS.iter().position(|(_, codes)| codes.contains(&code))?;
        let rates = self.bands[table].as_ref()?;
        if self.points_per_quality <= 0 {
            return None;
        }
        // the i-th point of a stat is worth the rate of the band it falls in; bands are as wide as a quality step
        let total = py::sum((0..count).map(|i| rates[(i / self.points_per_quality).min(self.last_quality) as usize]));
        Some(sign * py::round_to(total, 6))
    }
}
