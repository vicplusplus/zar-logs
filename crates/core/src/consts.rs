//! The game's enum values and names, as the running build defines them.
//!
//! No number that belongs to the game is written in this program. Flag bits, state numbers, entity types and the
//! like are looked up by member name in the layout generated from the game's own metadata, so a game update that
//! renumbers an enum is followed without a code change. A member the game no longer has stops the meter with a
//! message that names it, rather than letting it decode with a stale number.

use std::sync::{Arc, RwLock};

use crate::layout::Layout;

#[derive(Clone, Debug)]
pub struct Consts {
    // HitEventDamageInfoFlags
    pub hit_final: u16,
    pub hit_crit: u16,
    pub hit_explosion: u16,
    pub hit_dodge: u16,
    pub hit_dot: u16,
    /// EventSource.Server: the authoritative copy of an event (the others are local predictions)
    pub src_server: u8,
    /// WorldState.InGame
    pub ws_ingame: u8,
    /// ClientState.RunningGame / GameEnded
    pub cs_running: u32,
    pub cs_ended: u32,
    // GameEntityType
    pub et_player: u8,
    pub et_enemy: u8,
    pub et_destructible: u8,
    /// GameEntityFlags.Boss
    pub flag_boss: u32,
    /// StatusEffectState.Applied
    pub fx_applied: u8,
    /// StatusEffectType.DoBehaviours: an effect that only carries triggers, no stat change of its own
    pub fx_trigger_only: u8,
    /// AbilitySlotState.Active
    pub ab_active: u8,
    /// EntityKillAttributedAttackType.Weapon / Ability
    pub at_weapon: u8,
    pub at_ability: u8,
    /// EntityKillAttributedAttackType member names, by value
    pub attack_types: Vec<String>,
    /// DamageType member names, by value (the count marker left out)
    pub damage_types: Vec<String>,
    /// EffectCategory member names, by value; empty when the layout has no such enum
    pub effect_categories: Vec<String>,
    /// GameEntityType member names, by value
    pub entity_types: Vec<String>,
    /// LevelDifficulty member names, by value
    pub difficulties: Vec<String>,
    /// LevelType.Hub: the level is the hub (Aegis Station), not a mission. None when this game build has no such enum.
    pub lt_hub: Option<u8>,
}

/// Members that mark the end of an enum rather than name a value.
fn is_marker(name: &str) -> bool { matches!(name, "COUNT" | "Count" | "Last" | "MAX" | "Max") }

/// Member names indexed by value: the first declared member wins a shared value, markers are left out.
fn names_by_value(lay: &Layout, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (member, value) in lay.members(name) {
        let Ok(at) = usize::try_from(*value) else { continue };
        if is_marker(member) || at > 4096 {
            continue;
        }
        if out.len() <= at {
            out.resize(at + 1, String::new());
        }
        if out[at].is_empty() {
            out[at] = member.clone();
        }
    }
    out
}

impl Consts {
    pub fn new(lay: &Layout) -> Consts {
        let e = |name: &str, member: &str| lay.en(name, member);
        Consts {
            hit_final: e("HitEventDamageInfoFlags", "FinalBlow") as u16,
            hit_crit: e("HitEventDamageInfoFlags", "IsCrit") as u16,
            hit_explosion: e("HitEventDamageInfoFlags", "IsExplosion") as u16,
            hit_dodge: e("HitEventDamageInfoFlags", "IsDodge") as u16,
            hit_dot: e("HitEventDamageInfoFlags", "DoT") as u16,
            src_server: e("EventSource", "Server") as u8,
            ws_ingame: e("WorldState", "InGame") as u8,
            cs_running: e("ClientState", "RunningGame") as u32,
            cs_ended: e("ClientState", "GameEnded") as u32,
            et_player: e("GameEntityType", "Player") as u8,
            et_enemy: e("GameEntityType", "Enemy") as u8,
            et_destructible: e("GameEntityType", "Destructible") as u8,
            flag_boss: e("GameEntityFlags", "Boss") as u32,
            fx_applied: e("StatusEffectState", "Applied") as u8,
            fx_trigger_only: e("StatusEffectType", "DoBehaviours") as u8,
            ab_active: e("AbilitySlotState", "Active") as u8,
            at_weapon: e("EntityKillAttributedAttackType", "Weapon") as u8,
            at_ability: e("EntityKillAttributedAttackType", "Ability") as u8,
            attack_types: names_by_value(lay, "EntityKillAttributedAttackType"),
            damage_types: names_by_value(lay, "DamageType"),
            effect_categories: names_by_value(lay, "EffectCategory"),
            entity_types: names_by_value(lay, "GameEntityType"),
            difficulties: names_by_value(lay, "LevelDifficulty"),
            lt_hub: lay.members("LevelType").iter().find(|(m, _)| m == "Hub").map(|(_, v)| *v as u8),
        }
    }

    /// The word this program uses for where damage came from, from the game's attack type: "weapon", "ability",
    /// "entity", "behaviour" (EntityBehaviour), "shield", "environment", and the lower-cased member for any type
    /// a later game version adds.
    pub fn attack_kind(&self, attack_type: usize) -> String {
        match self.attack_types.get(attack_type).map(|m| m.to_lowercase()) {
            Some(m) if m.len() > "entity".len() && m.starts_with("entity") => m["entity".len()..].to_string(),
            Some(m) if !m.is_empty() => m,
            _ => "?".into(),
        }
    }

    /// "ExplosiveBarrel" -> "Explosive barrel": how an entity type is shown. Empty for type 0 (none).
    pub fn entity_type(&self, t: u8) -> String {
        if t == 0 {
            return String::new();
        }
        let Some(member) = self.entity_types.get(t as usize).filter(|m| !m.is_empty()) else { return String::new() };
        let mut out = String::new();
        for (n, c) in member.chars().enumerate() {
            if n > 0 && c.is_ascii_uppercase() {
                out.push(' ');
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The difficulty's name; empty for "not applicable" (value 0).
    pub fn difficulty(&self, d: u8) -> String {
        if d == 0 { String::new() } else { self.difficulties.get(d as usize).cloned().unwrap_or_default() }
    }

    pub fn damage_type(&self, dt: usize) -> Option<&str> { self.damage_types.get(dt).map(String::as_str).filter(|s| !s.is_empty()) }
}

static CURRENT: RwLock<Option<Arc<Consts>>> = RwLock::new(None);

/// Make the enum values of `lay` the ones in use. Called whenever a layout is loaded (the layout belongs to the
/// game build that is running, and that can change while the app stays open).
pub fn install(lay: &Layout) -> Arc<Consts> {
    let c = Arc::new(Consts::new(lay));
    *CURRENT.write().unwrap() = Some(c.clone());
    c
}

/// The enum values in use. Panics when no layout has been loaded yet: nothing can be decoded or named before.
pub fn get() -> Arc<Consts> {
    CURRENT.read().unwrap().clone().expect("no layout loaded: the game's enum values are not known yet")
}

/// Enum values for tests: made up, and deliberately not the game's. Nothing in this program may depend on what
/// the real numbers are, so a test that passes with these says so.
#[cfg(test)]
pub fn install_for_tests() -> Arc<Consts> {
    let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let c = Arc::new(Consts {
        hit_final: 0x40, hit_crit: 0x02, hit_explosion: 0x100, hit_dodge: 0x08, hit_dot: 0x10,
        src_server: 3, ws_ingame: 5, cs_running: 7, cs_ended: 9,
        et_player: 4, et_enemy: 2, et_destructible: 1, flag_boss: 0x800,
        fx_applied: 6, fx_trigger_only: 25, ab_active: 2, at_weapon: 5, at_ability: 6,
        attack_types: names(&["", "", "", "", "Entity", "Weapon", "Ability"]),
        damage_types: names(&["Physical", "Heat"]),
        effect_categories: Vec::new(),
        entity_types: names(&["", "Destructible", "Enemy", "Turret", "Player", "ExplosiveBarrel"]),
        difficulties: names(&["", "Easy", "Hard"]),
        lt_hub: Some(3),
    });
    *CURRENT.write().unwrap() = Some(c.clone());
    c
}
