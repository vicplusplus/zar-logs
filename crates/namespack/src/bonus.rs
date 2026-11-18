//! What a bonus parameter is worth at a skill rank or a player level. The formulas are the game's own, read from
//! its code by the reference tooling (`PassiveBonusParameter::GetBonus` and `AbilityBonusParameter::GetBonus` in
//! GameAssembly.dll); the inputs are the parameter's fields as dumped.

use serde_json::Value;

use crate::truthy;

fn field(parameter: &Value, name: &str) -> f64 { parameter.get(name).and_then(Value::as_f64).unwrap_or(0.0) }

/// A passive skill's parameter at `rank`, for a player at `level`.
///
/// A table of `manualRankValues`, when switched on, is read directly (ranks past its end take the last entry).
/// Otherwise `(level * perLevelPerRank + perRank) * r + initial`, where `r` is the rank, capped by
/// `upgradeUpToRank` if that is set, and `initial` is multiplied by `initialBonusMultiplierPerRank` for every rank
/// after the first when that multiplier is positive.
pub(crate) fn at_rank(parameter: &Value, rank: i64, level: i64) -> f64 {
    let manual = parameter.get("manualRankValues").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    if parameter.get("useManualRankValues").is_some_and(truthy) && !manual.is_empty() {
        let last = manual.len() as i64;
        // rank 0 would read the last entry in the reference too (it indexes from the end)
        let at = (rank.min(last) - 1).rem_euclid(last) as usize;
        return manual[at].as_f64().unwrap_or(f64::NAN);
    }
    let mut r = rank as f64;
    if parameter.get("upgradeUpToRank").is_some_and(truthy) {
        r = r.min(field(parameter, "upgradeUpToRank"));
    }
    let mut initial = field(parameter, "initialBonus");
    let multiplier = field(parameter, "initialBonusMultiplierPerRank");
    if multiplier > 0.0 {
        initial *= multiplier.powf(r - 1.0);
    }
    (level as f64 * field(parameter, "statBonusPerPlayerLevelPerRank") + field(parameter, "statBonusPerRank")) * r + initial
}

/// An ability's parameter for a player at `level`: the sum over its `bonusLevelRanges` that have started
/// (`level >= minLevel`) of `initialBonus + steps * statBonusPerLevelIncrement`, one step per
/// `levelIncrementPerStatBonus` levels above `minLevel`, counted up to `maxLevel` where there is one.
pub(crate) fn at_level(parameter: &Value, level: i64) -> f64 {
    let mut total = 0.0;
    for range in parameter.get("bonusLevelRanges").and_then(Value::as_array).into_iter().flatten() {
        let (increment, min_level) = (field(range, "levelIncrementPerStatBonus"), field(range, "minLevel"));
        if (level as f64) < min_level || increment <= 0.0 {
            continue;
        }
        let cap = match range.get("maxLevel").filter(|v| truthy(v)) {
            Some(max_level) => ((max_level.as_f64().unwrap_or(0.0) - min_level) / increment).floor(),
            None => f64::INFINITY,
        };
        let steps = ((level as f64 - min_level) / increment).floor().min(cap);
        total += field(range, "initialBonus") + steps * field(range, "statBonusPerLevelIncrement");
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ranks() {
        let table = json!({ "useManualRankValues": 1, "manualRankValues": [4, 6, 8.5] });
        assert_eq!([at_rank(&table, 1, 60), at_rank(&table, 3, 60), at_rank(&table, 5, 60)], [4.0, 8.5, 8.5]);
        let linear = json!({ "initialBonus": 2.0, "statBonusPerRank": 1.5, "statBonusPerPlayerLevelPerRank": 0.1, "upgradeUpToRank": 2 });
        assert_eq!([at_rank(&linear, 1, 60), at_rank(&linear, 3, 60)], [9.5, 17.0]);
        let growing = json!({ "initialBonus": 10.0, "initialBonusMultiplierPerRank": 2.0, "manualRankValues": [1] });
        assert_eq!(at_rank(&growing, 3, 60), 40.0);
    }

    #[test]
    fn levels() {
        let p = json!({ "bonusLevelRanges": [
            { "minLevel": 1, "maxLevel": 30, "levelIncrementPerStatBonus": 5, "initialBonus": 100.0, "statBonusPerLevelIncrement": 10.0 },
            { "minLevel": 40, "levelIncrementPerStatBonus": 1, "statBonusPerLevelIncrement": 0.5 },
            { "minLevel": 1, "levelIncrementPerStatBonus": 0, "initialBonus": 1000.0 },
        ] });
        assert_eq!([at_level(&p, 1), at_level(&p, 12), at_level(&p, 60)], [100.0, 120.0, 160.0]);
    }
}
