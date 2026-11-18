//! The numbers of a loadout as the game's own screens show them, computed from the game's data files: what each
//! mod gives and how much power it uses, a weapon's sheet numbers with its mods, an armour piece's resistances.
//!
//! This follows the three Python tools the build viewer of the reference meter calls, for exactly the calls it
//! makes (`meter/zarmeter/build.py`, methods `mod`, `weapon`, `armour`):
//!
//! * [`Gear::mod_stats`]: `zmods.mod_stats(zmods.get_mod(id), quality, offset, has_enh, has_enh)` (mods.rs);
//! * [`Gear::weapon`] and [`Gear::weapon_sheet`]: `dps.weapons()[id]` and
//!   `dps.bake(w, quality, level, dps.sum_feat(mods), dps.Env(cls="none"))` after `dps.set_enh_points(True)`
//!   (weapons.rs), with the effect of enhancements derived from their definitions;
//! * [`Gear::armour_piece`] and [`Gear::armour_resistance`]: the pieces of `armour_opt.all_pieces()` and their
//!   `resistance(quality, level)` (armour.rs).
//!
//! No value of the game is written into this crate. Everything comes from what the caller hands over:
//!
//! * the ScriptableObject dumps of the classes in [`CLASSES`], each `{pathID: {"_name": asset name, fields}}`
//!   with a non-finite float written as `null`;
//! * the English localisation XML, for the display names of weapons and armour pieces (loc.rs);
//! * the game's enum tables `{EnumName: {Member: value}}` for the enums in [`ENUMS`] (enums.rs);
//! * the few factors that exist only in the game's code, by name ([`CODE_CONSTANTS`]);
//! * optionally the game's `GameRunner` component ([`GAME_RUNNER`]) and a way to resolve Addressables GUIDs, for
//!   the list of player weapons.
//!
//! Where the reference tools are the yardstick, results are the same doubles they produce: the arithmetic follows
//! them operation by operation (py.rs), and a query the reference fails on (unknown id, a quality or slot out of
//! range, a mod below the quality its definition starts at) gives `None`.

mod armour;
mod data;
mod enums;
mod loc;
mod mods;
mod py;
mod weapons;

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

/// Key in the `so` map of the game's `GameRunner` component, given as `{bundle file name: object}`. Its `weapons`
/// list (Addressables asset references) is the game's own list of player weapons. Optional: without it the
/// player weapons are recognised by a rule on their definitions (see weapons.rs).
pub const GAME_RUNNER: &str = "GameRunner";

/// Every ScriptableObject class [`Gear::new`] needs.
pub const CLASSES: &[&str] = &[
    "GearModDefinition",
    "GearModCrafting",
    "GearModEnhancementDefinitionSO",
    "WeaponDefinitionSO",
    "GearCrafting",
    "ArmourPieceSO",
];

/// The game enums whose member values the data files store and this crate interprets.
pub const ENUMS: &[&str] = &[
    "BonusType",
    "BonusParameterType",
    "DamageType",
    "WeaponCategory",
    "GearType",
    "ItemQuality",
    "GearModType",
    "GearModEnhancementFilterFlags",
    "ExplodeDataFlags",
];

/// Number of damage types: the length of a resistance list. A Rust array needs it at compile time;
/// [`Gear::new`] refuses a game whose `DamageType.COUNT` differs.
pub const DAMAGE_TYPES: usize = 6;

/// A number that exists only in the game's managed code, not in its data or metadata. The caller reads it from
/// `GameAssembly.dll` for the installed game version and passes it to [`Gear::new`] under `name`.
pub struct CodeConstant {
    /// Key in the `code_constants` object.
    pub name: &'static str,
    /// Managed type and method the number lives in.
    pub type_name: &'static str,
    pub method: &'static str,
    pub meaning: &'static str,
    /// How the number appears in the method's machine code.
    pub appearance: &'static str,
}

/// The numbers [`Gear::new`] takes from the game's code. All four are in one method, which turns a mod-point
/// parameter into a real bonus parameter: it returns its parameter unchanged unless the parameter's type is
/// `ModPoint`, otherwise gets the point count (`BonusEvaluationContext.GetFinalModPointsValue`, made a float with
/// `cvtdq2ps`) and branches on the bonus type, its second argument. Each banded bonus type has two paths there:
/// with a mod crafting table in the evaluation context (a normal mod) it calls
/// `GearModCrafting.SumPercentPerPointByQualityTier`, without one (an enhancement) it multiplies the point count
/// by a factor. Those factors, and the one for accuracy, which has no table, are the constants.
pub const CODE_CONSTANTS: &[CodeConstant] = &[
    CodeConstant {
        name: "accuracy_percent_per_mod_point",
        type_name: "PassiveBonusDefinition",
        method: "TranslateModPointToRealBonus",
        meaning: "Percent of accuracy (firing error reduction) one mod point of BonusType.WeaponAccuracy is worth, \
                  on normal mods and enhancements alike.",
        appearance: "Branch for bonus type WeaponAccuracy: one `mulss xmm0, dword ptr [rip+..]` on the point count, \
                     next to the write of BonusParameterType.Multiplier into the result's type byte; the float at \
                     that address.",
    },
    CodeConstant {
        name: "enhancement_reload_percent_per_mod_point",
        type_name: "PassiveBonusDefinition",
        method: "TranslateModPointToRealBonus",
        meaning: "Percent of reload time one mod point of BonusType.ReloadTime takes off when the evaluation context \
                  has no mod crafting table (enhancement parameters). The game flips the sign afterwards, so \
                  negative points lengthen the reload.",
        appearance: "Branch for bonus type ReloadTime, path taken when the context's crafting table is null: the \
                     point count is doubled with `addss xmm1, xmm1` (no immediate: the value is 2 while that \
                     instruction is there; a `mulss` with a float would take its place), then `xorps` with the \
                     sign mask.",
    },
    CodeConstant {
        name: "enhancement_crit_percent_per_mod_point",
        type_name: "PassiveBonusDefinition",
        method: "TranslateModPointToRealBonus",
        meaning: "Percent points of crit chance one mod point of BonusType.CritChance is worth when the evaluation \
                  context has no mod crafting table (enhancement parameters).",
        appearance: "Branch for bonus type CritChance, path taken when the context's crafting table is null: \
                     `mulss xmm1, dword ptr [rip+..]` on the point count; the float at that address.",
    },
    CodeConstant {
        name: "enhancement_magazine_percent_per_mod_point",
        type_name: "PassiveBonusDefinition",
        method: "TranslateModPointToRealBonus",
        meaning: "Percent of magazine capacity one mod point of BonusType.WeaponMagazineCapacity is worth when the \
                  evaluation context has no mod crafting table (enhancement parameters).",
        appearance: "Branch for bonus type WeaponMagazineCapacity, path taken when the context's crafting table is \
                     null: `mulss xmm1, dword ptr [rip+..]` on the point count; the float at that address.",
    },
];

/// A mod instance as its card shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct ModStats {
    /// Mod power: the capacity the mod uses. An integer in the game data and in the reference.
    pub power: i64,
    /// The mod's stats in the order of its definition.
    pub values: Vec<StatValue>,
}

/// One stat of a mod.
#[derive(Clone, Debug, PartialEq)]
pub struct StatValue {
    /// The reference tooling's short code (`Dmg`, `Crt`, `Acc`, `Rng`, `Rld`, `Mag`, `Mvp`, or `Phy`, `Hea`,
    /// `Cry`, `Ene`, `Cor` for a resistance). Ours, not the game's.
    pub code: String,
    /// `BonusType` member the stat is in the game (`ProjectileDamage`, `DamageResistance`, ..); the game's text
    /// for it is the localisation entry `BonusStat_<member>`.
    pub bonus: String,
    /// `DamageType` member of a resistance (`Equipment_Stat_DamageType_<member>` in the localisation).
    pub damage_type: Option<String>,
    /// True when `value` is a flat amount (a mod's Damage: that much damage on top of the weapon's), false when
    /// it is a percentage.
    pub flat: bool,
    /// Percent, or the flat amount when `flat`; negative for a negative stat. For `Rld` a positive value is that
    /// much shorter a reload.
    pub value: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WeaponInfo {
    /// Localised name, or the asset name when the title has no text.
    pub display: String,
    /// `WeaponCategory` member (`Pistol`, `SniperRifle`, ..; `Equipment_WeaponCategory_<member>` in the
    /// localisation); empty for a value the enum does not have.
    pub catname: String,
    /// `DamageType` value of the weapon's damage: its position in a resistance list.
    pub dtype: usize,
    /// `DamageType` member (`Equipment_Stat_DamageType_<member>` in the localisation); empty for an unknown value.
    pub dtype_name: String,
    /// Projectiles per shot (1 unless the weapon fires several pellets).
    pub pellets: i64,
}

/// A weapon's numbers at a quality and upgrade level with its mods, before class skills and buffs.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    /// Damage per projectile.
    pub dmg: f64,
    /// Crit chance as a fraction.
    pub crit: f64,
    pub critmult: f64,
    /// Seconds between shots.
    pub tbs: f64,
    pub mag: i64,
    /// Reload time in seconds.
    pub reload: f64,
    /// Damage of the explosion the projectile sets off, for a weapon that has one.
    pub explosion: Option<f64>,
}

/// What an enhancement adds to the mod stage of a weapon it fits, on a mod of one quality: the sum of its
/// bonuses that reach one of the sheet numbers. All zero for an enhancement that works through behaviours,
/// status effects or conditional stat modifiers.
#[derive(Clone, Debug, PartialEq)]
pub struct EnhancementEffect {
    /// Percent of damage, added to the other mods' percent.
    pub damage_percent: f64,
    /// Damage added after the percent.
    pub damage_flat: f64,
    /// Percent points of crit chance.
    pub crit_chance_percent: f64,
    /// Added to the crit multiplier.
    pub crit_multiplier: f64,
    /// Fraction added to the time between shots.
    pub time_between_shots: f64,
    pub magazine_percent: f64,
    pub magazine_flat: f64,
    /// Fraction added to the reload time.
    pub reload_time: f64,
    /// Fraction of damage, multiplied in after the others.
    pub impact_damage: f64,
    /// Fraction of damage on pistols.
    pub pistol_damage: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PieceInfo {
    /// Localised name, or the asset name with spaces for underscores when the title has no text.
    pub display: String,
    /// Movement reduction as a fraction of speed.
    pub mov: f64,
}

/// One mod on a weapon, as the reference builds `dps.Mod(slot, id, quality, enh, offset)`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModSpec {
    /// The mod's position on the weapon: a `GearModType` value below `Armour`.
    pub slot: usize,
    /// `GearModDefinition` asset name; `None` for an enhancement without a known host mod.
    pub id: Option<String>,
    pub quality: u8,
    /// Enhancement name without its `ModEnhancement_` prefix.
    pub enh: Option<String>,
    /// The power offset an upgrade left on the mod (0 down to -4).
    pub offset: i32,
}

impl ModSpec {
    /// A mod slot of a captured loadout (`id` and `enh` as stored there, empty when absent, `enh` with its
    /// `ModEnhancement_` prefix), converted the way the reference's build viewer does. `None` for an empty slot,
    /// which takes no part in the sheet.
    pub fn from_loadout(slot: usize, id: &str, quality: u8, enh: &str, offset: i32) -> Option<ModSpec> {
        if id.is_empty() && enh.is_empty() {
            return None;
        }
        let enh = enh.replace("ModEnhancement_", "");
        Some(ModSpec {
            slot,
            id: (!id.is_empty()).then(|| id.to_string()),
            quality,
            enh: (!enh.is_empty()).then_some(enh),
            offset,
        })
    }
}

/// The game data needed to answer for any mod, weapon and armour piece, decoded once.
pub struct Gear {
    mods: Option<mods::Table>,
    weapons: weapons::Weapons,
    armour: Option<HashMap<String, armour::Piece>>,
}

impl Gear {
    /// * `so`: class name -> content of that class's dump, for the classes of [`CLASSES`].
    /// * `loc_xml`: the content of the English localisation file.
    /// * `enums`: `{EnumName: {Member: value}}` with the enums of [`ENUMS`].
    /// * `code_constants`: `{name: number}` with the entries of [`CODE_CONSTANTS`].
    /// * `guid_to_path`: Addressables asset GUID -> path of the asset (the first location the catalog lists for
    ///   it). Only used when `so` holds [`GAME_RUNNER`].
    ///
    /// Fails, naming what is missing, when a class, an enum, an enum member or a code constant was not given.
    pub fn new(
        so: &HashMap<String, Map<String, Value>>,
        loc_xml: &str,
        enums: &Value,
        code_constants: &Value,
        guid_to_path: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Gear, String> {
        let class = |name: &str| so.get(name).ok_or_else(|| format!("the dump of class {name} was not given"));
        let (mod_definitions, mod_crafting) = (class("GearModDefinition")?, class("GearModCrafting")?);
        let (weapon_definitions, gear_crafting) = (class("WeaponDefinitionSO")?, class("GearCrafting")?);
        let (armour_pieces, enhancements) = (class("ArmourPieceSO")?, class("GearModEnhancementDefinitionSO")?);
        let enums = enums::Enums::read(enums)?;
        let code = enums::CodeValues::read(code_constants)?;
        let player_weapons = match so.get(GAME_RUNNER) {
            Some(runner) => Some(runner_weapons(runner, guid_to_path).ok_or("the GameRunner object has no weapons list")?),
            None => None,
        };
        Ok(Gear {
            mods: mods::Table::build(mod_definitions, mod_crafting, &enums, &code),
            armour: armour::pieces(armour_pieces, gear_crafting, &loc::armour_names(loc_xml), enums.last_quality),
            weapons: weapons::Weapons::build(
                weapon_definitions,
                gear_crafting,
                mod_definitions,
                mod_crafting,
                enhancements,
                &loc::weapon_names(loc_xml),
                player_weapons.as_ref(),
                enums,
                code,
            ),
        })
    }

    /// Power and stat values of a mod at a quality (an `ItemQuality` value) with a power offset, with or
    /// without an enhancement on it. An enhanced mod gets the enhancement's extra power and the stat points
    /// that power buys. `id` may differ from the asset name in case.
    pub fn mod_stats(&self, id: &str, quality: u8, offset: i32, has_enh: bool) -> Option<ModStats> {
        self.mods.as_ref()?.stats(id, quality, offset, has_enh)
    }

    /// A player weapon by asset name.
    pub fn weapon(&self, id: &str) -> Option<WeaponInfo> {
        self.weapons.info(id)
    }

    /// The sheet numbers of a player weapon at a quality and upgrade level with the given mods.
    pub fn weapon_sheet(&self, id: &str, quality: u8, level: i32, mods: &[ModSpec]) -> Option<Sheet> {
        self.weapons.sheet(id, quality, level, mods)
    }

    /// What the enhancement `name` (without its `ModEnhancement_` prefix) on a mod of `quality` adds to the
    /// sheet of a weapon it fits.
    pub fn enhancement_effect(&self, name: &str, quality: u8) -> Option<EnhancementEffect> {
        self.weapons.enhancement_effect(name, quality)
    }

    /// An armour piece by asset name.
    pub fn armour_piece(&self, name: &str) -> Option<PieceInfo> {
        Some(self.armour.as_ref()?.get(name)?.info())
    }

    /// Resistance in percent of a piece at a quality and upgrade level, one entry per `DamageType` value.
    pub fn armour_resistance(&self, name: &str, quality: u8, level: i32) -> Option<[f64; DAMAGE_TYPES]> {
        self.armour.as_ref()?.get(name)?.resistance(quality, level)
    }
}

/// Asset names of the weapons `GameRunner.weapons` refers to. An asset is named after its file: the last path
/// component without its extension.
fn runner_weapons(runner: &Map<String, Value>, guid_to_path: &dyn Fn(&str) -> Option<String>) -> Option<HashSet<String>> {
    let references = runner.values().next()?.get("weapons")?.as_array()?;
    Some(references.iter().filter_map(|reference| {
        let path = guid_to_path(reference.get("m_AssetGUID")?.as_str()?)?;
        let file = path.rsplit(['/', '\\']).next()?;
        Some(match file.rfind('.') {
            Some(dot) if dot > 0 => &file[..dot],
            _ => file,
        }.to_string())
    }).collect())
}
