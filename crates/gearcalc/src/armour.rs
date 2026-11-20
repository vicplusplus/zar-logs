//! An armour piece as the loadout screen shows it: name, movement penalty and the resistance it gives against
//! each damage type at a quality and upgrade level. Port of `all_pieces` and `Piece.resistance` of
//! tools/armour_opt.py (the optimiser built on top of them is not part of this crate).
//!
//! From `ArmourPieceSO`: the defence rating per damage type, the movement reduction, the title key and the
//! reference to the piece's crafting table.
//! From `GearCrafting` (through that reference, or the asset `DefaultArmourCrafting` for a piece without one):
//! the rating factor of each quality and the gain per upgrade level.
//!
//! The game turns a rating into a resistance with a square root: a piece's percent against a damage type is the
//! root of its scaled rating, keeping the rating's sign.

use std::collections::HashMap;

use crate::data::{self, int, num, GearTable, Obj};
use crate::{py, PieceInfo, DAMAGE_TYPES};

pub(crate) struct Piece {
    info: PieceInfo,
    /// Defence rating per damage type at Common quality without upgrades.
    ratings: [f64; DAMAGE_TYPES],
    table: GearTable,
    /// `ItemQuality.Last`.
    last_quality: i64,
}

fn piece(
    name: &str,
    a: &Obj,
    names: &HashMap<String, String>,
    tables: &HashMap<&str, Option<GearTable>>,
    default_table: Option<&GearTable>,
    last_quality: i64,
) -> Option<Piece> {
    let table = match data::path_id(a.get("craftingTable")?).and_then(|id| tables.get(id.as_str())) {
        Some(table) => table.as_ref()?,
        None => default_table?,
    };
    let mut ratings = [0.0; DAMAGE_TYPES];
    for rating in a.get("defenceRatings")?.as_array()? {
        // a rating is filed under its DamageType value
        let slot = usize::try_from(int(rating.get("damageType")?)?).ok()?;
        *ratings.get_mut(slot)? += num(rating.get("defenceRating")?)?;
    }
    let title = a.get("title")?.as_str().unwrap_or("");
    let display = match names.get(title) {
        Some(text) if !text.is_empty() => text.clone(),
        _ => name.replace('_', " "),
    };
    // the field is a C# float; the reference takes the float's value and tidies its tail
    let mov = py::round_to(py::f32_of(num(a.get("movement_reduction")?)?)?, 7);
    Some(Piece { info: PieceInfo { display, mov }, ratings, table: table.clone(), last_quality })
}

/// Every armour piece by asset name. `None` when one of them cannot be decoded: the reference reads all pieces
/// before it answers for one, so a single odd piece leaves it without any.
pub(crate) fn pieces(
    class: &Obj,
    gear_crafting: &Obj,
    names: &HashMap<String, String>,
    last_quality: i64,
) -> Option<HashMap<String, Piece>> {
    let tables = data::gear_tables(gear_crafting);
    let default_table = data::by_name(gear_crafting).get("DefaultArmourCrafting").and_then(|t| GearTable::parse(t));
    data::by_name(class).into_iter()
        .map(|(name, a)| Some((name.to_string(), piece(name, a, names, &tables, default_table.as_ref(), last_quality)?)))
        .collect()
}

impl Piece {
    /// How many upgrade steps the piece can take.
    pub fn upgrade_steps(&self) -> i64 { self.table.max_steps }

    pub fn info(&self) -> PieceInfo {
        self.info.clone()
    }

    /// Percent per damage type, indexed by `DamageType` value.
    pub fn resistance(&self, quality: u8, level: i32) -> Option<[f64; DAMAGE_TYPES]> {
        if i64::from(quality) > self.last_quality || self.table.max_steps == 0 {
            return None;
        }
        // a quality the table does not list counts as factor 1; levels outside the table's steps are clamped
        let quality_factor = self.table.quality_factor(i64::from(quality)).unwrap_or(1.0);
        let level = i64::from(level).min(self.table.max_steps).max(0);
        let factor = quality_factor * (1.0 + self.table.max_increase * level as f64 / self.table.max_steps as f64);
        Some(self.ratings.map(|rating| {
            let scaled = rating * factor;
            scaled.abs().sqrt().copysign(scaled)
        }))
    }
}
