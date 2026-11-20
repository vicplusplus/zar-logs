//! Encounter model: turns the capture's records into per-mission statistics.
//!
//! One Encounter = one loaded world (mission, operation, boss raid). The hub is a loaded world too, but not a
//! mission: see `Encounter::is_mission`.
//! All times are world seconds (GameEvent.time) relative to the first InGame frame.
//!
//! The log time of an encounter is the mission time: from the first InGame frame (after the intro) until the mission
//! ends (client state leaves RunningGame) or the player leaves. Every rate (DPS) is damage divided by that time, so
//! it does not depend on when shots were fired; standing still makes the log longer, never shorter.
//!
//! The game's own timer (result screen) also counts the intro and outro cutscenes. It is kept as a secondary figure,
//! `game_time`, read from the game's result data when the level is left.
//!
//! Whose damage is counted:
//!
//! * Damage a player's weapons, abilities and deployables did, and blasts of barrels they set off.
//! * Not damage an enemy did. The game attributes an enemy's hits to a player (that is how it credits kills), but
//!   the player did not deal them. A zombie clawing a turret is never counted.
//! * Two kinds of hit are counted apart, because whether they are "yours" is a matter of taste: damage to
//!   structures (anything that is not an enemy: crates, pods, barrels, level geometry), and the blast of an enemy
//!   that explodes (a Detonator), which the game credits to a player. Every figure exists four times, for each
//!   choice of the two (`Counted`); the one with neither is shown unless the user asks otherwise (`view`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use std::sync::Arc;

use crate::consts::{self, Consts};
use crate::names::{Attacker, Owner, OwnerCtx, Resolver};
use crate::records::*;

/// Seconds per column of the time series (ours, not the game's).
const BUCKET: f64 = 1.0;
/// How many damage types an aggregate keeps apart. The game has six today; a seventh would be counted in the
/// totals and left out of the per-type split until this grows.
const TYPE_SLOTS: usize = 8;
pub const FORMAT: u32 = 9;
/// A hit whose source definition is not known yet waits this long (world seconds) for the definition tables of the
/// mission to be read again, so that it can be told whether an enemy did it.
const UNKNOWN_HOLD: f64 = 2.0;
/// The game writes its result data (time, outcome, experience) in one go; it is read again this long (seconds) after
/// the time first showed, so that the fields written last are in.
const RESULT_SETTLE: f64 = 0.3;
/// A bare effect that starts this close (seconds) to the end of an ability's effect follows from it.
const FOLLOW_GAP: f64 = 0.12;

/// Python's `round(x, nd)`: correctly rounded on the exact binary value.
fn rnd(x: f64, nd: usize) -> f64 {
    if !x.is_finite() { x } else { format!("{x:.nd$}").parse().unwrap_or(x) }
}
fn bucket(t: f64) -> i64 { (t / BUCKET).floor() as i64 }
fn min_f(a: f64, b: f64) -> f64 { if b < a { b } else { a } }
fn max_f(a: f64, b: f64) -> f64 { if b > a { b } else { a } }

/// Damage a hit would have dealt with no resistance. Rough by design: per pool, what got through divided by
/// (1 - the resistance the game reports for that hit). A hit on an immune pool lets nothing through, so the base
/// damage of the attack stands in for it.
fn pre_of(h: &Hit) -> f64 {
    let mut pre = 0.0;
    for (dealt, r) in [(h.dhu, h.res), (h.dsu, h.ress)] {
        if dealt > 0.0 {
            pre += if r < 0.999 { dealt / (1.0 - r) } else { dealt };
        }
    }
    if pre == 0.0 && (h.res >= 0.999 || h.ress >= 0.999) {
        pre = h.da;
    }
    pre
}

/// Lowest and highest resistance the game applied per damage type: [health lo, hi, shield lo, hi].
type Seen = BTreeMap<u8, [Option<f64>; 4]>;

fn seen_add(table: &mut Seen, h: &Hit) {
    let e = table.entry(h.dt).or_insert([None; 4]);
    let mut put = |i: usize, v: f64| {
        e[i] = Some(e[i].map_or(v, |x| min_f(x, v)));
        e[i + 1] = Some(e[i + 1].map_or(v, |x| max_f(x, v)));
    };
    if h.dhu > 0.0 || h.res >= 0.999 {
        put(0, h.res);
    }
    if h.dsu > 0.0 || h.ress >= 0.999 {
        put(2, h.ress);
    }
}

/// Damage aggregate for one (player, source) or (player, target) pair.
#[derive(Clone, Debug)]
pub struct Agg {
    pub dmg: f64,
    crit_dmg: f64,
    dot: f64,
    shield: f64,
    over: f64,
    /// damage before resistances / after them (both uncapped by the target's health)
    pre: f64,
    unc: f64,
    dodged: u64,
    pub hits: u64,
    pub crits: u64,
    kills: u64,
    max: f64,
    min: f64,
    types: [f64; TYPE_SLOTS],
    first: Option<f64>,
    last: Option<f64>,
}

impl Default for Agg {
    fn default() -> Agg {
        Agg { dmg: 0.0, crit_dmg: 0.0, dot: 0.0, shield: 0.0, over: 0.0, pre: 0.0, unc: 0.0, dodged: 0, hits: 0, crits: 0,
            kills: 0, max: 0.0, min: f64::INFINITY, types: [0.0; TYPE_SLOTS], first: None, last: None }
    }
}

impl Agg {
    fn add(&mut self, c: &Consts, t: f64, h: &Hit, dmg: f64, kill: bool) {
        self.dmg += dmg;
        self.hits += 1;
        self.shield += h.ds;
        self.over += max_f(0.0, h.dhu - h.dh) + max_f(0.0, h.dsu - h.ds);
        self.unc += h.dhu + h.dsu;
        self.pre += pre_of(h);
        if h.fl & c.hit_dodge != 0 {
            self.dodged += 1;
        }
        if h.fl & c.hit_crit != 0 {
            self.crits += 1;
            self.crit_dmg += dmg;
        }
        if h.fl & c.hit_dot != 0 {
            self.dot += dmg;
        }
        if kill {
            self.kills += 1;
        }
        if dmg > self.max {
            self.max = dmg;
        }
        if 0.0 < dmg && dmg < self.min {
            self.min = dmg;
        }
        if (h.dt as usize) < self.types.len() {
            self.types[h.dt as usize] += dmg;
        }
        if self.first.is_none() {
            self.first = Some(t);
        }
        self.last = Some(t);
    }

    fn merge(&mut self, o: &Agg) {
        self.dmg += o.dmg;
        self.crit_dmg += o.crit_dmg;
        self.dot += o.dot;
        self.shield += o.shield;
        self.over += o.over;
        self.pre += o.pre;
        self.unc += o.unc;
        self.dodged += o.dodged;
        self.hits += o.hits;
        self.crits += o.crits;
        self.kills += o.kills;
        self.max = max_f(self.max, o.max);
        self.min = min_f(self.min, o.min);
        for (a, b) in self.types.iter_mut().zip(o.types) {
            *a += b;
        }
        self.first = match (self.first, o.first) { (None, x) => x, (x, None) => x, (Some(a), Some(b)) => Some(min_f(a, b)) };
        self.last = match (self.last, o.last) { (None, x) => x, (x, None) => x, (Some(a), Some(b)) => Some(max_f(a, b)) };
    }

    fn mitigated(&self) -> Value {
        if self.pre > 0.0 { json!(rnd(1.0 - self.unc / self.pre, 4)) } else { Value::Null }
    }

    fn types_json(&self) -> Value {
        let c = consts::get();
        Value::Object(self.types.iter().enumerate().filter(|(_, v)| **v != 0.0)
            .map(|(i, v)| (c.damage_type(i).map(str::to_string).unwrap_or_else(|| format!("Type {i}")), json!(v))).collect())
    }

    fn json(&self) -> Map<String, Value> {
        let v = json!({
            "damage": self.dmg, "hits": self.hits, "crits": self.crits, "crit_damage": self.crit_dmg, "max": self.max,
            "min": if self.min.is_infinite() { 0.0 } else { self.min }, "kills": self.kills, "dot": self.dot,
            "shield": self.shield, "overkill": self.over, "first": self.first, "last": self.last,
            "pre": rnd(self.pre, 1), "dodged": self.dodged, "mitigated": self.mitigated(), "types": self.types_json(),
        });
        match v { Value::Object(m) => m, _ => unreachable!() }
    }
}

/// What a set of figures counts besides the players' own damage to enemies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counted {
    /// damage to structures: anything that is not an enemy
    pub structures: bool,
    /// blasts of enemies that explode, which the game credits to a player
    pub blasts: bool,
}

impl Counted {
    /// Every choice; the first is the one a log's own figures are in.
    const ALL: [Counted; 4] = [
        Counted { structures: false, blasts: false }, Counted { structures: true, blasts: false },
        Counted { structures: false, blasts: true }, Counted { structures: true, blasts: true },
    ];
    fn index(self) -> usize { self.structures as usize | (self.blasts as usize) << 1 }
    /// Under which name a log keeps the figures of this choice (`views`).
    fn key(self) -> &'static str {
        match (self.structures, self.blasts) { (false, false) => "", (true, false) => "structures", (false, true) => "blasts", (true, true) => "structures+blasts" }
    }
    /// Whether a hit belongs in these figures.
    fn takes(self, structure: bool, blast: bool) -> bool { (!structure || self.structures) && (!blast || self.blasts) }
}

/// One player's damage in one choice of what is counted.
#[derive(Clone, Debug, Default)]
struct Figures {
    total: Agg,
    /// attack key -> aggregate, and its damage over time
    sources: IndexMap<String, (Agg, Series)>,
    series: Series,
}

/// Sparse per-bucket sums.
#[derive(Clone, Debug, Default)]
pub struct Series(BTreeMap<i64, f64>);

impl Series {
    fn add(&mut self, t: f64, v: f64) {
        *self.0.entry(if t > 0.0 { bucket(t) } else { 0 }).or_insert(0.0) += v;
    }
    fn dense(&self, n: i64) -> Vec<f64> { (0..n).map(|i| rnd(self.0.get(&i).copied().unwrap_or(0.0), 2)).collect() }
    fn merge(&mut self, o: &Series) {
        for (i, v) in &o.0 {
            *self.0.entry(*i).or_insert(0.0) += v;
        }
    }
    fn sparse(&self) -> Value { Value::Array(self.0.iter().map(|(i, v)| json!([i, rnd(*v, 2)])).collect()) }
}

#[derive(Clone, Debug, Default)]
struct FxMeta {
    types: BTreeSet<u8>,
    owner: u32,
    by: String,
    /// entity the game names as the giver of the effect (0 = none)
    giver: u32,
}

/// Status effects on one entity (a player or a boss) over time.
#[derive(Clone, Debug, Default)]
struct Fx {
    /// effect instance id -> (key, start, last seen, level)
    live: IndexMap<u16, (String, f64, f64, u8)>,
    /// effect key -> finished intervals
    done: IndexMap<String, Vec<[f64; 2]>>,
    meta: HashMap<String, FxMeta>,
}

pub struct Player {
    pub cid: u32,
    pub name: String,
    pub cls: String,
    pub sub: String,
    pub lvl: u8,
    pub local: bool,
    host: bool,
    /// the player's damage, once per choice of what is counted (`Counted::index`)
    fig: [Figures; 4],
    /// target key -> aggregate, without and with the blasts of enemies (rows of structures are told apart when shown)
    targets: [IndexMap<String, Agg>; 2],
    /// entity ids this player has had
    ents: HashSet<u32>,
    pub taken: f64,
    taken_hits: u64,
    taken_by: IndexMap<String, (f64, u64)>,
    taken_series: Series,
    healed: f64,
    kills_native: u32,
    pub downs: u32,
    accuracy: Option<f64>,
    native_total: Option<f64>,
    /// ability slot -> use (keyed by its trigger time) -> [start, end, cooldown end]
    abil: BTreeMap<usize, IndexMap<u64, (f64, f64, Option<f64>)>>,
    abil_hash: BTreeMap<usize, u32>,
    fx: Fx,
    hp_series: HashMap<i64, (f64, f64)>,
    hp_max: f64,
    pub build: Option<Value>,
    /// identity hash of an equipped weapon -> its position in the entity's weapon list
    weapon_slot: HashMap<u32, usize>,
}

impl Player {
    fn new(cid: u32) -> Player {
        Player { cid, name: format!("Player {cid}"), cls: String::new(), sub: String::new(), lvl: 0, local: false, host: false,
            fig: Default::default(), targets: Default::default(), ents: HashSet::new(), taken: 0.0,
            taken_hits: 0, taken_by: IndexMap::new(), taken_series: Series::default(), healed: 0.0, kills_native: 0, downs: 0,
            accuracy: None, native_total: None, abil: BTreeMap::new(), abil_hash: BTreeMap::new(), fx: Fx::default(),
            hp_series: HashMap::new(), hp_max: 0.0, build: None, weapon_slot: HashMap::new() }
    }
}

/// A boss enemy: what was done to it and what was on it, over time.
struct Boss {
    hash: u32,
    /// absolute world times (0 = unknown / alive)
    spawn: f64,
    death: f64,
    hp_max: f64,
    hp_series: HashMap<i64, (f64, f64)>,
    /// client id -> damage dealt to this boss
    series: IndexMap<u32, Series>,
    damage: f64,
    first_hit: Option<f64>,
    last_hit: Option<f64>,
    /// (active mods mask, stage index, mod level)
    mods: Option<(u32, u8, u8)>,
    /// mission time of the final blow on it
    killed_at: Option<f64>,
    fx: Fx,
    /// everything the party did to this one boss
    agg: Agg,
    seen: Seen,
}

/// One row of the raw hit log: (time, client, source key, target key, health, shield, flags, damage type, entity).
pub type HitRow = (f64, u32, String, String, f64, f64, u16, u8, u32);

pub struct Encounter {
    /// the game's enum values, as the build that was running defines them
    c: Arc<Consts>,
    pub match_id: Option<String>,
    pub id: String,
    pub start_wall: f64,
    pub end_wall: Option<f64>,
    /// world time of the first InGame frame
    t0: Option<f64>,
    /// latest world time seen (relative)
    t_last: f64,
    first_hit: Option<f64>,
    last_hit: Option<f64>,
    pub players: IndexMap<u32, Player>,
    /// entity ids of players (damage to them is "taken"), and their client ids
    friendly: HashSet<u32>,
    ent_client: HashMap<u32, u32>,
    local_cid: Option<u32>,
    pub mission: Value,
    pub result: Option<&'static str>,
    /// the party's damage over time, once per choice of what is counted
    party_series: [Series; 4],
    /// as `Player::targets`, for the party
    targets: [IndexMap<String, Agg>; 2],
    pub hits: Vec<HitRow>,
    pub server_hits: u64,
    predicted_hits: u64,
    other_hits: u64,
    /// hits an enemy did that the game attributes to a player and that are no blast: never counted
    enemy_hits: u64,
    /// blasts of enemies the game attributes to a player: counted apart
    blast_hits: u64,
    late: u64,
    pub dirty: bool,
    pub closed: bool,
    pub pending_builds: Option<Vec<PlayerInfo>>,
    /// hits seen before the first state sample told us who the players are
    pending_hits: Vec<Hit>,
    flushing: bool,
    pending_heals: Vec<(u8, u32, f64)>,
    bosses: IndexMap<u32, Boss>,
    pub bossmods: Vec<String>,
    /// identity hash of a destructible prop -> tag id
    prop_tag: HashMap<u32, i32>,
    pub tags: Vec<String>,
    pub enemymods: Vec<String>,
    /// the world's own tables: health, shield and resistances per entity definition
    pub defense: Option<Defense>,
    seen_res: HashMap<String, Seen>,
    /// the mission is over (outro / result screen); the log time is frozen
    pub ended: bool,
    world_first: Option<f64>,
    world_last: f64,
    /// the game's own mission time (with cutscenes)
    pub game_time: Option<f64>,
    pub outcome: Option<&'static str>,
    /// experience the local player gained, from the game's result data
    pub xp: Option<u64>,
    /// result data as it was when this encounter started (left over from the last one)
    res0: Option<GameResult>,
    /// wall time at which the game's result for this mission first showed, and whether it has been read in full
    result_at: Option<f64>,
    result_settled: bool,
}

/// One ability of a player as the timeline shows it.
struct AbLane {
    name: String,
    asset: String,
    hash: u32,
    uses: Vec<(f64, f64, Option<f64>)>,
}

fn obj(v: Value) -> Map<String, Value> { match v { Value::Object(m) => m, _ => Map::new() } }
fn intervals_of(v: &Value) -> Vec<[f64; 2]> {
    v.as_array().into_iter().flatten().filter_map(|iv| Some([iv.get(0)?.as_f64()?, iv.get(1)?.as_f64()?])).collect()
}
fn merge_intervals(mut ivs: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    ivs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<[f64; 2]> = Vec::new();
    for [a, z] in ivs {
        match merged.last_mut() {
            Some(last) if a <= last[1] + 0.15 => last[1] = max_f(last[1], z), // sampling gap: treat as continuous
            _ => merged.push([a, z]),
        }
    }
    merged
}

/// An effect the player would not think of as a buff: a pure trigger carrier, or on for an instant only.
fn internal(e: &Map<String, Value>) -> bool {
    let types: Vec<u64> = e.get("types").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).collect();
    let trigger_only = consts::get().fx_trigger_only as u64;
    (!types.is_empty() && types.iter().all(|t| *t == trigger_only)) || e.get("total").and_then(Value::as_f64).unwrap_or(0.0) < 0.3
}

/// Timeline rows in display order: each ability (active spans + cooldowns) with the effects its upgrades and
/// enhancement grant, then the remaining buffs (with where they come from), debuffs and other effects.
fn group_lanes(abilities: &[AbLane], effects: Vec<Map<String, Value>>, dur: f64, owner_of: Option<&dyn Fn(&Value) -> Option<Owner>>) -> Vec<Value> {
    let name_of = |e: &Map<String, Value>| e.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let slot_key = |ab: &AbLane| (if ab.asset.is_empty() { &ab.name } else { &ab.asset }).to_lowercase();
    // slot lists: one per ability; reachable by its asset name and by its display name
    let mut lists: Vec<Vec<(Map<String, Value>, Option<String>)>> = Vec::new();
    let mut slots: HashMap<String, usize> = HashMap::new();
    for ab in abilities {
        lists.push(Vec::new());
        slots.insert(slot_key(ab), lists.len() - 1);
        slots.entry(ab.name.to_lowercase()).or_insert(lists.len() - 1);
    }
    let mut rest: Vec<(Map<String, Value>, Option<Owner>)> = Vec::new();
    for e in effects {
        let en = name_of(&e).to_lowercase();
        if en.ends_with(" visuals") {
            continue; // purely cosmetic
        }
        let mut own = owner_of.and_then(|f| f(&Value::Object(e.clone())));
        if own.is_none() {
            let full = name_of(&e);
            for ab in abilities {
                let low = ab.name.to_lowercase();
                if en == low || en.starts_with(&(low.clone() + " ")) {
                    let perk = full.get(ab.name.len()..).unwrap_or("").trim().to_string();
                    own = Some(("ability", ab.name.clone(), (!perk.is_empty()).then_some(perk)));
                    break;
                }
            }
        }
        match &own {
            Some((kind, key, perk)) if *kind == "ability" && slots.contains_key(&key.to_lowercase()) => {
                lists[slots[&key.to_lowercase()]].push((e, perk.clone()));
            }
            _ => rest.push((e, own)),
        }
    }
    // An effect without a set carries no owner in the data. When every one of its spans (two or more) starts right
    // where a span of one ability, or of one of that ability's effects, starts or ends, it is that ability's
    // follow-up.
    let mut i = 0;
    while i < rest.len() {
        let (e, own) = &rest[i];
        let mut moved = false;
        if own.is_none() && e.get("key").and_then(Value::as_str).unwrap_or("").starts_with('t') {
            let spans = intervals_of(e.get("intervals").unwrap_or(&Value::Null));
            for ab in abilities {
                let li = slots[&slot_key(ab)];
                let mut edges: Vec<f64> = ab.uses.iter().flat_map(|u| [u.0, u.1]).collect();
                for (other, _) in &lists[li] {
                    edges.extend(intervals_of(other.get("intervals").unwrap_or(&Value::Null)).into_iter().flatten());
                }
                // ... or, used once or often, every span starts just as a span of one named effect of the ability
                // ends: what that effect leaves behind (the speed of Relay Race, once its dash is over)
                let follows = lists[li].iter().filter_map(|(other, perk)| Some((other, perk.as_ref()?))).find(|(other, _)| {
                    let ends: Vec<f64> = intervals_of(other.get("intervals").unwrap_or(&Value::Null)).iter().map(|iv| iv[1]).collect();
                    !spans.is_empty() && spans.iter().all(|s| ends.iter().any(|x| (s[0] - x).abs() <= FOLLOW_GAP))
                }).map(|(_, perk)| perk.clone());
                if follows.is_some() || (spans.len() >= 2 && spans.iter().all(|s| edges.iter().any(|x| (s[0] - x).abs() <= 0.35))) {
                    let (e, _) = rest.remove(i);
                    let perk = match follows { Some(perk) => format!("{perk}: {}", name_of(&e).to_lowercase()), None => name_of(&e) };
                    lists[li].push((e, Some(perk)));
                    moved = true;
                    break;
                }
            }
        }
        if !moved {
            i += 1;
        }
    }
    let (mut lanes, mut other): (Vec<Map<String, Value>>, Vec<Map<String, Value>>) = (Vec::new(), Vec::new());
    for ab in abilities {
        let mine = &lists[slots[&slot_key(ab)]];
        let mut intervals: Vec<[f64; 2]> = ab.uses.iter().map(|u| [rnd(u.0, 3), rnd(min_f(u.1, dur), 3)]).collect();
        let mut total: f64 = intervals.iter().map(|iv| iv[1] - iv[0]).sum();
        let base: Vec<&Map<String, Value>> = mine.iter().filter(|(_, perk)| perk.is_none()).map(|(e, _)| e).collect();
        if total < 0.05 && !base.is_empty() {
            // an instant ability (or a log without durations): the effect set it applies to you is its active span
            let tot = |e: &Map<String, Value>| e.get("total").and_then(Value::as_f64).unwrap_or(0.0);
            let mut best = base[0];
            for e in &base[1..] {
                if tot(e) > tot(best) {
                    best = e;
                }
            }
            intervals = intervals_of(best.get("intervals").unwrap_or(&Value::Null));
            total = tot(best);
        }
        let timed = total >= 0.05;
        let cooldowns: Vec<Value> = ab.uses.iter().filter_map(|u| u.2.filter(|c| *c != 0.0).map(|c| json!([rnd(min_f(u.1, dur), 3), rnd(min_f(c, dur), 3)]))).collect();
        lanes.push(obj(json!({
            "group": "Abilities", "name": ab.name, "kind": "ability", "indent": 0, "intervals": intervals, "timed": timed,
            "cooldowns": cooldowns, "total": timed.then_some(total), "uptime": timed.then(|| min_f(1.0, total / dur)),
            "applications": if ab.uses.is_empty() { intervals.len() } else { ab.uses.len() },
        })));
        for (e, perk) in mine {
            let Some(perk) = perk else { continue }; // the ability itself: already on its own bar
            let mut row = e.clone();
            row.insert("kind".into(), json!("effect"));
            row.insert("source".into(), json!(ab.name));
            if internal(e) {
                row.insert("name".into(), json!(format!("{}: {perk}", ab.name)));
                row.insert("group".into(), json!("Other effects"));
                row.insert("indent".into(), json!(0));
                other.push(row);
            } else {
                row.insert("name".into(), json!(perk));
                row.insert("group".into(), json!("Abilities"));
                row.insert("indent".into(), json!(1));
                lanes.push(row);
            }
        }
    }
    for (e, own) in rest {
        let mut group = match e.get("kind").and_then(Value::as_str) { Some("Buff") => "Buffs", Some("Debuff") => "Debuffs", _ => "Other effects" };
        if internal(&e) {
            group = "Other effects";
        }
        let src = match own.as_ref().map(|o| o.0) { Some("skill") => "Skill", Some("mod") => "Mod", Some("ability") => "Ability", Some("ally") => "Ally", _ => "" };
        let given = e.get("source").and_then(Value::as_str).unwrap_or("").to_string();
        let source = if !given.is_empty() { given } else if !src.is_empty() { format!("{src}: {}", own.as_ref().unwrap().1) } else { String::new() };
        let mut row = e;
        row.insert("group".into(), json!(group));
        row.insert("indent".into(), json!(0));
        row.insert("kind".into(), json!("effect"));
        row.insert("source".into(), json!(source));
        other.push(row);
    }
    let order = |g: &str| match g { "Abilities" => 0, "Buffs" => 1, "Debuffs" => 2, _ => 3 };
    let key = |l: &Map<String, Value>| (order(l["group"].as_str().unwrap_or("")), -l.get("uptime").and_then(Value::as_f64).unwrap_or(0.0));
    other.sort_by(|a, b| key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal));
    lanes.extend(other);
    // one row per (group, level, name): several sets can share a name (a set per rank, a set per damage type)
    let mut out: Vec<Map<String, Value>> = Vec::new();
    let mut seen: HashMap<(String, i64, String, String), usize> = HashMap::new();
    for ln in lanes {
        let s = |k: &str| ln.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        if s("kind") == "ability" {
            out.push(ln);
            continue;
        }
        let k = (s("group"), ln["indent"].as_i64().unwrap_or(0), s("name"), s("source"));
        match seen.get(&k) {
            None => {
                seen.insert(k, out.len());
                out.push(ln);
            }
            Some(&at) => {
                let prev = &mut out[at];
                let mut ivs = intervals_of(&prev["intervals"]);
                ivs.extend(intervals_of(&ln["intervals"]));
                let merged = merge_intervals(ivs);
                let total: f64 = merged.iter().map(|iv| iv[1] - iv[0]).sum();
                let mut types: BTreeSet<u64> = prev.get("types").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).collect();
                types.extend(ln.get("types").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64));
                prev.insert("applications".into(), json!(merged.len()));
                prev.insert("intervals".into(), json!(merged));
                prev.insert("total".into(), json!(total));
                prev.insert("uptime".into(), json!(min_f(1.0, total / dur)));
                prev.insert("types".into(), json!(types));
            }
        }
    }
    out.into_iter().map(Value::Object).collect()
}

/// Identity of a damage source: attack type + the definition's identity hash (the index is per mission).
fn attack_key(h: &Hit) -> String {
    let (idx, hash) = h.ad;
    if idx == u32::MAX {
        // every bit set: the game's "no definition" id (WorldLoadDataId.Invalid)
        "none".into()
    } else if hash != 0 {
        format!("{}:{hash:08x}", h.at)
    } else {
        format!("{}:i{idx}", h.at)
    }
}

/// {name, kind} of a source key when no definition names it.
pub fn raw_attack(key: &str) -> Value {
    if key == "none" {
        return json!({ "name": "Unattributed", "kind": "environment" });
    }
    if let Some(rest) = key.strip_prefix("type:") {
        let (dt, how) = rest.split_once(':').unwrap_or((rest, ""));
        let name = dt.parse::<usize>().ok().and_then(|i| consts::get().damage_type(i).map(str::to_string)).unwrap_or_else(|| format!("Type {dt}"));
        let tail = match how { "1" => " damage over time", "2" => " explosion", _ => " attack" };
        return json!({ "name": name + tail, "kind": "enemy" });
    }
    let (at, a) = key.split_once(':').unwrap_or((key, ""));
    let kind = at.parse::<usize>().map(|i| consts::get().attack_kind(i)).unwrap_or_else(|_| "?".into());
    json!({ "name": format!("{kind} {a}"), "kind": kind })
}

impl Encounter {
    fn new(match_id: Option<String>, wall: f64, c: Arc<Consts>) -> Encounter {
        Encounter {
            c, match_id, id: local_stamp(wall), start_wall: wall, end_wall: None, t0: None, t_last: 0.0, first_hit: None,
            last_hit: None, players: IndexMap::new(), friendly: HashSet::new(), ent_client: HashMap::new(), local_cid: None,
            mission: json!({}), result: None, party_series: Default::default(),
            targets: Default::default(), hits: Vec::new(), server_hits: 0, predicted_hits: 0, other_hits: 0, enemy_hits: 0, blast_hits: 0, late: 0, dirty: true, closed: false, pending_builds: None,
            pending_hits: Vec::new(), flushing: false, pending_heals: Vec::new(), bosses: IndexMap::new(), bossmods: Vec::new(),
            prop_tag: HashMap::new(), tags: Vec::new(), enemymods: Vec::new(), defense: None, seen_res: HashMap::new(),
            ended: false, world_first: None, world_last: 0.0, game_time: None, outcome: None, xp: None, res0: None,
            result_at: None, result_settled: false,
        }
    }

    fn rel(&self, t: f64) -> f64 { t - self.t0.unwrap_or(t) }

    // ---- records ----
    fn on_frame(&mut self, f: &Frame) {
        if self.world_first.is_none() {
            self.world_first = Some(f.t);
        }
        self.world_last = max_f(self.world_last, f.t);
        if self.ended {
            return;
        }
        if self.t0.is_none() && f.ws == self.c.ws_ingame {
            self.t0 = Some(f.t);
        }
        if let Some(t0) = self.t0 {
            self.t_last = max_f(self.t_last, f.t - t0);
        }
        self.local_cid = Some(f.cid);
    }

    fn end(&mut self, result: &'static str) {
        if !self.ended {
            self.ended = true;
            self.result = Some(result);
            self.dirty = true;
        }
    }

    /// Result data of the game. Fresh when its time differs from what was there when the encounter started and lies
    /// at the end of this world's clock. (Seen live: the game writes it when the world state turns
    /// GoToPostGameScreen, while the client is still in GameEnded; `valid` is cleared again once the result screen
    /// has consumed it, so it is not a usable freshness test.)
    ///
    /// The game writes the result in one routine, the time early and the experience gained late, and a read can
    /// land in between. So the time and outcome are taken when they first show, and the answer is "the result is
    /// in" only once it has been read again a moment later (`RESULT_SETTLE`), which is when the experience is taken.
    fn on_result(&mut self, res: Option<GameResult>, wall: f64) -> bool {
        let Some(res) = res else { return false };
        let Some(res0) = self.res0 else {
            self.res0 = Some(res);
            return false;
        };
        let (_valid, t, objective, _leave, xp) = res;
        if self.game_time.is_none() && t > 0.0 && t != res0.1 && (t - self.world_last).abs() < 60.0 {
            self.game_time = Some(t);
            self.outcome = match objective { 1 => Some("won"), 2 => Some("lost"), _ => None };
            self.result_at = Some(wall);
            self.dirty = true;
            return false;
        }
        if let (false, Some(at), Some(gt)) = (self.result_settled, self.result_at, self.game_time) {
            if wall - at >= RESULT_SETTLE && t == gt {
                self.result_settled = true;
                self.xp = (xp > 0).then_some(xp);
                self.dirty = true;
                return true;
            }
        }
        false
    }

    fn on_sample(&mut self, s: &Sample, r: &Resolver) {
        if self.ended {
            return;
        }
        for c in &s.clients {
            let local = Some(c.id) == self.local_cid;
            let p = self.players.entry(c.id).or_insert_with(|| Player::new(c.id));
            if !c.name.is_empty() {
                p.name = c.name.clone();
            }
            let (cls, sub) = (r.class_name(&c.cls), r.class_name(&c.sub));
            if !cls.is_empty() {
                p.cls = cls;
            }
            if !sub.is_empty() {
                p.sub = sub;
            }
            if c.lvl != 0 {
                p.lvl = c.lvl;
            }
            p.host = c.host != 0;
            p.local = local;
            p.kills_native = c.kills;
            p.downs = c.downs;
            p.accuracy = Some(c.acc);
        }
        if let Some(e) = &s.ents {
            let c = self.c.clone();
            let (t0, rel_t) = (self.t0, self.rel(s.t));
            for bd in &e.bosses {
                let b = self.bosses.entry(bd.id).or_insert_with(|| new_boss(bd.hash));
                b.spawn = bd.spawn;
                b.death = bd.death;
                b.hp_max = max_f(b.hp_max, bd.hpmax + bd.shmax);
                if bd.mods.is_some() {
                    b.mods = bd.mods;
                }
                if let Some(t0) = t0 {
                    b.hp_series.insert(bucket(rel_t), (rnd(bd.hp, 1), rnd(bd.sh, 1)));
                    effects(&c, &mut b.fx, &bd.fx, rel_t, t0, true);
                }
            }
            for (_eid, hash, tag, _prop, _hpmax) in e.props.iter().flatten() {
                if *hash != 0 && *tag > 0 {
                    self.prop_tag.entry(*hash).or_insert(*tag);
                }
            }
            for pe in &e.players {
                self.friendly.insert(pe.id);
                self.ent_client.insert(pe.id, pe.client);
                let Some(p) = self.players.get_mut(&pe.client) else { continue };
                p.ents.insert(pe.id);
                if pe.dmg.is_some() {
                    p.native_total = pe.dmg;
                }
                for (i, w) in pe.wp.iter().enumerate() {
                    p.weapon_slot.insert(w.0, i);
                }
                if let Some(t0) = t0 {
                    p.hp_series.insert(bucket(rel_t), (rnd(pe.hp, 1), rnd(pe.sh, 1)));
                    p.hp_max = max_f(p.hp_max, pe.hpmax + pe.shmax);
                    effects(&c, &mut p.fx, &pe.fx, rel_t, t0, false);
                    abilities(&c, p, &pe.ab, rel_t, t0);
                }
            }
        }
        if !self.pending_hits.is_empty() && !self.friendly.is_empty() {
            self.flush(r, Some(s.t));
            for (src, tgt, hp) in std::mem::take(&mut self.pending_heals) {
                self.on_heal(src, tgt, hp);
            }
        }
        self.dirty = true;
    }

    /// Count the hits that were held back. With `now` (world time), a hit whose source is still not known stays
    /// held until it has waited `UNKNOWN_HOLD`; without it (the encounter is closing) everything is counted.
    fn flush(&mut self, r: &Resolver, now: Option<f64>) {
        let held = std::mem::take(&mut self.pending_hits);
        let mut keep = Vec::new();
        self.flushing = true;
        for h in held {
            let wait = now.is_some_and(|t| t - h.t < UNKNOWN_HOLD) && self.players.contains_key(&h.ac)
                && r.attacker(h.at, h.ad.1) == Attacker::Unknown;
            if wait {
                keep.push(h);
            } else {
                self.on_hit(&h, r);
            }
        }
        self.flushing = false;
        self.pending_hits.extend(keep);
    }

    fn on_hit(&mut self, h: &Hit, r: &Resolver) {
        let c = self.c.clone();
        if h.src != c.src_server {
            self.predicted_hits += 1;
            return;
        }
        if self.friendly.is_empty() || (!self.players.contains_key(&h.ac) && self.pending_hits.len() < 20000 && !self.flushing) {
            // players / player entities not known yet (first 0.1 s of a mission, or someone just joined):
            // decide after the next state sample
            self.pending_hits.push(h.clone());
            return;
        }
        if h.late.is_some() {
            self.late += 1;
        }
        let mut t = self.rel(h.t);
        if self.ended {
            t = min_f(t, self.t_last); // damage dealt during the outro lands in the last second of the log
        }
        let dmg = h.dh + h.ds;
        if self.friendly.contains(&h.tgt) {
            // damage taken by a player
            if let Some(p) = self.ent_client.get(&h.tgt).and_then(|c| self.players.get_mut(c)) {
                if h.fl & c.hit_dodge == 0 {
                    p.taken += dmg;
                    p.taken_hits += 1;
                    let mut key = attack_key(h);
                    if key == "none" {
                        key = format!("type:{}:{}", h.dt, if h.fl & c.hit_dot != 0 { 1 } else if h.fl & c.hit_explosion != 0 { 2 } else { 0 });
                    }
                    let e = p.taken_by.entry(key).or_insert((0.0, 0));
                    e.0 += dmg;
                    e.1 += 1;
                    p.taken_series.add(t, dmg);
                }
            }
            self.other_hits += 1;
            return;
        }
        if !self.players.contains_key(&h.ac) {
            self.other_hits += 1;
            return;
        }
        // Damage an enemy did. The game attributes it to a player (that is how it credits the kills), but it is not
        // damage the player dealt. The blast of an enemy that explodes is kept, apart; anything else (a Multiplier
        // clawing a turret) is not counted at all.
        let blast = match r.attacker(h.at, h.ad.1) {
            Attacker::Enemy if h.fl & c.hit_explosion != 0 => true,
            Attacker::Enemy => {
                self.enemy_hits += 1;
                return;
            }
            // not in the mission's definition tables yet (they are being read again): decide when it is
            Attacker::Unknown if !self.flushing && !self.ended && self.pending_hits.len() < 20000 => {
                self.pending_hits.push(h.clone());
                return;
            }
            _ => false,
        };
        if blast {
            self.blast_hits += 1;
        } else {
            self.server_hits += 1;
        }
        let key = attack_key(h);
        let tkey = if h.tdh != 0 { format!("{:08x}", h.tdh) } else { format!("i{}", h.tdi) };
        // a "final blow" on a prop (pod shell, barrel, crate) is not a kill: count enemies only
        let enemy = r.is_enemy(h.tdh);
        let kill = h.fl & c.hit_final != 0 && enemy;
        let p = self.players.get_mut(&h.ac).unwrap();
        let cid = p.cid;
        // a structure: the target is known not to be an enemy
        for counted in Counted::ALL.into_iter().filter(|counted| counted.takes(!enemy, blast)) {
            let f = &mut p.fig[counted.index()];
            f.total.add(&c, t, h, dmg, kill);
            let src = f.sources.entry(key.clone()).or_default();
            src.0.add(&c, t, h, dmg, kill);
            src.1.add(t, dmg);
            f.series.add(t, dmg);
            self.party_series[counted.index()].add(t, dmg);
        }
        for with_blasts in [false, true] {
            if !blast || with_blasts {
                p.targets[with_blasts as usize].entry(tkey.clone()).or_default().add(&c, t, h, dmg, kill);
                self.targets[with_blasts as usize].entry(tkey.clone()).or_default().add(&c, t, h, dmg, kill);
            }
        }
        seen_add(self.seen_res.entry(tkey.clone()).or_default(), h);
        // the view of a boss is what the players did to it
        if !blast && (self.bosses.contains_key(&h.tgt) || r.is_boss(h.tdh)) {
            let b = self.bosses.entry(h.tgt).or_insert_with(|| new_boss(h.tdh));
            b.series.entry(cid).or_default().add(t, dmg);
            b.agg.add(&c, t, h, dmg, kill);
            seen_add(&mut b.seen, h);
            b.damage += dmg;
            if b.first_hit.is_none() {
                b.first_hit = Some(t);
            }
            b.last_hit = Some(t);
            if kill {
                b.killed_at = Some(t);
            }
        }
        if dmg > 0.0 {
            // a hit that was held back is counted after later ones
            self.first_hit = Some(self.first_hit.map_or(t, |f| min_f(f, t)));
            self.last_hit = Some(self.last_hit.map_or(t, |l| max_f(l, t)));
        }
        self.hits.push((rnd(t, 3), h.ac, key, tkey, rnd(h.dh, 2), rnd(h.ds, 2), h.fl, h.dt, h.tgt));
        self.dirty = true;
    }

    fn on_heal(&mut self, src: u8, tgt: u32, hp: f64) {
        if src != self.c.src_server {
            return;
        }
        if self.friendly.is_empty() {
            self.pending_heals.push((src, tgt, hp));
            return;
        }
        if let Some(p) = self.ent_client.get(&tgt).and_then(|c| self.players.get_mut(c)) {
            p.healed += hp;
        }
    }

    // ---- output ----
    /// Mission time in seconds: InGame start until the mission ended (or now, while it is running).
    pub fn duration(&self) -> f64 { self.t_last }

    /// What the players dealt between them, for one choice of what is counted.
    pub fn party_damage(&self, counted: Counted) -> f64 {
        self.players.values().map(|p| p.fig[counted.index()].total.dmg).sum()
    }

    fn stats(&self) -> Value {
        json!({ "server_hits": self.server_hits, "predicted_hits": self.predicted_hits, "other_hits": self.other_hits,
                "enemy_hits": self.enemy_hits, "blast_hits": self.blast_hits, "late": self.late })
    }

    fn tag_name(&self, key: &str) -> String {
        let tag = u32::from_str_radix(key, 16).ok().and_then(|h| self.prop_tag.get(&h)).copied();
        match tag {
            Some(t) if t >= 0 && (t as usize) < self.tags.len() => self.tags[t as usize].clone(),
            _ => String::new(),
        }
    }

    /// Effect rows of a player or boss. span = (start, end) the uptime is measured over (default: the mission).
    fn buffs(&self, fx: &Fx, span: Option<(f64, f64)>, r: &Resolver) -> Vec<Map<String, Value>> {
        let mut keys: IndexMap<&str, Vec<[f64; 2]>> = IndexMap::new();
        for (key, ivs) in &fx.done {
            keys.entry(key.as_str()).or_default().extend(ivs.iter().copied());
        }
        for (key, a, z, _lvl) in fx.live.values() {
            keys.entry(key.as_str()).or_default().push([*a, if self.closed { *z } else { max_f(*z, self.t_last) }]);
        }
        let dur = max_f(span.map_or(self.t_last, |s| s.1 - s.0), 1e-6);
        let mut out = Vec::new();
        for (key, ivs) in keys {
            let merged = merge_intervals(ivs);
            let total: f64 = merged.iter().map(|iv| iv[1] - iv[0]).sum();
            let meta = fx.meta.get(key).cloned().unwrap_or_default();
            let (name, kind, asset) = r.effect(key);
            out.push(obj(json!({
                "key": key, "intervals": merged.iter().map(|iv| [rnd(iv[0], 2), rnd(iv[1], 2)]).collect::<Vec<_>>(), "total": total,
                "uptime": min_f(1.0, total / dur), "applications": merged.len(), "types": meta.types, "owner": meta.owner,
                "by": meta.by, "giver": meta.giver, "name": name, "kind": kind, "asset": asset,
            })));
        }
        let order = |b: &Map<String, Value>| (match b["kind"].as_str() { Some("Buff") => 0, Some("Debuff") => 1, _ => 2 }, -b["uptime"].as_f64().unwrap_or(0.0));
        out.sort_by(|a, b| order(a).partial_cmp(&order(b)).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    /// Timeline rows for one player: see group_lanes.
    fn lanes(&self, p: &Player, r: &Resolver) -> Vec<Value> {
        let abilities: Vec<AbLane> = p.abil_hash.iter().map(|(slot, hash)| {
            let (name, asset) = r.ability(*slot, *hash);
            let mut uses: Vec<(f64, f64, Option<f64>)> = p.abil.get(slot).map(|u| u.values().copied().collect()).unwrap_or_default();
            uses.sort_by(|a, b| (a.0, a.1).partial_cmp(&(b.0, b.1)).unwrap_or(std::cmp::Ordering::Equal));
            AbLane { name, asset, hash: *hash, uses }
        }).collect();
        let raw = |q: &Player, k: &str| q.build.as_ref().map(|b| b["raw"][k].clone()).unwrap_or(Value::Null);
        let strs = |v: Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect() };
        let ctx = OwnerCtx {
            allies: self.players.values().filter(|q| q.cid != p.cid).flat_map(|q| strs(raw(q, "abilities"))).filter(|a| !a.is_empty()).collect(),
            abilities: abilities.iter().filter(|a| !a.asset.is_empty()).map(|a| a.asset.clone()).collect(),
            skills: raw(p, "skills").as_array().into_iter().flatten()
                .filter(|s| s[1].as_i64().unwrap_or(0) != 0).filter_map(|s| s[0].as_str().map(str::to_string)).collect(),
            by_hash: abilities.iter().map(|a| (a.hash, (a.asset.as_str(), a.name.as_str()))).collect(),
            others: self.players.len().saturating_sub(1),
            ally_builds: self.players.values().any(|q| q.cid != p.cid && q.build.is_some()),
        };
        let owner_of = |e: &Value| r.effect_owner(e, &ctx);
        let mut effects = self.buffs(&p.fx, None, r);
        for e in &mut effects {
            // who the game names as the giver: the player's own entity, a teammate's, something else, or nobody
            let giver = e.get("giver").and_then(Value::as_u64).unwrap_or(0) as u32;
            let from = if giver == 0 { "" } else if p.ents.contains(&giver) { "self" } else if self.friendly.contains(&giver) { "ally" } else { "other" };
            e.insert("from".into(), json!(from));
        }
        group_lanes(&abilities, effects, max_f(self.t_last, 1e-6), Some(&owner_of))
    }

    /// (first, last) mission time the boss existed: spawn to death when the game says so, else as observed.
    fn boss_span(&self, b: &Boss) -> (f64, f64) {
        let first_seen = b.hp_series.keys().min().map(|i| *i as f64 * BUCKET);
        let a = if b.spawn > 0.0 { self.rel(b.spawn) } else { first_seen.or(b.first_hit).unwrap_or(0.0) };
        let z = if b.death > 0.0 { self.rel(b.death) } else { b.killed_at.unwrap_or(self.t_last) };
        let a = max_f(0.0, min_f(a, self.t_last));
        (a, max_f(a, min_f(z, self.t_last)))
    }

    fn boss_name(&self, b: &Boss, r: &Resolver) -> String {
        let n = r.entity(&format!("{:08x}", b.hash))["name"].as_str().unwrap_or("").to_string();
        if n.is_empty() { "Boss".into() } else { n }
    }

    /// Things worth a marker on the time axis: a boss appearing and dying.
    fn events(&self, r: &Resolver) -> Vec<Value> {
        let mut ids: Vec<&u32> = self.bosses.keys().collect();
        ids.sort();
        let mut out: Vec<(f64, Value)> = Vec::new();
        for id in ids {
            let b = &self.bosses[id];
            let name = self.boss_name(b, r);
            let (a, z) = self.boss_span(b);
            if b.spawn > 0.0 && a > 0.5 {
                out.push((rnd(a, 3), json!({ "t": rnd(a, 3), "kind": "boss", "label": format!("{name} appears") })));
            }
            if b.death > 0.0 || b.killed_at.is_some() {
                out.push((rnd(z, 3), json!({ "t": rnd(z, 3), "kind": "boss", "label": format!("{name} killed") })));
            }
        }
        out.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal));
        out.into_iter().map(|x| x.1).collect()
    }

    /// Per-boss view: damage over time per player, health over time, the effects on it and who applied them.
    fn bosses_json(&self, n: i64, r: &Resolver) -> Vec<Value> {
        let mut list: Vec<(&u32, &Boss)> = self.bosses.iter().collect();
        list.sort_by(|a, b| b.1.damage.partial_cmp(&a.1.damage).unwrap_or(std::cmp::Ordering::Equal));
        let mut out = Vec::new();
        for (eid, b) in list {
            if b.damage == 0.0 && b.hp_series.is_empty() {
                continue;
            }
            let (a, z) = self.boss_span(b);
            let mut effects = self.buffs(&b.fx, Some((a, z)), r);
            for e in &mut effects {
                let by = e["by"].as_str().unwrap_or("").to_string();
                if !by.is_empty() {
                    e.insert("source".into(), r.attack(&by)["name"].clone());
                }
            }
            let lanes = group_lanes(&[], effects, max_f(z - a, 1e-6), None);
            let mods: Vec<String> = match b.mods {
                Some((mask, _, _)) => (0..self.bossmods.len().min(32)).filter(|i| mask >> i & 1 != 0 && !self.bossmods[*i].is_empty())
                    .map(|i| self.bossmods[i].clone()).collect(),
                None => Vec::new(),
            };
            let mut name = self.boss_name(b, r);
            for m in &mods {
                name = if m.contains("{0}") { m.replace("{0}", &name) } else { format!("{m} {name}") };
            }
            let hash = format!("{:08x}", b.hash);
            out.push(json!({
                "id": eid, "name": name, "start": rnd(a, 3), "end": rnd(z, 3), "killed": b.death > 0.0 || b.killed_at.is_some(),
                "hp_max": b.hp_max, "damage": b.damage, "mods": mods, "stage": b.mods.map(|m| m.1),
                "first_hit": b.first_hit, "last_hit": b.last_hit, "hash": hash,
                "defense": self.defense_of(&[hash.clone()], Some(&b.seen), true, r),
                "mitigated": b.agg.mitigated(), "pre": rnd(b.agg.pre, 1), "types": b.agg.types_json(),
                "series": b.series.iter().map(|(cid, s)| (cid.to_string(), json!(s.dense(n)))).collect::<Map<String, Value>>(),
                "hp_series": (0..n).map(|i| b.hp_series.get(&i).map(|v| json!([v.0, v.1])).unwrap_or(Value::Null)).collect::<Vec<_>>(),
                "lanes": lanes,
            }));
        }
        out
    }

    /// Target rows merged by display name (enemy stage variants are separate definitions with one name).
    fn targets_json(&self, table: &IndexMap<String, Agg>, r: &Resolver) -> Vec<Value> {
        let mut rows: IndexMap<String, (Agg, Map<String, Value>, Vec<String>)> = IndexMap::new();
        for (key, a) in table {
            let mut info = obj(r.entity(key));
            let tag = self.tag_name(key);
            let kind = info.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            if !tag.is_empty() && matches!(kind.as_str(), "Destructible" | "") {
                // the game tags the props an objective cares about; in Pods the one tagged "Boss" is the boss pod
                let pod = tag.eq_ignore_ascii_case("boss") && self.mission["mode_id"].as_str().unwrap_or("").eq_ignore_ascii_case("modepods");
                let name = if pod { "Boss pod".to_string() } else { format!("{} ({tag})", info["name"].as_str().unwrap_or("")) };
                info.insert("name".into(), json!(name));
                info.insert("tag".into(), json!(tag));
            }
            let name = info["name"].as_str().unwrap_or("").to_string();
            match rows.get_mut(&name) {
                Some(row) => {
                    row.0.merge(a);
                    row.2.push(key.clone());
                }
                None => {
                    info.insert("key".into(), json!(key));
                    rows.insert(name, (a.clone(), info, vec![key.clone()]));
                }
            }
        }
        let mut list: Vec<(Agg, Map<String, Value>, Vec<String>)> = rows.into_values().collect();
        list.sort_by(|a, b| b.0.dmg.partial_cmp(&a.0.dmg).unwrap_or(std::cmp::Ordering::Equal));
        list.into_iter().map(|(a, info, keys)| {
            let mut d = a.json();
            let boss = info.get("boss").and_then(Value::as_bool).unwrap_or(false);
            // anything the game types as something other than an enemy: a crate, a pod, a barrel, level geometry
            let structure = info.get("type").and_then(Value::as_str).is_some_and(|t| !t.is_empty() && t != self.c.entity_type(self.c.et_enemy));
            d.insert("structure".into(), json!(structure));
            d.extend(info);
            let mut seen: Seen = BTreeMap::new();
            for k in &keys {
                for (dt, e) in self.seen_res.get(k).into_iter().flatten() {
                    let cur = seen.entry(*dt).or_insert([None; 4]);
                    for i in 0..4 {
                        if let Some(v) = e[i] {
                            cur[i] = Some(cur[i].map_or(v, |c| if i % 2 == 0 { min_f(c, v) } else { max_f(c, v) }));
                        }
                    }
                }
            }
            d.insert("defense".into(), self.defense_of(&keys, Some(&seen), boss, r));
            Value::Object(d)
        }).collect()
    }

    /// What a target can take, from the world's own tables: health, shield, resistance per damage type (health and
    /// shield pool), explosion resistance, dodge, which status effects it shrugs off. `keys` are the entity
    /// definitions shown as one row (stage and elite variants of one enemy), so values are (lowest, highest).
    /// `seen` adds the resistances the game actually applied on hits where they differ (debuffs on the target).
    fn defense_of(&self, keys: &[String], seen: Option<&Seen>, boss: bool, r: &Resolver) -> Value {
        let d = self.defense.as_ref();
        let profs: Vec<&EntDefense> = keys.iter().filter_map(|k| d?.ents.get(&u32::from_str_radix(k, 16).ok()?)).collect();
        let mut out = obj(json!({ "source": if profs.is_empty() { "" } else { "game" }, "variants": profs.len() }));
        let span = |vals: Vec<f64>| json!([vals.iter().copied().fold(f64::INFINITY, min_f), vals.iter().copied().fold(f64::NEG_INFINITY, max_f)]);
        if let (false, Some(d)) = (profs.is_empty(), d) {
            // one resistance per damage type; the tables say how many there are
            let ntypes = d.res.iter().map(Vec::len).max().unwrap_or(0);
            let no_res = vec![0.0; ntypes];
            let res_of = |i: i32| usize::try_from(i).ok().and_then(|i| d.res.get(i)).unwrap_or(&no_res);
            out.insert("hp".into(), span(profs.iter().map(|p| p.hp).collect()));
            out.insert("shield".into(), span(profs.iter().map(|p| p.shield).collect()));
            for (name, shield) in [("resist", false), ("shield_resist", true)] {
                let tables: Vec<&Vec<f64>> = profs.iter().filter(|p| !shield || p.shield > 0.0)
                    .map(|p| res_of(if shield { p.shield_res } else { p.res })).collect();
                let rows: Vec<Value> = (0..ntypes).filter(|t| tables.iter().any(|tb| tb[*t] != 0.0)).map(|t| {
                    let s = span(tables.iter().map(|tb| tb[t]).collect());
                    json!([r.damage_type_name(self.c.damage_type(t).unwrap_or("")), s[0], s[1]])
                }).collect();
                out.insert(name.into(), json!(rows));
            }
            let fields: [(&str, fn(&EntDefense) -> f64); 4] = [("explosion", |p| p.explosion), ("dodge", |p| p.dodge),
                ("elemental", |p| p.elemental), ("regen", |p| p.regen)];
            for (name, get) in fields {
                let vals: Vec<f64> = profs.iter().map(|p| get(p)).collect();
                if vals.iter().any(|v| *v != 0.0) {
                    out.insert(name.into(), span(vals));
                }
            }
            let no_eres = vec![0.0; d.eres.iter().map(Vec::len).max().unwrap_or(0)];
            let cats: Vec<&Vec<f64>> = profs.iter().map(|p| usize::try_from(p.effect_res).ok().and_then(|i| d.eres.get(i)).unwrap_or(&no_eres)).collect();
            let immune = |c: usize| cats.iter().all(|e| e[2 * c] >= 0.999);
            // one (apply chance, strength) pair per effect category; category 0 is "none"
            let ncat = cats.iter().map(|e| e.len() / 2).min().unwrap_or(0);
            let cat_name = |c: usize| self.c.effect_categories.get(c).filter(|n| !n.is_empty()).map(|n| r.effect_category_name(n)).unwrap_or_else(|| format!("Category {c}"));
            out.insert("immune".into(), json!((1..ncat).filter(|c| immune(*c)).map(cat_name).collect::<Vec<_>>()));
            out.insert("weaker".into(), json!((1..ncat).filter(|c| cats.iter().any(|e| e[2 * c + 1] != 0.0) && !immune(*c)).map(|c| {
                let s = span(cats.iter().map(|e| e[2 * c + 1]).collect());
                json!([cat_name(c), s[0], s[1]])
            }).collect::<Vec<_>>()));
            let names = if boss { &self.bossmods } else { &self.enemymods };
            let mask = profs.iter().fold(0u32, |m, p| m | p.mods);
            out.insert("mods".into(), json!((0..names.len().min(32)).filter(|i| mask >> i & 1 != 0 && !names[*i].is_empty()).map(|i| &names[i]).collect::<Vec<_>>()));
            out.insert("stage".into(), span(profs.iter().map(|p| p.stage as f64).collect()));
            out.insert("elite".into(), json!(if boss { 0 } else { profs.iter().filter(|p| p.mods != 0).count() }));
        }
        if let Some(seen) = seen.filter(|s| !s.is_empty()) {
            let table = |name: &str| -> HashMap<String, (f64, f64)> {
                out.get(name).and_then(Value::as_array).into_iter().flatten()
                    .filter_map(|r| Some((r[0].as_str()?.to_string(), (r[1].as_f64()?, r[2].as_f64()?)))).collect()
            };
            let (base, sbase) = (table("resist"), table("shield_resist"));
            let mut obs = Vec::new();
            for (dt, e) in seen {
                let Some(name) = self.c.damage_type(*dt as usize).map(|m| r.damage_type_name(m)) else { continue };
                for (pool, lo, hi, reference) in [("health", e[0], e[1], &base), ("shield", e[2], e[3], &sbase)] {
                    let (Some(lo), Some(hi)) = (lo, hi) else { continue };
                    let (blo, bhi) = reference.get(&name).copied().unwrap_or((0.0, 0.0));
                    let differs = if profs.is_empty() { lo != 0.0 || hi != 0.0 } else { lo < blo - 0.005 || hi > bhi + 0.005 };
                    if differs {
                        obs.push(json!([name, pool, rnd(lo, 4), rnd(hi, 4)]));
                    }
                }
            }
            out.insert("observed".into(), json!(obs));
        }
        Value::Object(out)
    }

    /// One row per player for the live view, for one choice of what is counted: (mission time, party damage, rows).
    fn live_rows(&self, counted: Counted) -> (f64, f64, Vec<Value>) {
        let ct = max_f(self.duration(), 1.0);
        let party = self.party_damage(counted);
        let widest = Counted { structures: true, blasts: true }.index();
        let mut rows: Vec<(f64, Value)> = self.players.values().filter(|p| p.fig[widest].total.hits != 0 || !p.name.is_empty()).map(|p| {
            let a = &p.fig[counted.index()].total;
            (a.dmg, json!({
                "cid": p.cid, "name": p.name, "cls": p.cls, "sub": p.sub, "local": p.local, "damage": a.dmg,
                "dps": a.dmg / ct, "share": if party != 0.0 { a.dmg / party } else { 0.0 },
                "crit": if a.hits != 0 { a.crits as f64 / a.hits as f64 } else { 0.0 }, "hits": a.hits,
                "taken": p.taken, "downs": p.downs,
            }))
        }).collect();
        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        (ct, party, rows.into_iter().map(|r| r.1).collect())
    }

    /// The figures of every other choice of what is counted, by `Counted::key`, in the shape `view` swaps in.
    fn views(make: impl Fn(Counted) -> Value) -> Value {
        Value::Object(Counted::ALL[1..].iter().map(|counted| (counted.key().to_string(), make(*counted))).collect())
    }

    /// For a list row or a live payload: the party's totals and, per client id, the picked fields of a player's row.
    fn row_views(&self, ct: f64, pick: &[&str]) -> Value {
        Self::views(|counted| {
            let (_, party, rows) = self.live_rows(counted);
            let players: Map<String, Value> = rows.into_iter().map(|r| {
                (r["cid"].to_string(), Value::Object(pick.iter().map(|k| (k.to_string(), r[*k].clone())).collect()))
            }).collect();
            json!({ "party_damage": party, "party_dps": party / ct, "players": players })
        })
    }

    /// Small payload for the overlay / live view.
    pub fn live(&self) -> Value {
        let (ct, party, rows) = self.live_rows(Counted::default());
        json!({ "id": self.id, "mission": self.mission, "elapsed": self.duration(), "party_damage": party, "party_dps": party / ct,
                "players": rows, "closed": self.closed, "result": self.result, "ended": self.ended, "game_time": self.game_time,
                "outcome": self.outcome, "xp": self.xp,
                "views": self.row_views(ct, &["damage", "dps", "share", "crit", "hits"]) })
    }

    /// False for a world that is not a mission: the hub, and a world whose definitions have not been read yet (its
    /// first moments). Such a world is not shown as a mission and not saved.
    pub fn is_mission(&self) -> bool {
        self.mission.get("name").is_some() && self.mission["hub"].as_bool() != Some(true)
    }

    /// The place's name when this world is the hub ("Aegis Station").
    pub fn hub_name(&self) -> Option<&str> {
        if self.mission["hub"].as_bool() == Some(true) { self.mission["name"].as_str() } else { None }
    }

    /// Row for the encounter list.
    pub fn summary(&self) -> Value {
        let (ct, party, rows) = self.live_rows(Counted::default());
        let players: Vec<Value> = rows.iter().map(|r| json!({ "cid": r["cid"], "name": r["name"], "cls": r["cls"], "sub": r["sub"],
            "damage": r["damage"], "dps": r["dps"], "local": r["local"] })).collect();
        json!({ "id": self.id, "start": self.start_wall, "mission": self.mission, "duration": self.duration(),
                "party_damage": party, "party_dps": party / ct, "result": self.result, "closed": self.closed,
                "game_time": self.game_time, "outcome": self.outcome, "xp": self.xp, "format": FORMAT, "players": players,
                "views": self.row_views(ct, &["damage", "dps"]) })
    }

    /// The damage figures of one player for one choice of what is counted: the totals, the rate, the share of the
    /// party's damage, the sources and the damage over time.
    fn damage_json(&self, p: &Player, counted: Counted, party: f64, ct: f64, n: i64, r: &Resolver) -> Map<String, Value> {
        let Figures { total, sources, series } = &p.fig[counted.index()];
        // sources with one name that are not equipped weapons are one row: a turret is a separate definition per
        // deployment slot, a barrel per level, and so on
        let mut rows: IndexMap<String, (Agg, Series, Map<String, Value>)> = IndexMap::new();
        for (key, (a, series)) in sources {
            let info = obj(r.attack(key));
            let kind = info.get("kind").and_then(Value::as_str).unwrap_or("");
            let gk = if kind == "weapon" { format!("k:{key}") } else { format!("n:{}\u{0}{kind}", info["name"].as_str().unwrap_or("")) };
            match rows.get_mut(&gk) {
                Some(row) => {
                    row.0.merge(a);
                    row.1.merge(series);
                }
                None => {
                    let mut info = info;
                    info.insert("key".into(), json!(key));
                    rows.insert(gk, (a.clone(), series.clone(), info));
                }
            }
        }
        let mut list: Vec<(Agg, Series, Map<String, Value>)> = rows.into_values().collect();
        list.sort_by(|a, b| b.0.dmg.partial_cmp(&a.0.dmg).unwrap_or(std::cmp::Ordering::Equal));
        let mut srcs: Vec<Map<String, Value>> = list.into_iter().map(|(a, series, info)| {
            let mut d = a.json();
            d.extend(info);
            d.insert("series".into(), series.sparse());
            d
        }).collect();
        // by the game's own attack type: its weapons, and what hits directly (weapons and abilities)
        let by_type = |types: &[u8]| sources.iter().filter(|(k, _)| types.iter().any(|t| k.starts_with(&format!("{t}:")))).map(|(_, v)| v.0.dmg).sum::<f64>();
        let (direct, weapon) = (by_type(&[self.c.at_weapon, self.c.at_ability]), by_type(&[self.c.at_weapon]));
        // two copies of one weapon (dual / swap builds) are separate sources: tell them apart by slot
        let mut same: IndexMap<String, Vec<usize>> = IndexMap::new();
        for (i, d) in srcs.iter().enumerate() {
            if d.get("kind").and_then(Value::as_str) == Some("weapon") {
                same.entry(d["name"].as_str().unwrap_or("").to_string()).or_default().push(i);
            }
        }
        for idx in same.into_values().filter(|v| v.len() > 1) {
            for i in idx {
                let hash = srcs[i]["key"].as_str().and_then(|k| k.split(':').nth(1)).and_then(|h| u32::from_str_radix(h, 16).ok());
                if let Some(slot) = hash.and_then(|h| p.weapon_slot.get(&h)) {
                    let name = format!("{} (weapon {})", srcs[i]["name"].as_str().unwrap_or(""), slot + 1);
                    srcs[i].insert("name".into(), json!(name));
                }
            }
        }
        let mut d = total.json();
        d.extend(obj(json!({
            "dps": total.dmg / ct, "share": if party != 0.0 { total.dmg / party } else { 0.0 }, "sources": srcs,
            "series": series.dense(n), "direct_damage": direct, "weapon_damage": weapon,
        })));
        d
    }

    pub fn detail(&self, r: &Resolver) -> Value {
        let ct = max_f(self.duration(), 1.0);
        let base = Counted::default();
        let party = self.party_damage(base);
        let n = bucket(self.t_last) + 1;
        let mut order: Vec<&Player> = self.players.values().collect();
        order.sort_by(|a, b| b.fig[base.index()].total.dmg.partial_cmp(&a.fig[base.index()].total.dmg).unwrap_or(std::cmp::Ordering::Equal));
        let mut players = Vec::new();
        for p in &order {
            let mut taken: Vec<(f64, Value)> = p.taken_by.iter().map(|(k, v)| {
                let mut d = obj(json!({ "key": k, "damage": v.0, "hits": v.1 }));
                d.extend(obj(r.attack(k)));
                (v.0, Value::Object(d))
            }).collect();
            taken.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut d = self.damage_json(p, base, party, ct, n, r);
            d.extend(obj(json!({
                "cid": p.cid, "name": p.name, "cls": p.cls, "sub": p.sub, "lvl": p.lvl, "local": p.local, "host": p.host,
                "targets": self.targets_json(&p.targets[0], r), "taken": p.taken,
                "taken_hits": p.taken_hits, "taken_by": taken.into_iter().map(|t| t.1).collect::<Vec<_>>(),
                "taken_series": p.taken_series.dense(n), "healed": p.healed, "kills_native": p.kills_native, "downs": p.downs,
                "accuracy": p.accuracy, "native_total": p.native_total, "lanes": self.lanes(p, r), "build": p.build, "hp_max": p.hp_max,
                "hp_series": (0..n).map(|i| p.hp_series.get(&i).map(|v| json!([v.0, v.1])).unwrap_or(Value::Null)).collect::<Vec<_>>(),
            })));
            // what the game's own damage counter is checked against, whatever is shown: it counts structures
            let with_structures = self.damage_json(p, Counted { structures: true, blasts: false }, 0.0, ct, n, r);
            d.insert("counter_direct".into(), with_structures["direct_damage"].clone());
            d.insert("counter_weapon".into(), with_structures["weapon_damage"].clone());
            players.push(Value::Object(d));
        }
        // the target lists with the blasts of enemies in them, made once for the two choices that count those
        let blast_targets = self.targets_json(&self.targets[1], r);
        let blast_player_targets: HashMap<u32, Vec<Value>> = order.iter().map(|p| (p.cid, self.targets_json(&p.targets[1], r))).collect();
        let views = Self::views(|counted| {
            let party = self.party_damage(counted);
            let players: Map<String, Value> = order.iter().map(|p| {
                let mut d = self.damage_json(p, counted, party, ct, n, r);
                if counted.blasts {
                    d.insert("targets".into(), json!(blast_player_targets[&p.cid]));
                }
                (p.cid.to_string(), Value::Object(d))
            }).collect();
            let mut view = obj(json!({ "party_damage": party, "party_dps": party / ct,
                                       "series": self.party_series[counted.index()].dense(n), "players": players }));
            if counted.blasts {
                view.insert("targets".into(), json!(blast_targets));
            }
            Value::Object(view)
        });
        let fresh_world = self.world_first.is_some_and(|w| w < 5.0);
        json!({
            "id": self.id, "match": self.match_id, "start": self.start_wall, "end": self.end_wall, "mission": self.mission,
            "duration": self.duration(), "game_time": self.game_time, "outcome": self.outcome, "xp": self.xp,
            "intro": match (self.t0, self.world_first) { (Some(t0), Some(w)) if fresh_world => Some(t0 - w), _ => None },
            "world_clock": fresh_world.then_some(self.world_last),
            "first_hit": self.first_hit, "last_hit": self.last_hit, "party_damage": party, "party_dps": party / ct,
            "result": self.result, "bucket": BUCKET, "series": self.party_series[base.index()].dense(n), "players": players,
            "targets": self.targets_json(&self.targets[0], r), "stats": self.stats(), "format": FORMAT, "events": self.events(r),
            "bosses": self.bosses_json(n, r), "closed": self.closed, "damage_types": self.c.damage_types,
            "views": views,
        })
    }
}

/// Puts a log, a list row or a live payload into the figures for one choice of what is counted, in place.
///
/// As recorded, the figures count neither damage to structures nor the blasts of enemies, and `views` holds the
/// figures of the other choices (`Counted::key`). The wanted ones are swapped in and the players put back in order
/// of damage. Without structures, their rows are dropped from the target lists, so every list adds up to the
/// totals shown.
///
/// `structures` and `blasts` say what the result is: "on" or "off"; "always" for a log recorded before the two
/// were kept apart (format 7), which has one set of figures with both in them; and for a log of format 8, which
/// kept structures apart and did not record blasts at all, `blasts` is "never".
pub fn view(v: &mut Value, want: Counted) {
    let Some(doc) = v.as_object_mut() else { return };
    let on_off = |on: bool| if on { "on" } else { "off" };
    let (structures, blasts, overlay) = match (doc.remove("views"), doc.remove("with_structures")) {
        (Some(Value::Object(mut views)), _) => (on_off(want.structures), on_off(want.blasts), views.remove(want.key())),
        (None, Some(with_structures)) => (on_off(want.structures), "never", want.structures.then_some(with_structures)),
        _ => ("always", "always", None),
    };
    doc.insert("structures".into(), json!(structures));
    doc.insert("blasts".into(), json!(blasts));
    if let Some(Value::Object(overlay)) = overlay {
        for (key, value) in overlay {
            if key != "players" {
                doc.insert(key, value);
                continue;
            }
            let Value::Object(mut theirs) = value else { continue };
            let rows = doc.get_mut("players").and_then(Value::as_array_mut);
            for p in rows.into_iter().flatten().filter_map(Value::as_object_mut) {
                let cid = p.get("cid").map(Value::to_string).unwrap_or_default();
                if let Some(Value::Object(fields)) = theirs.remove(&cid) {
                    p.extend(fields);
                }
            }
        }
        if let Some(rows) = doc.get_mut("players").and_then(Value::as_array_mut) {
            let damage = |p: &Value| p["damage"].as_f64().unwrap_or(0.0);
            rows.sort_by(|a, b| damage(b).partial_cmp(&damage(a)).unwrap_or(std::cmp::Ordering::Equal));
        }
    }
    if structures == "off" {
        let drop_structures = |rows: Option<&mut Value>| {
            if let Some(rows) = rows.and_then(Value::as_array_mut) {
                rows.retain(|t| t["structure"].as_bool() != Some(true));
            }
        };
        drop_structures(doc.get_mut("targets"));
        for p in doc.get_mut("players").and_then(Value::as_array_mut).into_iter().flatten() {
            drop_structures(p.get_mut("targets"));
        }
    }
}

fn new_boss(hash: u32) -> Boss {
    Boss { hash, spawn: 0.0, death: 0.0, hp_max: 0.0, hp_series: HashMap::new(), series: IndexMap::new(), damage: 0.0,
        first_hit: None, last_hit: None, mods: None, killed_at: None, fx: Fx::default(), agg: Agg::default(), seen: BTreeMap::new() }
}

/// Status effects on an entity: one interval per effect instance, from apply_time until it stops being Applied
/// (state 1, not suspended) or disappears. Effects without a set (a bare "apply status effect") are keyed by their
/// type, plus the ability that applied them when the game attributes one.
fn effects(c: &Consts, fx: &mut Fx, rows: &[Effect], t: f64, t0: f64, any_owner: bool) {
    let mut seen: HashSet<u16> = HashSet::new();
    for &(iid, hash, ty, apply_t, _dur, state, susp, lvl, giver, at, ah) in rows {
        if state != c.fx_applied || susp != 0 {
            continue;
        }
        let owner = if at == c.at_ability && ah != 0 { ah } else { 0 };
        let by = if any_owner && ah != 0 { format!("{at}:{ah:08x}") } else { String::new() };
        let key = if hash != 0 { format!("{hash:08x}") } else if owner != 0 { format!("t{ty}@{owner:08x}") } else { format!("t{ty}") };
        let meta = fx.meta.entry(key.clone()).or_default();
        meta.types.insert(ty);
        if !by.is_empty() {
            meta.by = by;
        }
        if owner != 0 {
            meta.owner = owner;
        }
        if giver != 0 {
            meta.giver = giver;
        }
        seen.insert(iid);
        match fx.live.get_mut(&iid) {
            Some(cur) if cur.0 == key => cur.2 = t,
            cur => {
                if let Some(cur) = cur {
                    fx.done.entry(cur.0.clone()).or_default().push([cur.1, cur.2]);
                }
                let start = min_f(t, max_f(0.0, apply_t - t0));
                // a re-keyed instance keeps its place, as assigning to an existing dict key does
                fx.live.insert(iid, (key, start, t, lvl));
            }
        }
    }
    let gone: Vec<u16> = fx.live.keys().filter(|i| !seen.contains(i)).copied().collect();
    for iid in gone {
        if let Some((key, a, z, _)) = fx.live.shift_remove(&iid) {
            fx.done.entry(key).or_default().push([a, z]);
        }
    }
}

/// Ability slots of a player. One use = one distinct `last_trigger_time`. The ability is active from the trigger
/// until its cooldown starts (`cooldown_start_time`; equal to the trigger for instant abilities), or for as long as
/// the slot state is Active (2) when no cooldown has started yet.
fn abilities(c: &Consts, p: &mut Player, ab: &[Ability], t: f64, t0: f64) {
    for (slot, &(hash, last_trigger, (cd_dur, cd_start), (_remaining, total), state)) in ab.iter().enumerate() {
        p.abil_hash.insert(slot, hash);
        if total == 0 || last_trigger <= 0.0 {
            continue;
        }
        let uses = p.abil.entry(slot).or_default();
        let a = max_f(0.0, last_trigger - t0);
        let u = uses.entry(last_trigger.to_bits()).or_insert((a, a, None));
        if state == c.ab_active {
            u.1 = max_f(u.1, t);
        }
        if cd_start >= last_trigger && cd_dur > 0.0 {
            u.1 = max_f(u.0, cd_start - t0);
            u.2 = Some(max_f(u.1, cd_start + cd_dur - t0));
        }
    }
}

/// "%Y%m%d-%H%M%S" in local time: the id of an encounter, from the wall time it started at.
fn local_stamp(wall: f64) -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
        let ticks = ((wall + 11_644_473_600.0) * 1e7) as u64; // 100 ns since 1601
        let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        // SAFETY: plain conversions between two stack structs.
        unsafe {
            let (mut utc, mut local): (SYSTEMTIME, SYSTEMTIME) = (std::mem::zeroed(), std::mem::zeroed());
            if FileTimeToSystemTime(&ft, &mut utc) != 0 && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0 {
                return format!("{:04}{:02}{:02}-{:02}{:02}{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond);
            }
        }
    }
    // fallback: UTC, by the civil-from-days algorithm
    let secs = wall.floor() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Feeds records to the current Encounter and decides when one starts and ends.
pub struct Tracker {
    pub resolver: Resolver,
    pub cur: Option<Encounter>,
    match_id: Option<String>,
    last_frame_wall: Option<f64>,
    /// wall time at which the current encounter's world went away
    leaving: Option<f64>,
    /// latest result data seen, whether or not an encounter is open
    last_res: Option<GameResult>,
    /// encounters that ended since the caller last took them
    pub closed: Vec<Encounter>,
}

impl Tracker {
    pub fn new(resolver: Resolver) -> Tracker {
        Tracker { resolver, cur: None, match_id: None, last_frame_wall: None, leaving: None, last_res: None, closed: Vec::new() }
    }

    pub fn feed(&mut self, recs: &[Rec]) {
        for r in recs {
            match r {
                Rec::Frame(f) => self.frame(f),
                Rec::Idle { wall, res } => self.idle(*wall, *res),
                Rec::Defs(d) => self.resolver.on_defs(self.cur.as_mut(), d),
                Rec::Builds { players } => self.resolver.on_builds(self.cur.as_mut(), players),
                Rec::Sample(s) => {
                    if self.cur.is_none() {
                        if s.res.is_some() {
                            self.last_res = s.res;
                        }
                        self.match_id = Some(s.match_id.clone());
                    } else {
                        self.sample(s);
                    }
                }
                Rec::Hit(h) => {
                    if let Some(e) = &mut self.cur {
                        e.on_hit(h, &self.resolver);
                    }
                }
                Rec::Heal { ev, tgt, hp, .. } => {
                    if let Some(e) = &mut self.cur {
                        e.on_heal(ev.src, *tgt, *hp);
                    }
                }
                _ => {}
            }
        }
    }

    /// Life of an encounter: client state RunningGame (2) = playing; GameEnded (3) = outro, the log time is frozen
    /// but the game's clock still runs; anything else = the world is gone. After that the encounter stays open for
    /// a few seconds, because the game fills in its result data (its own mission time, won / lost) while the level
    /// is being left.
    fn frame(&mut self, f: &Frame) {
        self.last_frame_wall = Some(f.wall);
        let c = consts::get();
        let cs = f.cs;
        let leaving = self.leaving.is_some();
        let (mut close, mut expire) = (false, false);
        if let Some(e) = &mut self.cur {
            if !leaving && cs == c.cs_running && !e.ended {
                e.on_frame(f);
            } else if !leaving && cs == c.cs_ended {
                e.on_frame(f); // the first GameEnded frame is the end of the mission time
                e.end(if e.t0.is_some() { "finished" } else { "left" });
            } else if cs == c.cs_running {
                close = true; // a new world is already running
            } else {
                e.end("left");
                expire = true;
            }
        }
        if close {
            self.close();
        }
        if expire {
            if self.leaving.is_none() {
                self.leaving = Some(f.wall);
            }
            self.expire(f.wall);
        }
        if self.cur.is_none() && cs == c.cs_running {
            let mut e = Encounter::new(self.match_id.clone(), f.wall, c);
            e.on_frame(f);
            e.res0 = self.last_res; // result data left over from the previous mission
            self.cur = Some(e);
        }
    }

    fn idle(&mut self, wall: f64, res: Option<GameResult>) {
        if res.is_some() {
            self.last_res = res;
        }
        let stalled = self.leaving.is_none() && self.last_frame_wall.is_some_and(|w| wall - w > 2.0);
        let Some(e) = &mut self.cur else { return };
        if e.on_result(res, wall) && e.ended {
            self.close();
            return;
        }
        if stalled {
            let result = e.result.unwrap_or(if e.ended { "finished" } else { "left" });
            e.end(result);
            self.leaving = Some(wall); // the simulation stopped updating (loading screen)
        }
        self.expire(wall);
    }

    fn expire(&mut self, wall: f64) {
        if self.cur.is_some() && self.leaving.is_some_and(|l| wall - l > 4.0) {
            self.close();
        }
    }

    fn sample(&mut self, s: &Sample) {
        if s.res.is_some() {
            self.last_res = s.res;
        }
        let Tracker { cur, resolver, match_id, leaving, .. } = self;
        let e = cur.as_mut().unwrap();
        let fresh = e.on_result(s.res, s.wall) && e.ended; // mission over and the game's result is in: save the log
        let new_match = match_id.is_some() && !s.match_id.is_empty() && match_id.as_deref() != Some(&s.match_id) && e.match_id.is_some();
        if !fresh && (leaving.is_some() || e.ended) {
            return;
        }
        if !fresh && !new_match {
            if !s.match_id.is_empty() {
                *match_id = Some(s.match_id.clone());
            }
            if e.match_id.is_none() {
                e.match_id = match_id.clone();
            }
            e.on_sample(s, resolver);
            resolver.on_sample(e);
            return;
        }
        self.close();
        if !fresh {
            self.match_id = Some(s.match_id.clone());
        }
    }

    /// End the current encounter (also called when the game closes).
    pub fn close(&mut self) {
        self.leaving = None;
        let Some(mut e) = self.cur.take() else { return };
        if !e.ended {
            e.end("left");
        }
        e.flush(&self.resolver, None); // hits still held back are counted now
        e.closed = true;
        e.end_wall = Some(self.last_frame_wall.unwrap_or_else(crate::mem::now));
        self.closed.push(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEAPON: u32 = 0xA1;
    const ZOMBIE: u32 = 0xE1;
    const DETONATOR: u32 = 0xD1;
    const CRATE: u32 = 0xC1;
    const TURRET: u32 = 0x71;
    const BARREL: u32 = 0xB1;

    fn frame(c: &Consts, wall: f64, t: f64, ended: bool) -> Rec {
        Rec::Frame(Frame { wall, t, ct: t, cs: if ended { c.cs_ended } else { c.cs_running }, cid: 1, paused: 0, ws: c.ws_ingame, wf: 0,
            snap: 0, ctl: 0, nent: 0, nenemy: 0, nev: 0 })
    }

    /// The mission's definitions: one weapon, and one entity per kind the tests hit or are hit by.
    fn defs(c: &Consts, more_enemies: &[u32]) -> Rec {
        let entity = |hash: u32, kind: &str| (hash, 0, 0, c.entity_types.iter().position(|t| t == kind).unwrap() as u8, 0, -1);
        let mut entities = vec![entity(ZOMBIE, "Enemy"), entity(DETONATOR, "Enemy"), entity(CRATE, "Destructible"), entity(TURRET, "Turret"),
            entity(BARREL, "ExplosiveBarrel")];
        entities.extend(more_enemies.iter().map(|h| entity(*h, "Enemy")));
        Rec::Defs(Box::new(Defs { level: "Assets/Levels/Operations/Op_Test.unity".into(), level_type: Some(2), mode: "ModeTest".into(),
            difficulty: 2, weapons: vec![(WEAPON, 0, 0, "Weapon_Title".into())], entities, ..Default::default() }))
    }

    fn sample(wall: f64, t: f64) -> Rec {
        let client = Client { id: 1, dc: 0, lvl: 30, host: 1, name: "P".into(), cls: "assault".into(), sub: String::new(), nk: String::new(),
            kills: 0, downs: 0, acc: 0.0, crit_kills: 0, attacks: Vec::new(), uses: Vec::new() };
        let me = PlayerEnt { id: 100, client: 1, hp: 100.0, hpmax: 100.0, sh: 0.0, shmax: 0.0, flags: 0, downs: 0, aw: 0, dmg: None,
            fx: Vec::new(), ab: Vec::new(), wp: Vec::new() };
        Rec::Sample(Sample { wall, t, match_id: "m".into(), clients: vec![client], ents: Some(Ents { n: 1, players: vec![me], ..Default::default() }), res: None })
    }

    /// A hit the server reports, credited to client 1: `attack` = (attack type, source definition).
    fn hit(c: &Consts, t: f64, attack: (u8, u32), target: u32, dmg: f64) -> Rec {
        Rec::Hit(Hit { t, seq: 0, src: c.src_server, sc: 0, tgt: 500, tdi: 0, tdh: target, da: dmg, dhu: dmg, dsu: 0.0, dh: dmg, ds: 0.0,
            res: 0.0, ress: 0.0, fl: 0, dt: 0, ac: 1, ai: 0, at: attack.0, ad: (0, attack.1), af: 0, pos: (0.0, 0.0, 0.0), late: None })
    }

    #[test]
    fn whose_damage_is_counted() {
        let c = consts::install_for_tests();
        let entity_attack = c.attack_types.iter().position(|t| t == "Entity").unwrap() as u8;
        let (weapon, by) = ((c.at_weapon, WEAPON), |hash: u32| (entity_attack, hash));
        let blast = |rec: Rec| match rec { Rec::Hit(mut h) => { h.fl |= c.hit_explosion; Rec::Hit(h) }, other => other };
        let (late_enemy, never_defined) = (0x99, 0x98);
        let (neither, structures, blasts, both) = (Counted::default(), Counted { structures: true, blasts: false },
            Counted { structures: false, blasts: true }, Counted { structures: true, blasts: true });
        let mut t = Tracker::new(Resolver::empty());
        t.feed(&[frame(&c, 10.0, 1.0, false), defs(&c, &[]), sample(10.0, 1.0)]);
        t.feed(&[
            hit(&c, 2.0, weapon, ZOMBIE, 100.0),
            hit(&c, 2.0, weapon, CRATE, 50.0),
            // an enemy that explodes, which the game credits to the player: among zombies, and on a turret
            blast(hit(&c, 2.5, by(DETONATOR), ZOMBIE, 30.0)),
            blast(hit(&c, 2.5, by(DETONATOR), TURRET, 20.0)),
            // an enemy's attack is never the player's
            hit(&c, 2.6, by(ZOMBIE), TURRET, 8.0),
            // a barrel the player set off is the player's
            blast(hit(&c, 3.0, by(BARREL), ZOMBIE, 10.0)),
            // sources the mission's tables do not have yet: held until they do, or for a while
            hit(&c, 3.0, by(late_enemy), ZOMBIE, 7.0),
            hit(&c, 3.0, by(never_defined), ZOMBIE, 5.0),
        ]);
        let e = t.cur.as_ref().unwrap();
        let figures = |e: &Encounter| [neither, structures, blasts, both].map(|counted| e.party_damage(counted));
        assert_eq!((figures(e), e.enemy_hits, e.blast_hits, e.pending_hits.len()), ([110.0, 160.0, 140.0, 210.0], 1, 2, 2));

        // the tables are read again and the first of the two turns out to be an enemy; the other stays unknown,
        // and is counted once it has waited long enough
        t.feed(&[frame(&c, 10.2, 3.2, false), defs(&c, &[late_enemy]), sample(10.2, 3.2)]);
        let e = t.cur.as_ref().unwrap();
        assert_eq!((figures(e), e.enemy_hits, e.pending_hits.len()), ([110.0, 160.0, 140.0, 210.0], 2, 1));
        t.feed(&[frame(&c, 13.0, 6.0, false), sample(13.0, 6.0)]);
        let e = t.cur.as_ref().unwrap();
        assert_eq!((figures(e), e.enemy_hits, e.blast_hits, e.pending_hits.len()), ([115.0, 165.0, 145.0, 215.0], 2, 2, 0));

        // the log: figures with neither, the other three choices next to it
        let d = e.detail(&t.resolver);
        let of = |key: &str| d["views"][key]["party_damage"].as_f64();
        assert_eq!((d["party_damage"].as_f64(), of("structures"), of("blasts"), of("structures+blasts")), (Some(115.0), Some(165.0), Some(145.0), Some(215.0)));
        let p = &d["players"][0];
        assert_eq!((p["damage"].as_f64(), p["hits"].as_u64(), d["views"]["structures"]["players"]["1"]["damage"].as_f64()), (Some(115.0), Some(3), Some(165.0)));
        let sources = |p: &Value| p["sources"].as_array().unwrap().iter().map(|s| (s["kind"].as_str().unwrap().to_string(), s["damage"].as_f64().unwrap())).collect::<Vec<_>>();
        assert_eq!(sources(p), [("weapon".to_string(), 100.0), ("barrel".to_string(), 10.0), ("entity".to_string(), 5.0)]);
        assert_eq!(sources(&d["views"]["structures"]["players"]["1"])[0], ("weapon".to_string(), 150.0));
        assert_eq!(sources(&d["views"]["blasts"]["players"]["1"])[1], ("enemy".to_string(), 30.0));
        assert_eq!(sources(&d["views"]["structures+blasts"]["players"]["1"])[1], ("enemy".to_string(), 50.0));
        assert_eq!((d["stats"]["enemy_hits"].as_u64(), d["stats"]["blast_hits"].as_u64(), d["stats"]["server_hits"].as_u64()), (Some(2), Some(2), Some(4)));
        let targets = |d: &Value| d["targets"].as_array().unwrap().iter()
            .map(|t| (t["type"].as_str().unwrap().to_string(), t["structure"].as_bool().unwrap(), t["damage"].as_f64().unwrap())).collect::<Vec<_>>();
        assert_eq!(targets(&d), [("Enemy".to_string(), false, 115.0), ("Destructible".to_string(), true, 50.0)], "no turret: only enemies hit it");

        // the views of it
        let shown = |want: Counted| { let mut v = d.clone(); view(&mut v, want); v };
        let (off, with_structures, with_blasts, all) = (shown(neither), shown(structures), shown(blasts), shown(both));
        assert_eq!((off["structures"].as_str(), off["blasts"].as_str(), off["party_damage"].as_f64(), off["targets"].as_array().unwrap().len()), (Some("off"), Some("off"), Some(115.0), 1));
        assert_eq!(off["players"][0]["targets"].as_array().unwrap().len(), 1);
        assert_eq!((with_structures["structures"].as_str(), with_structures["party_damage"].as_f64(), targets(&with_structures).len()), (Some("on"), Some(165.0), 2));
        assert_eq!((with_structures["players"][0]["damage"].as_f64(), with_structures["players"][0]["sources"][0]["damage"].as_f64()), (Some(165.0), Some(150.0)));
        // blasts without structures: the zombies take the blast, the turret stays out of sight
        assert_eq!((with_blasts["blasts"].as_str(), with_blasts["party_damage"].as_f64()), (Some("on"), Some(145.0)));
        assert_eq!(targets(&with_blasts), [("Enemy".to_string(), false, 145.0)]);
        assert_eq!(targets(&with_blasts["players"][0]), [("Enemy".to_string(), false, 145.0)]);
        assert_eq!(targets(&all), [("Enemy".to_string(), false, 145.0), ("Destructible".to_string(), true, 50.0), ("Turret".to_string(), true, 20.0)]);
        assert!([&off, &with_structures, &with_blasts, &all].iter().all(|v| v.get("views").is_none()));
        // a list row and a live payload switch the same way
        let (mut row, mut live) = (e.summary(), e.live());
        view(&mut row, both);
        view(&mut live, blasts);
        assert_eq!((row["party_damage"].as_f64(), row["players"][0]["damage"].as_f64(), live["players"][0]["damage"].as_f64()), (Some(215.0), Some(215.0), Some(145.0)));
        // a log of format 8 kept structures apart and has no blasts; one from before has a single set of figures
        let mut eight = json!({ "party_damage": 9.0, "players": [{ "cid": 1, "damage": 9.0 }], "targets": [{ "structure": true }],
            "with_structures": { "party_damage": 12.0, "players": { "1": { "damage": 12.0 } } } });
        let mut seven = json!({ "party_damage": 9.0, "players": [], "targets": [{ "type": "Destructible" }] });
        view(&mut eight, both);
        view(&mut seven, neither);
        assert_eq!((eight["structures"].as_str(), eight["blasts"].as_str(), eight["party_damage"].as_f64(), eight["targets"].as_array().unwrap().len()), (Some("on"), Some("never"), Some(12.0), 1));
        assert_eq!((seven["structures"].as_str(), seven["blasts"].as_str(), seven["targets"].as_array().unwrap().len()), (Some("always"), Some("always"), 1));
    }

    #[test]
    fn the_result_is_read_once_the_game_has_written_all_of_it() {
        let c = consts::install_for_tests();
        let mut t = Tracker::new(Resolver::empty());
        // what the last mission left behind
        t.feed(&[Rec::Idle { wall: 90.0, res: Some((1, 50.0, 1, 0, 111)) }]);
        t.feed(&[frame(&c, 99.0, 1.0, false), defs(&c, &[]), sample(99.0, 1.0), hit(&c, 2.0, (c.at_weapon, WEAPON), ZOMBIE, 100.0),
            frame(&c, 99.5, 10.0, false), frame(&c, 99.6, 10.1, true)]);
        assert!(t.cur.as_ref().unwrap().ended && t.closed.is_empty());
        // the time shows first, with the experience still that of the last mission
        t.feed(&[Rec::Idle { wall: 100.0, res: Some((1, 12.5, 1, 0, 111)) }]);
        t.feed(&[Rec::Idle { wall: 100.1, res: Some((1, 12.5, 1, 0, 111)) }]);
        assert!(t.closed.is_empty(), "not yet: the game may still be writing its result");
        t.feed(&[Rec::Idle { wall: 100.5, res: Some((1, 12.5, 1, 0, 2400)) }]);
        let e = &t.closed[0];
        assert_eq!((e.game_time, e.outcome, e.xp, e.result), (Some(12.5), Some("won"), Some(2400), Some("finished")));
        assert_eq!((e.summary()["xp"].as_u64(), e.is_mission()), (Some(2400), true));
    }

    #[test]
    fn what_an_effect_leaves_behind_belongs_to_it() {
        consts::install_for_tests();
        let dash = AbLane { name: "Dash".into(), asset: "dash_ability".into(), hash: 1, uses: vec![(3.41, 3.71, Some(45.4))] };
        let effect = |key: &str, name: &str, a: f64, z: f64| obj(json!({ "key": key, "name": name, "kind": "", "intervals": [[a, z]], "total": z - a,
            "uptime": (z - a) / 100.0, "applications": 1, "types": [7], "owner": 0, "by": "", "asset": "" }));
        let owner_of = |e: &Value| (e["key"] == "8b154ce5").then(|| ("ability", "dash_ability".to_string(), Some("Relay Race".to_string())));
        let lanes = group_lanes(&[dash], vec![
            effect("8b154ce5", "Relay Race", 3.41, 4.41),
            // starts as the Relay Race set ends: the speed it grants. And one that merely happens later.
            effect("t7", "Movement speed modifier", 4.42, 9.41),
            effect("t11", "Block actions", 60.0, 63.0),
        ], 100.0, Some(&owner_of));
        let rows: Vec<(&str, &str, &str)> = lanes.iter().map(|l| (l["group"].as_str().unwrap(), l["name"].as_str().unwrap(), l["source"].as_str().unwrap_or(""))).collect();
        assert_eq!(rows, [("Abilities", "Dash", ""), ("Abilities", "Relay Race", "Dash"),
            ("Abilities", "Relay Race: movement speed modifier", "Dash"), ("Other effects", "Block actions", "")]);
    }
}
