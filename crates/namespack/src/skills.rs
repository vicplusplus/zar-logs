//! The class-skills table: one row for every class ability, ability upgrade, ability enhancement and passive
//! skill of the game, with what the build viewer shows about it. The names pack's `skills` section is the same
//! rows reduced to id, name, kind and the status-effect sets their data mentions.
//!
//! Where the rows come from:
//!
//! * `PlayerClassDefinitionSO`: every base class, each followed by its `subclasses` in the order it lists them.
//!   A base class is one that no class lists as a subclass. Their order is the game's own where it is known: the
//!   `playerClassDefinitions` list of the game's main object (`GameRunner`, an optional input); base classes that
//!   list does not have, and all of them without it, go in alphabetical order of asset name, which is stable
//!   across dumps and is also the order the game uses today.
//! * A class lists `activatedAbilities` and `passiveSkills` (each entry with its `unlockLevel`, a skill entry also
//!   with the tree it belongs to); both point at their definition through an Addressables asset GUID, which the
//!   catalog turns into an asset path whose file stem is the definition's name.
//! * `CharacterLevelsSO`: the level cap (`levelCap`) at which level-dependent numbers are given. The table is the
//!   one `GameRunner` points at, or without it the one with the highest cap (the dump also has a demo's table).
//! * `AbilityDefinitionSO`: one row for the ability, then one per entry of `abilityUpgrades` and of
//!   `abilityEnhancements` (row id `<ability>/<entry name>`), with their `costs` (`Currency`) and the subclasses
//!   they are limited to.
//! * `PassiveSkillDefinitionSO`: one row per skill, with `maxSkillPoints` ranks.
//! * Names, descriptions and tooltips are the English localisation texts the definitions point at.
//!
//! Columns worth explaining:
//!
//! * `data`: the description of the row's game data in the notation of the `describe` module.
//! * `per_rank`: the upgrade tooltip of a skill as the game would show it at each rank. The tooltip text has
//!   numbered placeholders (`{0}`, `{1:0.0}`, `{2:shown if positive;if negative;if zero}`) which the skill's
//!   `descriptionParameters` fill, either from a table of values per rank or from one parameter of one of the
//!   skill's bonuses. A best-effort reconstruction of the game's own tooltip code: a value that cannot be found
//!   shows as `nan`, a placeholder without a parameter as `?`.
//! * `dps_relevant`: `Y` when a bonus type or status-effect type involved is one that changes damage output,
//!   `maybe` when only the description's wording suggests it, else `N`.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::bonus;
use crate::describe::{items, named, ref_name, whole, Enums, Game, Obj, Scale, Seen, HEALTH_FACTOR, HEALTH_FACTOR_PER_MINUTE};

/// Members of `AbilityDescriptionParameterSource`: where the number in an upgrade's description comes from.
pub(crate) const FROM_BONUS: &str = "BonusParameter";
const FROM_EFFECT_SET: &str = "StatusEffectSet";
const FROM_MANUAL: &str = "ManualValue";

/// An enum field as the dump writes it: `{"value": n}` or the bare number.
fn enum_value(v: Option<&Value>) -> i128 { v.and_then(|x| x.get("value").and_then(whole).or_else(|| whole(x))).unwrap_or(0) }
use crate::{fmt, truthy};

/// One row of the class-skills table. Every field is the text of that column's cell; a cell that does not apply
/// to the row's kind is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillRow {
    /// title of the base class
    pub cls: String,
    /// title of the subclass, empty for the base class's own rows
    pub subclass: String,
    /// "active ability", "ability upgrade", "ability enhancement" or "passive skill"
    pub kind: String,
    /// the skill tree of a passive skill, as the game's `SkillPointType` names it, in lower case ("subclass")
    pub point_type: String,
    /// asset name of the definition; `<ability>/<entry name>` for an upgrade or enhancement
    pub id: String,
    pub name: String,
    pub unlock_level: String,
    pub max_rank: String,
    pub requires: String,
    pub cooldown_s: String,
    pub duration_s: String,
    pub cost: String,
    pub description: String,
    pub per_rank: String,
    pub max_rank_bonus: String,
    /// "Y", "maybe" or "N"
    pub dps_relevant: String,
    pub data: String,
}

impl SkillRow {
    /// The column names, in the table's order.
    pub const COLUMNS: [&'static str; 17] = [
        "cls", "subclass", "kind", "point_type", "id", "name", "unlock_level", "max_rank", "requires", "cooldown_s",
        "duration_s", "cost", "description", "per_rank", "max_rank_bonus", "dps_relevant", "data",
    ];

    /// The cells in the order of [`SkillRow::COLUMNS`].
    pub fn cells(&self) -> [&str; 17] {
        [
            &self.cls, &self.subclass, &self.kind, &self.point_type, &self.id, &self.name, &self.unlock_level,
            &self.max_rank, &self.requires, &self.cooldown_s, &self.duration_s, &self.cost, &self.description,
            &self.per_rank, &self.max_rank_bonus, &self.dps_relevant, &self.data,
        ]
    }

    /// Lower-case asset names of the status-effect sets the row's data mentions, in order of first mention.
    pub(crate) fn sets(&self) -> Vec<String> {
        let mut out = Vec::new();
        mentioned_sets(&self.data, &mut out);
        out
    }
}

/// `StatusEffectType` members that change damage output. This classification and the next are a judgement of ours
/// about the game's mechanics; nothing in the data says it, so the members are named (and checked: see `check`).
pub(crate) const DPS_EFFECT_TYPES: [&str; 18] = [
    "WeaponDamage", "WeaponTimeBetweenShots", "WeaponCritChance", "WeaponCritDamage", "WeaponReloadTime",
    "PierceModifier", "IncomingDamageModifier", "DOTTickRateModifier", "StackableWeaponReloadTimeModifier",
    "StackableWeaponDamageModifier", "MissingHealthBasedWeaponDamage", "ChanceToNotConsumeAmmo",
    "ChanceToNotConsumePierce", "AbilityCooldownModifier", "TrackedValueBonusRamp", "IncomingCritChance",
    "DamageOverTime", "WeaponAccuracy",
];

/// `BonusType` members that change damage output.
pub(crate) const DPS_BONUS_TYPES: [&str; 28] = [
    "ReloadTime", "CritChance", "AdditionalDamage", "CritDamageMultiplier", "HdAmmoDamageModifier",
    "ExtraHdAmmoMagazines", "WeaponAccuracy", "CooldownReduction", "ProjectileDamage", "ProjectilePierce",
    "WeaponTimeBetweenShots", "StatusEffectWeaponDamage", "StatusEffectWeaponPierce", "StatusEffectDOT",
    "StatusEffectWeaponTimeBetweenShots", "StatusEffectReloadTime", "WeaponMagazineCapacity",
    "DamageIncreaseInReviveZones", "ExposedSetLevel", "AppliesUnstableChemistry", "ElementalEffectChance",
    "WeaponChargeSpeedMultiplier", "ExplosiveCritChance", "InspiredGreatnessRank", "ElementalSetLevel",
    "AdditionalExplodeRadius", "DeployableAmmo", "StatusEffectCooldownModifier",
];

/// Words in an English description that suggest an effect on damage output (ours, not the game's).
const DPS_WORDS: [&str; 8] = ["damage", "fire rate", "reload", "crit", "cooldown", "ammo", "pierce", "vulnerable"];

fn dps_relevant(bonus_types: &[&str], seen: &Seen, description: &str) -> &'static str {
    if bonus_types.iter().any(|t| DPS_BONUS_TYPES.contains(t)) || DPS_EFFECT_TYPES.iter().any(|t| seen.effect_types.contains(*t)) {
        return "Y";
    }
    let description = description.to_lowercase();
    if DPS_WORDS.iter().any(|word| description.contains(word)) { "maybe" } else { "N" }
}

/// Adds the set names a description mentions: what follows `set=` (as a whole field name), `statusEffectSet=` /
/// `StatusEffectSet=` (also as the end of a longer field name) or the heading "set `", as far as letters, digits
/// and `_ . -` go. This is the reference's regular expression
/// ``(?:\bset=|[sS]tatusEffectSet=|set `)([A-Za-z0-9_.\-]+)``, including that a name stops at its first space.
fn mentioned_sets(text: &str, out: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let after_word = |at: usize| text[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_');
        let prefix = if rest.starts_with(b"set=") && !after_word(i) {
            4
        } else if rest.starts_with(b"statusEffectSet=") || rest.starts_with(b"StatusEffectSet=") {
            16
        } else if rest.starts_with(b"set `") {
            5
        } else {
            0
        };
        let len = match prefix {
            0 => 0,
            _ => rest[prefix..].iter().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-')).count(),
        };
        if len == 0 {
            i += 1;
            continue;
        }
        let name = text[i + prefix..i + prefix + len].to_ascii_lowercase();
        if !out.contains(&name) {
            out.push(name);
        }
        i += prefix + len;
    }
}

const SECONDS_PER_MINUTE: f64 = 60.0;

/// A tooltip value: a number still to be formatted by its placeholder, or a finished text.
enum Shown {
    Number(f64),
    Text(String),
}

/// One placeholder: `index` into the values, `format` the part after the colon if there is one.
fn placeholder(index: Option<usize>, format: Option<&str>, values: &[Shown]) -> String {
    let v = match index.and_then(|i| values.get(i)) {
        None => return "?".into(),
        Some(Shown::Text(text)) => return text.clone(),
        Some(Shown::Number(v)) => *v,
    };
    match format.filter(|f| !f.is_empty()) {
        // "positive;negative;zero": the section for the value's sign is the whole replacement
        Some(sections) if sections.contains(';') => {
            let mut sections = sections.split(';');
            let which = if v > 0.0 { 0 } else if v < 0.0 { 1 } else { 2 };
            sections.nth(which).unwrap_or("").to_string()
        }
        // "0.00": that many decimals
        Some(zeros) if zeros.len() > 2 && zeros.starts_with("0.") && zeros[2..].bytes().all(|b| b == b'0') => fmt::fixed(v, zeros.len() - 2),
        _ => fmt::num(fmt::round(v, 3)),
    }
}

/// Replaces every `{n}` and `{n:format}` of a tooltip text.
fn fill(template: &str, values: &[Shown]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        let tail = &after[digits..];
        let parsed = if digits == 0 {
            None
        } else if let Some(remaining) = tail.strip_prefix('}') {
            Some((None, remaining))
        } else {
            let format = tail.strip_prefix(':');
            format.and_then(|f| f.find('}').map(|close| (Some(&f[..close]), &f[close + 1..])))
        };
        match parsed {
            Some((format, remaining)) => {
                out.push_str(&placeholder(after[..digits].parse().ok(), format, values));
                rest = remaining;
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out + rest
}

/// File name without its extension, of a path with either kind of separator.
fn stem(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match file.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < file.len() => &file[..dot],
        _ => file,
    }
}

fn by_name<'a>(so: &'a HashMap<String, Obj>, class: &str) -> HashMap<&'a str, &'a Obj> {
    // two objects with one name: the later one in the file counts
    so.get(class).into_iter().flat_map(|objects| objects.values()).filter_map(Value::as_object)
        .filter_map(|o| Some((o.get("_name")?.as_str()?, o))).collect()
}

/// A number cell: a whole number plain, a float rounded to three decimals and written as a float (`15.0`).
fn seconds(v: Option<&Value>) -> String {
    match v {
        None => "0".into(),
        Some(Value::Number(n)) if n.is_f64() => fmt::repr(fmt::round(n.as_f64().unwrap_or(0.0), 3)),
        Some(other) => fmt::text(other),
    }
}

/// "entityBonusStats" for the tooltip kind `Entity`: a kind that is named after a bonus list of the skill takes
/// its value from that list.
fn bonus_list(kind: &str) -> String {
    let mut letters = kind.chars();
    let first = letters.next().map(|c| c.to_lowercase().collect::<String>()).unwrap_or_default();
    format!("{first}{}BonusStats", letters.as_str())
}

/// What all the names end in, from a separator on ("-player-class" of the class assets).
fn common_ending<'n>(names: impl IntoIterator<Item = &'n str>) -> String {
    let mut names = names.into_iter();
    let Some(first) = names.next() else { return String::new() };
    let mut ending: Vec<char> = first.chars().rev().collect();
    let mut count = 1;
    for name in names {
        count += 1;
        let shared = ending.iter().zip(name.chars().rev()).take_while(|(a, b)| **a == *b).count();
        ending.truncate(shared);
    }
    // one name alone has no ending in common with anything
    if count < 2 {
        return String::new();
    }
    let ending: String = ending.into_iter().rev().collect();
    ending.trim_start_matches(char::is_alphanumeric).to_string()
}

/// The class a row belongs to.
struct Owner<'c> {
    cls: &'c str,
    subclass: &'c str,
}

struct Table<'a> {
    game: Game<'a>,
    skills: HashMap<&'a str, &'a Obj>,
    classes: HashMap<&'a str, &'a Obj>,
    /// localisation key -> text as written in the file
    texts: &'a HashMap<String, String>,
    guid_to_path: &'a dyn Fn(&str) -> Option<String>,
    /// the level by which every class ability is unlocked: the highest `unlockLevel` of any
    last_unlock: i64,
    /// what every class asset's name ends in; a class is called by its name without it
    class_ending: String,
    rows: Vec<SkillRow>,
}

impl<'a> Table<'a> {
    /// Name of the asset an Addressables reference field (`{"m_AssetGUID": ...}`) points at.
    fn asset_name(&self, reference: Option<&Value>) -> Option<String> {
        let path = (self.guid_to_path)(reference?.get("m_AssetGUID")?.as_str()?)?;
        Some(stem(&path).to_string())
    }

    /// The text a definition's field points at; empty when there is none.
    fn text(&self, o: &Obj, field: &str) -> &'a str {
        o.get(field).and_then(Value::as_str).and_then(|key| self.texts.get(key)).map_or("", String::as_str)
    }

    fn title(&self, o: &Obj, fallback: &str) -> String {
        let title = self.text(o, "title");
        if title.is_empty() { fallback.to_string() } else { title.to_string() }
    }

    /// The upgrade tooltip of a skill at `rank`.
    fn tooltip(&self, skill: &Obj, rank: i64) -> String {
        let template = self.text(skill, "upgradeDescription");
        if template.is_empty() {
            return String::new();
        }
        let mut values = Vec::new();
        for parameter in items(skill.get("descriptionParameters")) {
            let kind = named(&self.game.enums.description_kind, parameter.get("type").and_then(whole).unwrap_or(0));
            let manual = items(parameter.get("manualRankValues"));
            if !manual.is_empty() {
                let last = manual.len() as i64;
                let v = manual[(rank.min(last) - 1).rem_euclid(last) as usize].as_f64().unwrap_or(f64::NAN);
                values.push(match kind {
                    // a fraction of maximum health per second; the tooltip gives percent, per minute for the second kind
                    HEALTH_FACTOR | HEALTH_FACTOR_PER_MINUTE => {
                        let percent = v * 100.0 * if kind == HEALTH_FACTOR_PER_MINUTE { SECONDS_PER_MINUTE } else { 1.0 };
                        Shown::Text(format!("{}% of max HP", fmt::num(fmt::round(percent, 4))))
                    }
                    _ => Shown::Number(v),
                });
                continue;
            }
            // otherwise: parameter `paramType` of bonus number `bonusIndex` in the bonus list the kind is named after
            let list = items(skill.get(&bonus_list(kind)));
            let index = parameter.get("bonusIndex").and_then(Value::as_i64).unwrap_or(0);
            let index = if index < 0 { index + list.len() as i64 } else { index };
            let wanted = parameter.get("paramType").and_then(Value::as_i64).unwrap_or(0);
            let found = usize::try_from(index).ok().and_then(|i| list.get(i)).and_then(|bonus| {
                let is_wanted = |p: &&Value| p.get("parameterType").and_then(|t| t.get("value")).and_then(Value::as_i64) == Some(wanted);
                items(bonus.get("parameters")).iter().find(is_wanted)
            });
            values.push(Shown::Number(found.map_or(f64::NAN, |p| bonus::at_rank(p, rank, self.game.level_cap))));
        }
        fmt::clean(&fill(&fmt::decode(template), &values))
    }

    /// The value a status effect of kind `effect_type` has in the set that one of `bonus`'s behaviours applies:
    /// the effect's field named after its type (`ResistanceModifier` -> `resistanceModifier`).
    fn set_effect_value(&self, bonus: &Value, registry: &HashMap<i64, &Value>, effect_type: i128) -> Option<f64> {
        let member = self.game.enums.effect_type_names.get(&effect_type)?;
        let mut letters = member.chars();
        let field = letters.next()?.to_lowercase().collect::<String>() + letters.as_str();
        for list in ["additionalAbilityBehaviours", "additionalBehaviours"] {
            for behaviour in Game::listed(bonus.get(list).unwrap_or(&Value::Null), registry) {
                let Some(data) = behaviour.get("data") else { continue };
                let Some(set) = data.get("set").and_then(Value::as_str).and_then(|reference| self.game.sets.get(ref_name(reference))) else { continue };
                let levels = items(set.get("leveledSet"));
                // the level of the set the behaviour applies; the first when it names none
                let level = data.get("setLevel").and_then(Value::as_i64).unwrap_or(1).max(1) as usize - 1;
                for leveled in levels.get(level).into_iter().chain(levels.iter()) {
                    let effects = items(leveled.get("statusEffects")).iter().filter_map(|e| e.get("statusEffect"));
                    let value = effects.filter(|e| enum_value(e.get("type")) == effect_type).find_map(|e| e.get(&field)?.as_f64());
                    if value.is_some() {
                        return value;
                    }
                }
            }
        }
        None
    }

    /// The description of an ability upgrade or enhancement with its numbers filled in, as the game does from
    /// the entry's `descriptionParameters` (`AbilityDescriptionParameterHelper.GetDescriptionParameter`): a
    /// status effect of the set one of its bonuses applies, times `multiplier`, plus `offset`; or a number given
    /// outright, plus `offset`. A number that cannot be found shows as `?`.
    ///
    /// The third kind, a parameter of one of its bonuses, grows with the player's level. Its placeholder is left
    /// in the text (as `{n}`), for whoever shows the description to a player of a known level to fill
    /// (`upgrade_parameter_at_level`).
    fn perk_description(&self, entry: &Obj, registry: &HashMap<i64, &Value>) -> String {
        let template = self.text(entry, "description");
        let parameters = items(entry.get("descriptionParameters"));
        if parameters.is_empty() {
            return fmt::clean(template);
        }
        let bonuses = items(entry.get("bonuses"));
        let mut values: Vec<Shown> = Vec::new();
        for parameter in parameters {
            let number = |field: &str| parameter.get(field).and_then(Value::as_f64).unwrap_or(0.0);
            let index = parameter.get("bonusIndex").and_then(Value::as_i64).unwrap_or(0);
            let index = if index < 0 { index + bonuses.len() as i64 } else { index };
            let bonus = usize::try_from(index).ok().and_then(|i| bonuses.get(i));
            let found = match named(&self.game.enums.description_source, enum_value(parameter.get("source"))) {
                FROM_MANUAL => Some(number("manualValue") + number("offset")),
                FROM_EFFECT_SET => bonus.and_then(|b| self.set_effect_value(b, registry, enum_value(parameter.get("statusEffectType"))))
                    .map(|v| v * number("multiplier") + number("offset")),
                FROM_BONUS => {
                    values.push(Shown::Text(format!("{{{}}}", values.len())));
                    continue;
                }
                _ => None,
            };
            values.push(match found {
                // single-precision noise off the product: 0.12 * 100 is 12
                Some(v) => Shown::Number(fmt::round(v, 4)),
                None => Shown::Text("?".into()),
            });
        }
        fmt::clean(&fill(&fmt::decode(template), &values))
    }

    fn skill_row(&mut self, owner: &Owner, class_entry: &Value) {
        let Some(name) = self.asset_name(class_entry.get("skillDef")) else { return };
        let Some(skill) = self.skills.get(name.as_str()).copied() else { return };
        // the skill tree, called as the game's enum calls it: "basic", "class", "subclass"
        let tree = class_entry.get("skillType").and_then(whole).unwrap_or(0);
        let point_type = &named(&self.game.enums.skill_point_type, tree).to_lowercase();
        let max_rank = skill.get("maxSkillPoints").and_then(Value::as_i64).unwrap_or(0);
        let references = self.game.strip_field(skill, "references");
        let registry = self.game.registry(&references);
        let (mut seen, mut lines, mut bonus_types) = (Seen::default(), Vec::new(), Vec::new());
        for (scale, lists) in [
            (Scale::Ranks(max_rank), [("entityBonusStats", "[entity] "), ("weaponBonusStats", "[weapon] "), ("abilityBonusStats", "[ability] ")].as_slice()),
            // a bonus granted on reaching the last rank has one value
            (Scale::Ranks(1), [
                ("maxRankEntityBonuses", "[max rank, entity] "), ("maxRankWeaponBonuses", "[max rank, weapon] "),
                ("maxRankAbilityBonuses", "[max rank, ability] "), ("maxRankProfileBonuses", "[max rank, profile] "),
            ].as_slice()),
        ] {
            for (list, prefix) in lists {
                for bonus in items(skill.get(*list)) {
                    bonus_types.push(self.game.bonus(bonus, scale, &registry, prefix, &mut seen, &mut lines));
                }
            }
        }
        self.game.follow_references(&mut seen, None, &mut lines);

        let description = fmt::clean(self.text(skill, "description"));
        let tooltips = (1..=max_rank).map(|rank| (rank, self.tooltip(skill, rank))).filter(|(_, tip)| !tip.is_empty());
        let mut requires = Vec::new();
        if let Some(points) = skill.get("requiresSpentSkillPoints").filter(|v| truthy(v)) {
            requires.push(format!("{} {point_type} points already assigned", fmt::whole(points)));
        }
        if let Some(ability) = self.game.strip_field(skill, "requiredAbility").as_str().filter(|a| !a.is_empty()) {
            requires.push(format!("requires passive ability {}", ref_name(ability)));
        }
        let row = SkillRow {
            cls: owner.cls.into(),
            subclass: owner.subclass.into(),
            kind: "passive skill".into(),
            point_type: point_type.clone(),
            name: self.title(skill, &name),
            unlock_level: class_entry.get("unlockLevel").map_or("0".into(), fmt::text),
            max_rank: skill.get("maxSkillPoints").map_or("0".into(), fmt::text),
            requires: requires.join("; "),
            cost: format!("1 {point_type} skill point per rank"),
            per_rank: tooltips.map(|(rank, tip)| format!("R{rank}: {tip}")).collect::<Vec<_>>().join(" || "),
            max_rank_bonus: fmt::clean(self.text(skill, "maxRankBonusDesc")),
            dps_relevant: dps_relevant(&bonus_types, &seen, &description).into(),
            data: lines.join(" ;; "),
            description,
            id: name,
            ..SkillRow::default()
        };
        self.rows.push(row);
    }

    fn ability_rows(&mut self, owner: &Owner, class_entry: &Value) {
        let Some(name) = self.asset_name(class_entry.get("ability")) else { return };
        let Some(ability) = self.abilities_get(&name) else { return };
        let references = self.game.strip_field(ability, "references");
        let registry = self.game.registry(&references);
        let unlock = class_entry.get("unlockLevel");
        let unlock_level = unlock.and_then(Value::as_i64).unwrap_or(0);
        // the levels its scaling numbers are given at: when it unlocks, when all abilities are unlocked, and the cap
        let levels: BTreeSet<i64> = [unlock_level.max(1), self.last_unlock, self.game.level_cap].into();
        let levels: Vec<i64> = levels.into_iter().collect();

        let (mut seen, mut lines, mut bonus_types) = (Seen::default(), Vec::new(), Vec::new());
        self.game.ability(&name, &mut seen, &mut lines);
        for bonus in items(ability.get("abilityBonuses")) {
            bonus_types.push(self.game.bonus(bonus, Scale::Levels(&levels), &registry, "level scaling: ", &mut seen, &mut lines));
        }
        self.game.follow_references(&mut seen, Some(&name), &mut lines);
        let description = fmt::clean(self.text(ability, "description"));
        let duration = ability.get("abilityDuration").filter(|v| v.as_f64().is_some_and(|d| fmt::round(d, 3) != 0.0));
        let unlock_text = unlock.map_or("0".into(), fmt::text);
        let mut rows = vec![SkillRow {
            cls: owner.cls.into(),
            subclass: owner.subclass.into(),
            kind: "active ability".into(),
            id: name.clone(),
            name: self.title(ability, &name),
            unlock_level: unlock_text.clone(),
            cooldown_s: seconds(ability.get("cooldown")),
            duration_s: duration.map_or(String::new(), |d| seconds(Some(d))),
            cost: format!("unlocks at level {}", unlock.map_or("0".into(), fmt::whole)),
            dps_relevant: dps_relevant(&bonus_types, &seen, &description).into(),
            data: lines.join(" ;; "),
            description,
            ..SkillRow::default()
        }];

        for (list, kind) in [("abilityUpgrades", "ability upgrade"), ("abilityEnhancements", "ability enhancement")] {
            for entry in items(ability.get(list)).iter().filter_map(Value::as_object) {
                let (mut seen, mut lines, mut bonus_types) = (Seen::default(), Vec::new(), Vec::new());
                for bonus in items(entry.get("bonuses")) {
                    bonus_types.push(self.game.bonus(bonus, Scale::Levels(&[self.game.level_cap]), &registry, "", &mut seen, &mut lines));
                }
                self.game.follow_references(&mut seen, Some(&name), &mut lines);
                let costs = self.game.strip_field(entry, "costs");
                let price = |c: &Value| format!("{} {}", fmt::text(&c["cost"]), ref_name(c["currency"].as_str().unwrap_or("None")));
                let allowed = self.game.strip_field(entry, "allowedPlayerClasses");
                let only: Vec<String> = items(Some(&allowed)).iter()
                    .map(|class| ref_name(class.as_str().unwrap_or("None")).replace(&self.class_ending, "")).collect();
                let entry_name = entry.get("name").and_then(Value::as_str).unwrap_or("");
                let description = self.perk_description(entry, &registry);
                rows.push(SkillRow {
                    cls: owner.cls.into(),
                    subclass: owner.subclass.into(),
                    kind: kind.into(),
                    id: format!("{name}/{entry_name}"),
                    name: self.title(entry, entry_name),
                    unlock_level: unlock_text.clone(),
                    requires: if only.is_empty() { String::new() } else { format!("subclass {}", only.join("/")) },
                    cost: items(Some(&costs)).iter().map(price).collect::<Vec<_>>().join(" + "),
                    dps_relevant: dps_relevant(&bonus_types, &seen, &description).into(),
                    data: lines.join(" ;; "),
                    description,
                    ..SkillRow::default()
                });
            }
        }
        self.rows.append(&mut rows);
    }

    fn abilities_get(&self, name: &str) -> Option<&'a Obj> { self.game.abilities.get(name).copied() }

    /// The rows of one class or subclass; `base_title` is given for a subclass. Returns the class and its title.
    fn class_rows(&mut self, class_name: &str, base_title: Option<&str>) -> Result<(&'a Obj, String), String> {
        let class = *self.classes.get(class_name)
            .ok_or_else(|| format!("PlayerClassDefinitionSO has no class named {class_name}"))?;
        let mut title = fmt::clean(self.text(class, "title"));
        if title.is_empty() {
            title = class.get("className").and_then(Value::as_str).unwrap_or("").to_string();
        }
        let owner = match base_title {
            Some(base) => Owner { cls: base, subclass: &title },
            None => Owner { cls: &title, subclass: "" },
        };
        for entry in items(class.get("activatedAbilities")) {
            self.ability_rows(&owner, entry);
        }
        for entry in items(class.get("passiveSkills")) {
            self.skill_row(&owner, entry);
        }
        Ok((class, title))
    }
}

/// Asset names of the classes an object lists by path id reference.
fn referenced<'a>(index: &HashMap<&'a str, (&'a str, &'a str)>, list: Option<&Value>) -> Vec<&'a str> {
    let ids = items(list).iter().filter_map(|reference| Some(reference.get("m_PathID")?.to_string()));
    ids.filter_map(|id| index.get(id.as_str()).map(|(_, name)| *name)).collect()
}

/// The asset names of the base classes, in the order of the module description.
fn base_classes<'a>(
    classes: &HashMap<&'a str, &'a Obj>,
    index: &HashMap<&'a str, (&'a str, &'a str)>,
    listed_by_game: &[String],
) -> Vec<&'a str> {
    let subclasses: Vec<&str> = classes.values().flat_map(|class| referenced(index, class.get("subclasses"))).collect();
    let mut bases: Vec<&str> = classes.keys().copied().filter(|name| !subclasses.contains(name)).collect();
    let place = |name: &str| listed_by_game.iter().position(|listed| listed == name).unwrap_or(listed_by_game.len());
    bases.sort_by(|a, b| place(a).cmp(&place(b)).then(a.cmp(b)));
    bases
}

/// `texts` are the localisation texts as written (`loc::Texts::raw`); `enums` the game's enums, of which a missing
/// one names nothing (the caller decides which it insists on).
pub(crate) fn rows(
    so: &HashMap<String, Obj>,
    texts: &HashMap<String, String>,
    enums: &Value,
    guid_to_path: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<SkillRow>, String> {
    // sorted so that the result cannot depend on the hash map's order, should two classes ever share a path id
    let mut class_names: Vec<&String> = so.keys().collect();
    class_names.sort();
    let mut index = HashMap::new();
    for class in class_names {
        for (path_id, object) in &so[class] {
            let name = object.get("_name").and_then(Value::as_str).unwrap_or("");
            index.entry(path_id.as_str()).or_insert((class.as_str(), name));
        }
    }
    let classes = by_name(so, "PlayerClassDefinitionSO");
    // the game's main object, when given: it is dumped under its bundle's name, and is the only object there
    let runner = so.get(crate::GAME_RUNNER).and_then(|objects| objects.values().find_map(Value::as_object));
    let listed_by_game: Vec<String> = runner.map_or(&[][..], |r| items(r.get("playerClassDefinitions"))).iter()
        .filter_map(|reference| Some(stem(&guid_to_path(reference.get("m_AssetGUID")?.as_str()?)?).to_string())).collect();
    let level_tables = so.get("CharacterLevelsSO");
    let cap = |table: &Value| table.get("levelCap").and_then(Value::as_i64);
    let level_cap = runner.and_then(|r| r.get("defaultCharacterLevelsTable")?.get("m_PathID")).map(Value::to_string)
        .and_then(|id| level_tables?.get(&id)).and_then(cap)
        .or_else(|| level_tables?.values().filter_map(cap).max())
        .ok_or("CharacterLevelsSO has no table with a levelCap")?;
    let unlock_levels = classes.values().flat_map(|class| items(class.get("activatedAbilities")))
        .filter_map(|entry| entry.get("unlockLevel")?.as_i64());

    let mut table = Table {
        last_unlock: unlock_levels.max().unwrap_or(1),
        class_ending: common_ending(classes.keys().copied()),
        game: Game {
            abilities: by_name(so, "AbilityDefinitionSO"),
            sets: by_name(so, "StatusEffectSetDefinitionSO"),
            entities: by_name(so, "EntityDefinitionSO"),
            enums: Enums::new(enums),
            level_cap,
            index,
        },
        skills: by_name(so, "PassiveSkillDefinitionSO"),
        classes,
        texts,
        guid_to_path,
        rows: Vec::new(),
    };
    for base_name in base_classes(&table.classes, &table.game.index, &listed_by_game) {
        let (base, base_title) = table.class_rows(base_name, None)?;
        for reference in items(base.get("subclasses")) {
            let id = reference.get("m_PathID").map(Value::to_string).unwrap_or_default();
            let (_, sub_name) = *table.game.index.get(id.as_str())
                .ok_or_else(|| format!("{base_name}: subclass with path id {id} is not among the dumped objects"))?;
            table.class_rows(sub_name, Some(&base_title))?;
        }
    }
    Ok(table.rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        mentioned_sets(text, &mut out);
        out
    }

    #[test]
    fn set_mentions() {
        assert_eq!(found("duration=4, set=Dash_Set, setLevel=1"), ["dash_set"]);
        assert_eq!(found("{appliedStatusEffectSet=A-1.b, statusEffectSet=B c}; offset=x, reset=y"), ["a-1.b", "b"]);
        assert_eq!(found("set `Loaded Chamber` set `x`, set=X, set=[y], set={set=z}"), ["loaded", "x", "z"]);
        assert_eq!(found("éset=a (set=b)"), ["b"]);
    }

    #[test]
    fn tooltip_placeholders() {
        let values = [Shown::Number(4.0), Shown::Number(-0.125), Shown::Text("2% of max HP".into()), Shown::Number(f64::NAN)];
        assert_eq!(fill("Speed: {0}%, {1:0.00} {1:up;down;same}, {2}, {3:a;b}|{3}, {9} {x} {0:} {4:1", &values),
                   "Speed: 4%, -0.12 down, 2% of max HP, |nan, ? {x} 4 {4:1");
        assert_eq!(fill("{1:0.0}{0:0.}{0:p;n;z}", &[Shown::Number(0.0), Shown::Number(2.25)]), "2.20z");
    }

    #[test]
    fn names_from_the_data() {
        assert_eq!(common_ending(["medic-player-class", "shock-trooper-player-class", "blackops-player-class"]), "-player-class");
        assert_eq!(common_ending(["heavies-class", "medics-class"]), "-class");
        assert_eq!(common_ending(["assault", "heavy"]), "");
        assert_eq!(common_ending(["medic-player-class"]), "");
        assert_eq!([bonus_list("Entity"), bonus_list("PlayerHealthFactorCalculated")], ["entityBonusStats", "playerHealthFactorCalculatedBonusStats"]);
    }

    #[test]
    fn base_class_order() {
        let class = |subs: &[i64]| -> Obj {
            let subs: Vec<Value> = subs.iter().map(|id| serde_json::json!({ "m_FileID": 0, "m_PathID": id })).collect();
            serde_json::from_value(serde_json::json!({ "subclasses": subs })).unwrap()
        };
        let (medic, heavy, assault, ops, engineer) = (class(&[4]), class(&[]), class(&[]), class(&[]), class(&[]));
        let classes = HashMap::from([("medic", &medic), ("heavy", &heavy), ("assault", &assault), ("ops", &ops), ("engineer", &engineer)]);
        let index = HashMap::from([("4", ("PlayerClassDefinitionSO", "ops")), ("9", ("Currency", "medic"))]);
        assert_eq!(base_classes(&classes, &index, &[]), ["assault", "engineer", "heavy", "medic"]);
        let game_order = ["medic".to_string(), "assault".to_string(), "gone".to_string()];
        assert_eq!(base_classes(&classes, &index, &game_order), ["medic", "assault", "engineer", "heavy"]);
    }

    #[test]
    fn stems_and_seconds() {
        assert_eq!(stem("Assets/Data/Abilities/dash_ability.asset"), "dash_ability");
        assert_eq!(stem("a\\b.c.asset"), "b.c");
        assert_eq!(stem(".hidden"), ".hidden");
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!([seconds(Some(&v("15.0"))), seconds(Some(&v("17.5004"))), seconds(Some(&v("12"))), seconds(None)], ["15.0", "17.5", "12", "0"]);
    }
}
