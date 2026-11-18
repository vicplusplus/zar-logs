//! What the decoder emits. Field names are the ones the Python capture uses, so a record serialises to the same
//! JSON in both implementations (that is how the port is regression-tested).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// JSON object keys are text. Read back inside a tagged record they stay text, so maps keyed by a number parse
/// their keys here.
fn int_keys<'de, D, K, V>(d: D) -> Result<BTreeMap<K, V>, D::Error>
where D: serde::Deserializer<'de>, K: std::str::FromStr + Ord, V: Deserialize<'de> {
    let raw: BTreeMap<String, V> = BTreeMap::deserialize(d)?;
    raw.into_iter().map(|(k, v)| k.parse().map(|k| (k, v)).map_err(|_| serde::de::Error::custom(format!("key {k:?} is not a number")))).collect()
}
fn opt_int_keys<'de, D, K, V>(d: D) -> Result<Option<BTreeMap<K, V>>, D::Error>
where D: serde::Deserializer<'de>, K: std::str::FromStr + Ord, V: Deserialize<'de> {
    let raw: Option<BTreeMap<String, V>> = Option::deserialize(d)?;
    raw.map(|m| m.into_iter().map(|(k, v)| k.parse().map(|k| (k, v)).map_err(|_| serde::de::Error::custom(format!("key {k:?} is not a number"))))
        .collect()).transpose()
}

/// GameRunner.resultData: (valid, time, objective result, leave reason).
pub type GameResult = (u8, f64, u8, i32);
/// One status effect on an entity: (instance id, set hash, effect type, apply time, duration, state, suspended,
/// set level, giver entity id, attributed attack type, attributed attack hash).
pub type Effect = (u16, u32, u8, f64, f64, u8, u8, u8, u32, u8, u32);
/// One ability slot: (ability hash, last trigger time, (cooldown duration, cooldown start), (remaining count,
/// max count), state).
pub type Ability = (u32, f64, (f64, f64), (u16, u16), u8);
/// One weapon: (weapon hash, weapon data index, ammo in magazine, state, fire count).
pub type Weapon = (u32, u32, u16, u8, u16);
/// A destructible level prop seen for the first time: (entity id, data hash, tag id, prop id, max health).
pub type Prop = (u32, u32, i32, i32, f64);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Frame {
    pub wall: f64,
    pub t: f64,
    pub ct: f64,
    pub cs: u32,
    pub cid: u32,
    pub paused: u8,
    pub ws: u8,
    pub wf: u8,
    pub snap: u32,
    pub ctl: u32,
    pub nent: i64,
    pub nenemy: i64,
    pub nev: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Hit {
    pub t: f64,
    pub seq: u32,
    pub src: u8,
    pub sc: u32,
    pub tgt: u32,
    pub tdi: u32,
    pub tdh: u32,
    pub da: f64,
    pub dhu: f64,
    pub dsu: f64,
    pub dh: f64,
    pub ds: f64,
    pub res: f64,
    pub ress: f64,
    pub fl: u16,
    pub dt: u8,
    pub ac: u32,
    pub ai: u32,
    pub at: u8,
    pub ad: (u32, u32),
    pub af: u8,
    pub pos: (f64, f64, f64),
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late: Option<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Ev {
    pub t: f64,
    pub seq: u32,
    pub src: u8,
    pub sc: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late: Option<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Client {
    pub id: u32,
    pub dc: u8,
    pub lvl: u8,
    pub host: u8,
    pub name: String,
    pub cls: String,
    pub sub: String,
    pub nk: String,
    pub kills: u32,
    pub downs: u32,
    pub acc: f64,
    pub crit_kills: u32,
    pub attacks: Vec<(u8, u32)>,
    pub uses: Vec<(u32, u16, u16, u16, u16, u16, u16, f64, f64)>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlayerEnt {
    pub id: u32,
    pub client: u32,
    pub hp: f64,
    pub hpmax: f64,
    pub sh: f64,
    pub shmax: f64,
    pub flags: u32,
    pub downs: u8,
    pub aw: u8,
    pub dmg: Option<f64>,
    pub fx: Vec<Effect>,
    pub ab: Vec<Ability>,
    pub wp: Vec<Weapon>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BossEnt {
    pub id: u32,
    pub hash: u32,
    pub hp: f64,
    pub hpmax: f64,
    pub sh: f64,
    pub shmax: f64,
    pub flags: u32,
    pub spawn: f64,
    pub death: f64,
    pub mods: Option<(u32, u8, u8)>,
    pub fx: Vec<Effect>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Ents {
    pub n: i64,
    pub players: Vec<PlayerEnt>,
    #[serde(default)]
    pub full: bool,
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "opt_int_keys")]
    pub census: Option<BTreeMap<u8, u32>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub props: Option<Vec<Prop>>,
    #[serde(default)]
    pub bosses: Vec<BossEnt>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Sample {
    pub wall: f64,
    pub t: f64,
    #[serde(rename = "match")]
    pub match_id: String,
    pub clients: Vec<Client>,
    pub ents: Option<Ents>,
    #[serde(default)]
    pub res: Option<GameResult>,
}

/// What one entity definition can take. Serialised as the 13-element array the Python capture emits, in field order.
#[derive(Clone, Debug, PartialEq)]
pub struct EntDefense {
    pub hp: f64,
    pub shield: f64,
    /// index into `Defense::res` for the health pool (-1 = none)
    pub res: i32,
    /// index into `Defense::res` for the shield pool (-1 = none)
    pub shield_res: i32,
    /// index into `Defense::eres` (-1 = none)
    pub effect_res: i32,
    pub explosion: f64,
    pub dodge: f64,
    /// elemental effectiveness reduction
    pub elemental: f64,
    pub regen: f64,
    pub mods: u32,
    pub stage: u8,
    pub mod_level: u8,
    pub entity_type: u8,
}

impl<'de> Deserialize<'de> for EntDefense {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        type Row = ((f64, f64, i32, i32, i32, f64, f64), (f64, f64, u32, u8, u8, u8));
        // 13 values in one array: read it as a flat sequence, in two halves
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Row;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("an array of 13 numbers") }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut s: A) -> Result<Row, A::Error> {
                macro_rules! next { () => { s.next_element()?.ok_or_else(|| serde::de::Error::custom("defence row too short"))? } }
                Ok(((next!(), next!(), next!(), next!(), next!(), next!(), next!()), (next!(), next!(), next!(), next!(), next!(), next!())))
            }
        }
        let (a, b) = d.deserialize_seq(V)?;
        Ok(EntDefense { hp: a.0, shield: a.1, res: a.2, shield_res: a.3, effect_res: a.4, explosion: a.5, dodge: a.6,
            elemental: b.0, regen: b.1, mods: b.2, stage: b.3, mod_level: b.4, entity_type: b.5 })
    }
}

impl Serialize for EntDefense {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let mut t = s.serialize_tuple(13)?;
        t.serialize_element(&self.hp)?;
        t.serialize_element(&self.shield)?;
        t.serialize_element(&self.res)?;
        t.serialize_element(&self.shield_res)?;
        t.serialize_element(&self.effect_res)?;
        t.serialize_element(&self.explosion)?;
        t.serialize_element(&self.dodge)?;
        t.serialize_element(&self.elemental)?;
        t.serialize_element(&self.regen)?;
        t.serialize_element(&self.mods)?;
        t.serialize_element(&self.stage)?;
        t.serialize_element(&self.mod_level)?;
        t.serialize_element(&self.entity_type)?;
        t.end()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Defense {
    pub res: Vec<Vec<f64>>,
    pub eres: Vec<Vec<f64>>,
    #[serde(deserialize_with = "int_keys")]
    pub ents: BTreeMap<u32, EntDefense>,
    pub mult: Vec<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Defs {
    pub level: String,
    pub mode: String,
    pub difficulty: u8,
    pub modifier: u8,
    pub mode_title: String,
    pub avg_level: i32,
    /// (blob hash, own id, original id, title)
    pub weapons: Vec<(u32, u32, u32, String)>,
    /// (blob hash, original id, title)
    pub abilities: Vec<(u32, u32, String)>,
    /// (blob hash, own id, original id, entity type, is boss, stat group index)
    pub entities: Vec<(u32, u32, u32, u8, u8, i32)>,
    /// (blob hash, own id, original id)
    pub sets: Vec<(u32, u32, u32)>,
    /// (id, title, singular title, is boss)
    pub statgroups: Vec<(String, String, String, u8)>,
    pub bossmods: Vec<String>,
    pub tags: Vec<String>,
    pub enemymods: Vec<String>,
    pub defense: Option<Defense>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GearMod {
    pub id: String,
    pub quality: u8,
    pub offset: i32,
    pub enh: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Gear {
    pub slot: usize,
    pub id: String,
    pub level: i32,
    #[serde(rename = "type")]
    pub kind: u8,
    pub quality: u8,
    pub mods: Vec<GearMod>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AbilityUpgrades {
    pub ability: String,
    pub ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AbilityEnhancement {
    pub ability: String,
    pub id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PlayerInfo {
    #[serde(default)]
    pub nk: String,
    pub name: String,
    pub level: i32,
    pub cls: String,
    pub sub: String,
    pub gear: Vec<Gear>,
    pub armour_mods: Vec<GearMod>,
    pub abilities: Vec<String>,
    pub upgrades: Vec<AbilityUpgrades>,
    pub enhancements: Vec<AbilityEnhancement>,
    pub skills: Vec<(String, i32)>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum Rec {
    Frame(Frame),
    /// Nothing new from the simulation (loading screen, menus); still carries the game's result data.
    Idle { wall: f64, res: Option<GameResult> },
    Hit(Hit),
    Heal { #[serde(flatten)] ev: Ev, tgt: u32, ha: f64, hp: f64, rev: u8 },
    Abil { #[serde(flatten)] ev: Ev, slot: u32 },
    Gkill { #[serde(flatten)] ev: Ev, n: u8, ac: u32, at: u8, ad: (u32, u32) },
    Fire { #[serde(flatten)] ev: Ev },
    Reload { #[serde(flatten)] ev: Ev, ent: u32 },
    Reloadb { #[serde(flatten)] ev: Ev, ent: u32 },
    Sample(Sample),
    Defs(Box<Defs>),
    Builds { players: Vec<PlayerInfo> },
}

impl Rec {
    pub fn kind(&self) -> &'static str {
        match self {
            Rec::Frame(_) => "frame",
            Rec::Idle { .. } => "idle",
            Rec::Hit(_) => "hit",
            Rec::Heal { .. } => "heal",
            Rec::Abil { .. } => "abil",
            Rec::Gkill { .. } => "gkill",
            Rec::Fire { .. } => "fire",
            Rec::Reload { .. } => "reload",
            Rec::Reloadb { .. } => "reloadb",
            Rec::Sample(_) => "sample",
            Rec::Defs(_) => "defs",
            Rec::Builds { .. } => "builds",
        }
    }
}
