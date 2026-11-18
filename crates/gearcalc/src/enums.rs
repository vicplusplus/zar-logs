//! The game enums whose values the data files store, and the numbers that exist only in the game's code.
//!
//! Enum values come from the tables the app derives from the game's metadata (`{EnumName: {Member: value}}`, the
//! enums of `ENUMS` in lib.rs). Nothing is assumed about a value: a missing enum or member is an error, so a
//! game update that renames one is noticed instead of being papered over.
//!
//! A few conversion factors are neither in the ScriptableObjects nor in the metadata: they are float immediates
//! in a managed method. They are an input too (`code_constants`, described by `CODE_CONSTANTS` in lib.rs), read
//! from `GameAssembly.dll` by whoever calls this crate.

use std::collections::HashMap;

use serde_json::Value;

use crate::CODE_CONSTANTS;

/// The bonus types that change a number this crate reports, by `BonusType` member.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Bonus {
    ProjectileDamage,
    CritChance,
    CritDamageMultiplier,
    WeaponAccuracy,
    WeaponRange,
    ReloadTime,
    WeaponTimeBetweenShots,
    WeaponMagazineCapacity,
    ReducedMovementPenalty,
    DamageResistance,
    ProjectileImpactDamage,
    SharedPistolDamage,
    SharedPistolStepUpDamage,
}

const BONUS_MEMBERS: [(Bonus, &str); 13] = [
    (Bonus::ProjectileDamage, "ProjectileDamage"),
    (Bonus::CritChance, "CritChance"),
    (Bonus::CritDamageMultiplier, "CritDamageMultiplier"),
    (Bonus::WeaponAccuracy, "WeaponAccuracy"),
    (Bonus::WeaponRange, "WeaponRange"),
    (Bonus::ReloadTime, "ReloadTime"),
    (Bonus::WeaponTimeBetweenShots, "WeaponTimeBetweenShots"),
    (Bonus::WeaponMagazineCapacity, "WeaponMagazineCapacity"),
    (Bonus::ReducedMovementPenalty, "ReducedMovementPenalty"),
    (Bonus::DamageResistance, "DamageResistance"),
    (Bonus::ProjectileImpactDamage, "ProjectileImpactDamage"),
    (Bonus::SharedPistolDamage, "SharedPistolDamage"),
    (Bonus::SharedPistolStepUpDamage, "SharedPistolStepUpDamage"),
];

/// The `BonusType` member a kind is.
pub(crate) fn bonus_name(kind: Bonus) -> &'static str {
    BONUS_MEMBERS.iter().find(|(k, _)| *k == kind).map_or("", |(_, name)| name)
}

/// `BonusParameterType` members: which part of a bonus a parameter's number is.
pub(crate) struct ParameterTypes {
    /// The flat part of a bonus.
    pub generic: i64,
    /// The percent part of a bonus.
    pub multiplier: i64,
    /// A second percent part that projectile damage also counts.
    pub damage_multiplier: i64,
    /// A number of mod points, converted by bonus type before use.
    pub mod_point: i64,
}

/// `GearModEnhancementFilterFlags` members: what a weapon must be for an enhancement to work on it.
pub(crate) struct FilterFlags {
    pub damage_type: i64,
    pub weapon_category: i64,
    pub weapon_flags: i64,
    pub no_weapon_flags: i64,
    pub charge_effects: i64,
    pub more_than_one_ammo: i64,
}

pub(crate) struct Enums {
    bonus: HashMap<i64, Bonus>,
    pub parameter: ParameterTypes,
    pub filter: FilterFlags,
    /// `DamageType` members by value, without the member that only counts them.
    damage_types: HashMap<i64, String>,
    weapon_categories: HashMap<i64, String>,
    pub pistol: i64,
    /// `GearType.SideArm` and `GearType.PrimaryWeapon`: the two weapon slots of a loadout.
    pub weapon_gear: [i64; 2],
    /// `ItemQuality.Last`: the highest quality, and with it the number of point bands of a mod stat.
    pub last_quality: i64,
    /// `GearModType.Armour`: the weapon mod slots are the types below it.
    pub armour_mod_type: i64,
    /// `ExplodeDataFlags.ScaleWithProjectileDamage`: the explosion deals the projectile's damage times a factor.
    pub explosion_scales_with_projectile: i64,
}

impl Enums {
    pub fn read(tables: &Value) -> Result<Enums, String> {
        let table = |name: &str| {
            tables.get(name).and_then(Value::as_object).ok_or_else(|| format!("the enum {name} was not given"))
        };
        let member = |name: &str, member: &str| {
            table(name)?.get(member).and_then(Value::as_i64).ok_or_else(|| format!("the enum {name} has no member {member}"))
        };
        let names = |name: &str| -> Result<HashMap<i64, String>, String> {
            Ok(table(name)?.iter().filter_map(|(member, value)| Some((value.as_i64()?, member.clone()))).collect())
        };
        let mut bonus = HashMap::new();
        for (kind, name) in BONUS_MEMBERS {
            bonus.insert(member("BonusType", name)?, kind);
        }
        // COUNT is how many damage types there are, not one of them; every per-type list here has that length
        let count = member("DamageType", "COUNT")?;
        if count != crate::DAMAGE_TYPES as i64 {
            return Err(format!("the game has {count} damage types, this crate's resistance list holds {}", crate::DAMAGE_TYPES));
        }
        let mut damage_types = names("DamageType")?;
        damage_types.retain(|value, name| name != "COUNT" && (0..count).contains(value));
        Ok(Enums {
            bonus,
            parameter: ParameterTypes {
                generic: member("BonusParameterType", "Generic")?,
                multiplier: member("BonusParameterType", "Multiplier")?,
                damage_multiplier: member("BonusParameterType", "DamageMultiplier")?,
                mod_point: member("BonusParameterType", "ModPoint")?,
            },
            filter: FilterFlags {
                damage_type: member("GearModEnhancementFilterFlags", "RequiresDamageType")?,
                weapon_category: member("GearModEnhancementFilterFlags", "RequiresWeaponCategory")?,
                weapon_flags: member("GearModEnhancementFilterFlags", "RequiresWeaponFlags")?,
                no_weapon_flags: member("GearModEnhancementFilterFlags", "RequiresNoWeaponFlags")?,
                charge_effects: member("GearModEnhancementFilterFlags", "RequiresChargeEffects")?,
                more_than_one_ammo: member("GearModEnhancementFilterFlags", "RequiresWeaponWithMoreThan1Ammo")?,
            },
            damage_types,
            weapon_categories: names("WeaponCategory")?,
            pistol: member("WeaponCategory", "Pistol")?,
            weapon_gear: [member("GearType", "SideArm")?, member("GearType", "PrimaryWeapon")?],
            last_quality: member("ItemQuality", "Last")?,
            armour_mod_type: member("GearModType", "Armour")?,
            explosion_scales_with_projectile: member("ExplodeDataFlags", "ScaleWithProjectileDamage")?,
        })
    }

    pub fn bonus(&self, value: i64) -> Option<Bonus> {
        self.bonus.get(&value).copied()
    }

    /// Member name of a damage type (`Physical`, `Heat`, ..).
    pub fn damage_type(&self, value: i64) -> Option<&str> {
        self.damage_types.get(&value).map(String::as_str)
    }

    /// Member name of a weapon category (`Pistol`, `SniperRifle`, ..).
    pub fn weapon_category(&self, value: i64) -> Option<&str> {
        self.weapon_categories.get(&value).map(String::as_str)
    }
}

/// The values of `CODE_CONSTANTS`, in that order.
pub(crate) struct CodeValues {
    pub accuracy_per_point: f64,
    pub reload_per_point: f64,
    pub crit_per_point: f64,
    pub magazine_per_point: f64,
}

impl CodeValues {
    pub fn read(constants: &Value) -> Result<CodeValues, String> {
        let mut values = [0.0; 4];
        for (value, constant) in values.iter_mut().zip(CODE_CONSTANTS) {
            *value = constants.get(constant.name).and_then(Value::as_f64)
                .ok_or_else(|| format!("the code constant {} was not given", constant.name))?;
        }
        let [accuracy_per_point, reload_per_point, crit_per_point, magazine_per_point] = values;
        Ok(CodeValues { accuracy_per_point, reload_per_point, crit_per_point, magazine_per_point })
    }
}
