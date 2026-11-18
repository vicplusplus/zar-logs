//! Frame poller + event decoder.
//!
//! Once per rendered frame the game calls the native `client_update` and stores its output struct in
//! `GameRunner.clientUpdateResult` (a plain NativeBuildInterop.ClientUpdateResult field). That struct points at the
//! frame's event list (`events`, `event_data`, `event_count`) which stays untouched until the next `client_update`.
//! We poll the struct about once a millisecond; when it changes we read that frame's events once.
//!
//! Hit events carry the damage split (health / shield, attempted, resistance), flags (crit, DoT, final blow, ...),
//! the damage type, the target and a full attribution (which client, which weapon / ability / entity).
//! Only events with source == Server are authoritative; source == Client are local predictions and are tagged.
//!
//! Every offset, struct size, array stride and enum value used here is looked up by name in the layout generated
//! from the running game build (`Off`, `consts`). The numbers left in this file are the meter's own choices: how
//! often it samples, and caps that reject garbage read while the game is rewriting a buffer.
//!
//! Everything here is a read. See proc.rs for the access rights of the process handle.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::layout::Layout;
use crate::mem::{le_f32, le_f64, le_i32, le_i64, le_u16, le_u32, le_u64, Mem};
use crate::records::*;
use crate::world::World;

pub const GAME_EXE: &str = "zombieassault.exe";
pub const GAME_DLL: &str = "gameassembly.dll";

// ---- the meter's own choices (not game values) ----
/// More events than this in one frame means the header was read while it was being rewritten.
const MAX_EVENTS: u32 = 20000;
/// Finding the players again after one of them moved in memory: at most this often.
const WALK_RETRY: f64 = 0.1;
/// Full entity walk (one small read per entity): once a second.
const ENTITY_PERIOD: f64 = 1.0;
/// Following the pointer chain to the game's runner object again: once a second.
const RESOLVE_PERIOD: f64 = 1.0;
/// Caps on counts read from memory, against garbage: clients in a match, entities in a world, effects on an
/// entity, ability slots, weapons.
const MAX_CLIENTS: usize = 16;
const MAX_ENTITIES: i64 = 20000;
const MAX_EFFECTS: usize = 512;
const MAX_ABILITIES: usize = 16;
const MAX_WEAPONS: usize = 8;
/// Longest player / class name and match id read (bytes).
const NAME_LEN: usize = 64;
const ACCOUNT_LEN: usize = 40;
const MATCH_LEN: usize = 80;

#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct Stats {
    pub frames: u64,
    pub events: u64,
    pub hits: u64,
    pub dup: u64,
    pub torn: u64,
    pub bad: u64,
    pub late: u64,
}

/// Every offset the decoder uses, looked up once by name. A missing field stops the meter at start-up with a
/// message that names it.
struct Off {
    hdr_size: usize,
    // ClientUpdateResult
    r_event_count: usize, r_events: usize, r_event_data: usize, r_time: usize, r_client_time: usize, r_client_state: usize,
    r_client_id: usize, r_client_paused: usize, r_world_state: usize, r_world_flags: usize, r_snapshot: usize, r_ctl: usize,
    r_entity_count: usize, r_enemy_count: usize, r_client_count: usize, r_clients: usize, r_match_id: usize, r_entities: usize,
    // GameEvent
    ev_stride: usize, e_time: usize, e_seq: usize, e_client: usize, e_type: usize, e_source: usize,
    // HitEventData: every offset from the start of the record
    h_target: usize, h_data_index: usize, h_data_hash: usize, h_key_len: usize,
    d_attempted: usize, d_health_uncapped: usize, d_shield_uncapped: usize, d_health: usize, d_shield: usize,
    d_resistance: usize, d_resistance_shield: usize, d_flags: usize, d_type: usize,
    a_client: usize, a_index: usize, a_type: usize, a_data_index: usize, a_data_hash: usize, a_flags: usize,
    h_x: usize, h_y: usize, h_z: usize,
    // HealEventData, GroupedKillsEventData
    he_entity: usize, he_attempted: usize, he_applied: usize, he_revived: usize,
    gk_kills: usize, gk_client: usize, gk_type: usize, gk_data_index: usize, gk_data_hash: usize,
    // ClientUpdateResultClientInfo and the two small tables it points at
    ci_size: usize, ci_nkapi: usize, ci_name: usize, ci_class: usize, ci_subclass: usize, ci_attacks_count: usize,
    ci_attacks: usize, ci_uses_count: usize, ci_uses: usize, ci_id: usize, ci_dc: usize, ci_lvl: usize, ci_host: usize,
    ci_kills: usize, ci_downs: usize, ci_acc: usize, ci_crit: usize,
    at_stride: usize, at_type: usize, at_data: usize,
    au_stride: usize, au_index: usize, au_u16: [usize; 6], au_heal: usize, au_distance: usize,
    // GameEntity
    ge_size: usize, ge_type: usize, ge_id: usize, ge_flags: usize, ge_data_hash: usize, ge_tag: usize, ge_prop: usize,
    ge_hp: usize, ge_hp_max: usize, ge_shield: usize, ge_shield_max: usize, ge_subclass: usize, ge_client: usize,
    ge_downed: usize, ge_active_weapon: usize, ge_spawn: usize, ge_death: usize, ge_effects: usize, ge_abilities: usize,
    ge_weapons: usize,
    /// how much of an entity the walk reads to tell what it is, and to read a prop's tag
    ge_head: usize, ge_prop_window: usize,
    pg_size: usize, pg_total: usize,
    // EnemyGameEntity -> EnemyData
    eg_data: usize, ed_size: usize, ed_mask: usize, ed_stage: usize, ed_level: usize,
    // StatusEffect
    se_stride: usize, se_apply: usize, se_duration: usize, se_id: usize, se_set_hash: usize, se_type: usize, se_state: usize,
    se_susp: usize, se_level: usize, se_giver: usize, se_attack_type: usize, se_attack_hash: usize,
    // AbilitySlot, WeaponState
    as_stride: usize, as_hash: usize, as_trigger: usize, as_cooldown: usize, as_cooldown_start: usize, as_remaining: usize,
    as_total: usize, as_state: usize,
    ws_stride: usize, ws_hash: usize, ws_index: usize, ws_ammo: usize, ws_state: usize, ws_fire: usize,
    /// a native vector embedded in a struct: where its first-element and past-the-end pointers sit
    vec_begin: usize, vec_end: usize,
    // GameRunner, GameResultData
    gr_cur: u64, gr_result: u64, rd_valid: usize, rd_time: usize, rd_objective: usize, rd_leave: usize, rd_window: usize,
    /// bits set in the class slot while the runtime has not resolved it yet
    slot_unresolved: u64,
    // GameEventType
    t_hit: u8, t_heal: u8, t_abil: u8, t_gk: u8, t_fire: u8, t_reload: u8, t_reloadb: u8,
}

impl Off {
    fn new(l: &Layout) -> Off {
        let (r, e, h, d) = (l.st("ClientUpdateResult"), l.st("GameEvent"), l.st("HitEventData"), l.st("HitEventDamageInfo"));
        let (a, aa, he, gk) = (l.st("EntityKillAttributionInfo"), l.st("EntityKillAttributedAttack"), l.st("HealEventData"), l.st("GroupedKillsEventData"));
        let (ci, ge, pg, gr) = (l.st("ClientUpdateResultClientInfo"), l.st("GameEntity"), l.st("PlayerGameEntity"), l.st("GameRunner"));
        let (hs, sh, stk, se, sd) = (l.st("HealthState"), l.st("ShieldState"), l.st("StatusEffectStack"), l.st("StatusEffect"), l.st("StatusEffectData"));
        let (asl, ws, rd, id, v3) = (l.st("AbilitySlot"), l.st("WeaponState"), l.st("GameResultData"), l.st("WorldLoadDataId"), l.st("CVec3"));
        let (at, au, aud, eg, ed) = (l.st("ClientUpdateResultAttributedAttack"), l.st("ClientUpdateResultAbilityUse"), l.st("AbilityUseData"),
            l.st("EnemyGameEntity"), l.st("EnemyData"));
        let et = |n: &str| l.en("GameEventType", n) as u8;
        assert_eq!(l.event_extra.get(&et("Hit")).copied(), Some(h.size()), "layout: the Hit event's record is not a HitEventData");
        // offsets from the start of a hit record
        let (dmg, attr, pos) = (h.o("damage_info"), h.o("damage_info") + d.o("attribution_info"), h.o("position"));
        let attack = attr + a.o("attack");
        let (hid, gkid) = (h.o("hit_entity_data_id"), gk.o("src_entity_data_id"));
        let use_data = au.o("use_data");
        let (health, shield) = (ge.o("health"), ge.o("shield") + sh.o("health"));
        let se_attack = se.o("attribution_info") + a.o("attack");
        let four = std::mem::size_of::<u32>();
        Off {
            hdr_size: r.size(),
            r_event_count: r.o("event_count"), r_events: r.o("events"), r_event_data: r.o("event_data"), r_time: r.o("time"),
            r_client_time: r.o("client_time"), r_client_state: r.o("client_state"), r_client_id: r.o("client_id"),
            r_client_paused: r.o("client_paused"), r_world_state: r.o("world_state"), r_world_flags: r.o("world_flags"),
            r_snapshot: r.o("current_snapshot_id"), r_ctl: r.o("client_controlling_entity_id"), r_entity_count: r.o("entity_count"),
            r_enemy_count: r.o("enemy_entity_count"), r_client_count: r.o("client_count"), r_clients: r.o("clients"),
            r_match_id: r.o("match_id"), r_entities: r.o("entities"),
            ev_stride: e.size(), e_time: e.o("time"), e_seq: e.o("seq_id"), e_client: e.o("source_client_id"), e_type: e.o("type"),
            e_source: e.o("source"),
            h_target: h.o("hit_entity_id"), h_data_index: hid + id.o("index"), h_data_hash: hid + id.o("hash"),
            // what tells two hits of one frame apart: the target and its definition
            h_key_len: hid + id.size(),
            d_attempted: dmg + d.o("damage_attempted"), d_health_uncapped: dmg + d.o("damage_to_health_uncapped"),
            d_shield_uncapped: dmg + d.o("damage_to_shield_uncapped"), d_health: dmg + d.o("damage_applied_to_health"),
            d_shield: dmg + d.o("damage_applied_to_shield"), d_resistance: dmg + d.o("damage_resistance"),
            d_resistance_shield: dmg + d.o("damage_resistance_shield"), d_flags: dmg + d.o("flags"), d_type: dmg + d.o("damage_type"),
            a_client: attr + a.o("client_id"), a_index: attr + a.o("attribution_index"), a_type: attack + aa.o("type"),
            a_data_index: attack + aa.o("data") + id.o("index"), a_data_hash: attack + aa.o("data") + id.o("hash"), a_flags: attr + a.o("flags"),
            h_x: pos + v3.o("x"), h_y: pos + v3.o("y"), h_z: pos + v3.o("z"),
            he_entity: he.o("entity_id"), he_attempted: he.o("heal_attempted"), he_applied: he.o("heal_applied"), he_revived: he.o("entity_revived"),
            gk_kills: gk.o("kills"), gk_client: gk.o("attribution_client_id"), gk_type: gk.o("attack_type"),
            gk_data_index: gkid + id.o("index"), gk_data_hash: gkid + id.o("hash"),
            ci_size: ci.size(), ci_nkapi: ci.o("nkapi_id"), ci_name: ci.o("display_name"), ci_class: ci.o("class_name"),
            ci_subclass: ci.o("subclass_name"), ci_attacks_count: ci.o("attacks_count"), ci_attacks: ci.o("attacks"),
            ci_uses_count: ci.o("ability_uses_count"), ci_uses: ci.o("ability_uses"), ci_id: ci.o("id"), ci_dc: ci.o("disconnected"),
            ci_lvl: ci.o("player_level"), ci_host: ci.o("is_mission_host"), ci_kills: ci.o("total_kills"),
            ci_downs: ci.o("downed_count"), ci_acc: ci.o("weapon_accuracy"), ci_crit: ci.o("critical_kills"),
            at_stride: at.size(), at_type: at.o("type"), at_data: at.o("data"),
            au_stride: au.size(), au_index: au.o("ability_data_index"),
            au_u16: ["uses", "kills", "supportive_assists", "supportive_self_assists", "aggressive_assists", "aggressive_self_assists"]
                .map(|f| use_data + aud.o(f)),
            au_heal: use_data + aud.o("heal"), au_distance: use_data + aud.o("distance_travelled"),
            ge_size: ge.size(), ge_type: ge.o("entity_type"), ge_id: ge.o("id"), ge_flags: ge.o("flags"),
            ge_data_hash: ge.o("entity_data_id") + id.o("hash"), ge_tag: ge.o("tag_id"), ge_prop: ge.o("prop_id"),
            ge_hp: health + hs.o("current"), ge_hp_max: health + hs.o("max"), ge_shield: shield + hs.o("current"),
            ge_shield_max: shield + hs.o("max"), ge_subclass: ge.o("subclass"), ge_client: ge.o("client_id"),
            ge_downed: ge.o("downed_count"), ge_active_weapon: ge.o("active_weapon"), ge_spawn: ge.o("spawn_time"),
            ge_death: ge.o("death_time"), ge_effects: ge.o("status_effects") + stk.o("effects_"),
            ge_abilities: ge.o("activated_abilities_"), ge_weapons: ge.o("weapons_"),
            ge_head: [ge.o("entity_type") + 1, ge.o("id") + four, ge.o("flags") + four].into_iter().max().unwrap(),
            ge_prop_window: [ge.o("entity_data_id") + id.size(), ge.o("tag_id") + four, ge.o("prop_id") + four, health + hs.o("max") + four]
                .into_iter().max().unwrap(),
            pg_size: pg.size(), pg_total: pg.o("total_damage_done"),
            eg_data: eg.o("enemy_data"), ed_size: ed.size(), ed_mask: ed.o("active_mods_mask"), ed_stage: ed.o("stage_index"),
            ed_level: ed.o("mod_level"),
            se_stride: se.size(), se_apply: se.o("apply_time"), se_duration: se.o("duration"), se_id: se.o("id"),
            se_set_hash: se.o("set_id") + id.o("hash"), se_type: se.o("data") + sd.o("type"), se_state: se.o("state"),
            se_susp: se.o("is_suspended"), se_level: se.o("data") + sd.o("set_level"), se_giver: se.o("giver_entity_id"),
            se_attack_type: se_attack + aa.o("type"), se_attack_hash: se_attack + aa.o("data") + id.o("hash"),
            as_stride: asl.size(), as_hash: asl.o("data_id") + id.o("hash"), as_trigger: asl.o("last_trigger_time"),
            as_cooldown: asl.o("cooldown_duration"), as_cooldown_start: asl.o("cooldown_start_time"),
            as_remaining: asl.o("remaining_count"), as_total: asl.o("total_trigger_count"), as_state: asl.o("state"),
            ws_stride: ws.size(), ws_hash: ws.o("weapon_data_id") + id.o("hash"), ws_index: ws.o("weapon_data_id") + id.o("index"),
            ws_ammo: ws.o("mag_ammo_remaining"), ws_state: ws.o("state"), ws_fire: ws.o("fire_count"),
            vec_begin: l.rt("native_vector", "begin"), vec_end: l.rt("native_vector", "end"),
            gr_cur: gr.o("clientUpdateResult") as u64, gr_result: gr.o("resultData") as u64,
            rd_valid: rd.o("valid"), rd_time: rd.o("time"), rd_objective: rd.o("objectiveResult"), rd_leave: rd.o("leaveLevelReason"),
            rd_window: rd.o("leaveLevelReason") + four,
            slot_unresolved: l.rt("typeinfo_slot", "unresolved_mask") as u64,
            t_hit: et("Hit"), t_heal: et("Heal"), t_abil: et("ActivateAbility"), t_gk: et("GroupedKills"),
            t_fire: et("WeaponFire"), t_reload: et("WeaponReload"), t_reloadb: et("WeaponReloadBegin"),
        }
    }
}

/// What makes two event records the same event: (sequence id, source, type, time bits, first bytes of its data).
type SeenKey = (u32, u8, u8, u64, [u8; 16], u8);

/// Turns memory reads into records.
pub struct Decoder {
    o: Off,
    /// the game's enum values, by member name from the layout
    c: std::sync::Arc<crate::consts::Consts>,
    extra: HashMap<u8, usize>,
    slot: u64,
    sf_off: u64,
    inst_off: u64,
    pub gr: u64,
    last_walk: f64,
    last_resolve: f64,
    last_hdr: Option<Vec<u8>>,
    last_ents: f64,
    last_idle: f64,
    seen: HashSet<SeenKey>,
    seen_q: VecDeque<SeenKey>,
    pub stats: Stats,
    world: World,
    known: HashSet<u32>,
    unknown: bool,
    last_rescan: f64,
    world_key: Option<(u64, u64)>,
    rescans: u32,
    player_ptrs: Vec<(u64, u32)>,
    boss_ptrs: Vec<(u64, u32)>,
    props_seen: HashSet<u32>,
}

impl Decoder {
    /// `base` = load address of GameAssembly.dll in the target.
    pub fn new(lay: &Layout, base: u64) -> Decoder {
        Decoder {
            o: Off::new(lay), c: crate::consts::install(lay), extra: lay.event_extra.clone(), slot: base + lay.typeinfo_rva,
            sf_off: lay.static_fields, inst_off: lay.instance_off, gr: 0, last_walk: 0.0, last_resolve: 0.0,
            last_hdr: None, last_ents: 0.0, last_idle: 0.0, seen: HashSet::new(), seen_q: VecDeque::new(),
            stats: Stats::default(), world: World::new(lay), known: HashSet::new(), unknown: false, last_rescan: 0.0,
            world_key: None, rescans: 0, player_ptrs: Vec::new(), boss_ptrs: Vec::new(), props_seen: HashSet::new(),
        }
    }

    // ---- pointer chain ----
    fn resolve(&mut self, m: &mut dyn Mem) -> u64 {
        let ti = m.u64(self.slot);
        if ti == 0 || ti & self.o.slot_unresolved != 0 {
            // the class slot is not initialised yet (game still starting)
            self.gr = 0;
            return 0;
        }
        let sf = m.u64(ti + self.sf_off);
        self.gr = if sf != 0 { m.u64(sf + self.inst_off) } else { 0 };
        self.gr
    }

    // ---- one poll ----
    /// Read the current frame if it is new. Appends records to `out`. Returns true when a new frame was read.
    pub fn poll(&mut self, m: &mut dyn Mem, out: &mut Vec<Rec>) -> bool {
        let wall = m.begin_frame();
        if self.gr == 0 || wall - self.last_resolve > RESOLVE_PERIOD {
            self.last_resolve = wall;
            if self.resolve(m) == 0 {
                m.end_frame(true);
                return false;
            }
        }
        let Some(hdr) = m.read(self.gr + self.o.gr_cur, self.o.hdr_size) else {
            self.gr = 0;
            m.end_frame(false);
            return false;
        };
        if self.last_hdr.as_deref() == Some(&hdr[..]) {
            // nothing new from the simulation (loading screen, menus). Twice a second still report the game's
            // result data, which is filled in while the level is being left
            if wall - self.last_idle >= 0.5 {
                self.last_idle = wall;
                out.push(Rec::Idle { wall, res: self.result(m) });
                m.end_frame(true);
            } else {
                m.end_frame(false);
            }
            return false;
        }
        let o = &self.o;
        let nev = le_u32(&hdr, o.r_event_count);
        let (evp, edp) = (le_u64(&hdr, o.r_events), le_u64(&hdr, o.r_event_data));
        let frame = Frame {
            wall,
            t: le_f64(&hdr, o.r_time), ct: le_f64(&hdr, o.r_client_time),
            cs: le_u32(&hdr, o.r_client_state), cid: le_u32(&hdr, o.r_client_id),
            paused: hdr[o.r_client_paused], ws: hdr[o.r_world_state], wf: hdr[o.r_world_flags],
            snap: le_u32(&hdr, o.r_snapshot), ctl: le_u32(&hdr, o.r_ctl),
            nent: le_i64(&hdr, o.r_entity_count), nenemy: le_i64(&hdr, o.r_enemy_count), nev,
        };
        let (cs, t) = (frame.cs, frame.t);
        out.push(Rec::Frame(frame));
        self.stats.frames += 1;
        if 0 < nev && nev <= MAX_EVENTS && evp != 0 {
            self.events(m, &hdr, nev as usize, evp, edp, out);
        }
        // state (health, status effects, ability slots, bosses) is sampled on every frame, like the events: effects
        // and abilities then start and stop on the frame the game changed them
        self.sample(m, &hdr, wall, t, out);
        if cs == self.c.cs_running {
            // RunningGame: definitions + loadouts of this world
            let n = out.len();
            self.world.step(m, self.gr, out);
            for r in &out[n..] {
                if let Rec::Defs(d) = r {
                    self.known = d.weapons.iter().map(|x| x.0).chain(d.abilities.iter().map(|x| x.0))
                        .chain(d.entities.iter().map(|x| x.0)).collect();
                }
            }
            if self.world.key != self.world_key {
                self.world_key = self.world.key;
                self.rescans = 0;
                self.props_seen.clear();
            }
            if self.unknown && self.world.done && wall - self.last_rescan > 15.0 && self.rescans < 4 {
                self.last_rescan = wall;
                self.unknown = false;
                self.rescans += 1;
                self.world.rescan(m);
            }
        }
        self.last_hdr = Some(hdr);
        m.end_frame(true);
        true
    }

    fn events(&mut self, m: &mut dyn Mem, hdr: &[u8], nev: usize, evp: u64, edp: u64, out: &mut Vec<Rec>) {
        let stride = self.o.ev_stride;
        let Some(evs) = m.read(evp, nev * stride) else {
            self.stats.bad += 1;
            return;
        };
        let mut total = 0usize;
        for i in 0..nev {
            match self.extra.get(&evs[i * stride + self.o.e_type]) {
                Some(sz) => total += sz,
                None => {
                    // not a valid event type: we raced the next client_update
                    self.stats.torn += 1;
                    return;
                }
            }
        }
        let data = if total > 0 {
            match m.read(edp, total) {
                Some(d) => d,
                None => {
                    self.stats.bad += 1;
                    return;
                }
            }
        } else {
            Vec::new()
        };
        // the buffers are rewritten by the next client_update; if the header moved on while we were reading,
        // the event list and its data may not belong together
        let again = m.read(self.gr + self.o.gr_cur, self.o.hdr_size);
        let late = again.as_deref() != Some(hdr);
        if late {
            self.stats.late += 1;
        }
        let late_tag = late.then_some(1u8);
        let o = &self.o;
        let mut off = 0usize;
        for i in 0..nev {
            let b = i * stride;
            let (t, seq, scid) = (le_f64(&evs, b + o.e_time), le_u32(&evs, b + o.e_seq), le_u32(&evs, b + o.e_client));
            let (ty, src) = (evs[b + o.e_type], evs[b + o.e_source]);
            let sz = self.extra[&ty];
            // a hit is told apart by its target; any other event by the start of its data
            let klen = if ty == o.t_hit { o.h_key_len } else { sz.min(8) }.min(16);
            let mut kb = [0u8; 16];
            kb[..klen].copy_from_slice(&data[off..off + klen]);
            let key: SeenKey = (seq, src, ty, t.to_bits(), kb, klen as u8);
            if !self.seen.insert(key) {
                self.stats.dup += 1;
                off += sz;
                continue;
            }
            self.seen_q.push_back(key);
            if self.seen_q.len() > 60000 {
                if let Some(old) = self.seen_q.pop_front() {
                    self.seen.remove(&old);
                }
            }
            let ev = Ev { t, seq, src, sc: scid, late: late_tag };
            let rec = if ty == o.t_hit {
                let attack_hash = le_u32(&data, off + o.a_data_hash);
                self.stats.hits += 1;
                if attack_hash != 0 && !self.known.contains(&attack_hash) {
                    self.unknown = true;
                }
                Some(Rec::Hit(Hit {
                    t, seq, src, sc: scid,
                    tgt: le_u32(&data, off + o.h_target), tdi: le_u32(&data, off + o.h_data_index), tdh: le_u32(&data, off + o.h_data_hash),
                    da: le_f32(&data, off + o.d_attempted), dhu: le_f32(&data, off + o.d_health_uncapped),
                    dsu: le_f32(&data, off + o.d_shield_uncapped), dh: le_f32(&data, off + o.d_health),
                    ds: le_f32(&data, off + o.d_shield), res: le_f32(&data, off + o.d_resistance),
                    ress: le_f32(&data, off + o.d_resistance_shield),
                    fl: le_u16(&data, off + o.d_flags), dt: data[off + o.d_type],
                    ac: le_u32(&data, off + o.a_client), ai: le_u32(&data, off + o.a_index),
                    at: data[off + o.a_type], ad: (le_u32(&data, off + o.a_data_index), attack_hash), af: data[off + o.a_flags],
                    pos: (le_f32(&data, off + o.h_x), le_f32(&data, off + o.h_y), le_f32(&data, off + o.h_z)),
                    late: late_tag,
                }))
            } else if ty == o.t_heal {
                Some(Rec::Heal { ev, tgt: le_u32(&data, off + o.he_entity), ha: le_f32(&data, off + o.he_attempted),
                    hp: le_f32(&data, off + o.he_applied), rev: data[off + o.he_revived] })
            } else if ty == o.t_abil {
                Some(Rec::Abil { ev, slot: le_u32(&data, off) })
            } else if ty == o.t_gk {
                Some(Rec::Gkill { ev, n: data[off + o.gk_kills], ac: le_u32(&data, off + o.gk_client), at: data[off + o.gk_type],
                    ad: (le_u32(&data, off + o.gk_data_index), le_u32(&data, off + o.gk_data_hash)) })
            } else if ty == o.t_fire {
                Some(Rec::Fire { ev })
            } else if ty == o.t_reload {
                Some(Rec::Reload { ev, ent: le_u32(&data, off) })
            } else if ty == o.t_reloadb {
                Some(Rec::Reloadb { ev, ent: le_u32(&data, off) })
            } else {
                None
            };
            out.extend(rec);
            off += sz;
            self.stats.events += 1;
        }
    }

    // ---- state sample ----
    fn sample(&mut self, m: &mut dyn Mem, hdr: &[u8], wall: f64, t: f64, out: &mut Vec<Rec>) {
        let o = &self.o;
        let ncl = le_u32(hdr, o.r_client_count) as usize;
        let clp = le_u64(hdr, o.r_clients);
        let mut clients = Vec::new();
        if 0 < ncl && ncl <= MAX_CLIENTS && clp != 0 {
            if let Some(raw) = m.read(clp, ncl * o.ci_size) {
                for i in 0..ncl {
                    let c = i * o.ci_size;
                    let na = raw[c + o.ci_attacks_count] as usize;
                    let ap = le_u64(&raw, c + o.ci_attacks);
                    let mut attacks = Vec::new();
                    if na > 0 && ap != 0 {
                        if let Some(ab) = m.read(ap, na * o.at_stride) {
                            attacks = (0..na).map(|j| (ab[j * o.at_stride + o.at_type], le_u32(&ab, j * o.at_stride + o.at_data))).collect();
                        }
                    }
                    let nau = raw[c + o.ci_uses_count] as usize;
                    let aup = le_u64(&raw, c + o.ci_uses);
                    let mut uses = Vec::new();
                    if nau > 0 && aup != 0 {
                        if let Some(ub) = m.read(aup, nau * o.au_stride) {
                            uses = (0..nau).map(|j| {
                                let u = j * o.au_stride;
                                let n = o.au_u16.map(|f| le_u16(&ub, u + f));
                                (le_u32(&ub, u + o.au_index), n[0], n[1], n[2], n[3], n[4], n[5], le_f32(&ub, u + o.au_heal), le_f32(&ub, u + o.au_distance))
                            }).collect();
                        }
                    }
                    let text = |m: &mut dyn Mem, field: usize, len: usize| m.cstr(le_u64(&raw, c + field), len);
                    clients.push(Client {
                        id: le_u32(&raw, c + o.ci_id), dc: raw[c + o.ci_dc], lvl: raw[c + o.ci_lvl], host: raw[c + o.ci_host],
                        name: text(m, o.ci_name, NAME_LEN), cls: text(m, o.ci_class, NAME_LEN), sub: text(m, o.ci_subclass, NAME_LEN),
                        nk: text(m, o.ci_nkapi, ACCOUNT_LEN),
                        kills: le_u32(&raw, c + o.ci_kills), downs: le_u32(&raw, c + o.ci_downs), acc: le_f32(&raw, c + o.ci_acc),
                        crit_kills: le_u32(&raw, c + o.ci_crit), attacks, uses,
                    });
                }
            }
        }
        let match_id = m.cstr(le_u64(hdr, self.o.r_match_id), MATCH_LEN);
        let full = wall - self.last_ents >= ENTITY_PERIOD;
        if full {
            self.last_ents = wall;
        }
        let ents = self.entities(m, hdr, wall, full, clients.len());
        out.push(Rec::Sample(Sample { wall, t, match_id, clients, ents, res: self.result(m) }));
    }

    /// GameRunner.resultData: (valid, time, objective result, leave reason). `time` is the mission time the game
    /// itself shows on the result screen: world time (intro and outro included) at the moment the level is left.
    fn result(&self, m: &mut dyn Mem) -> Option<GameResult> {
        let o = &self.o;
        let b = m.read(self.gr + o.gr_result, o.rd_window)?;
        Some((b[o.rd_valid], le_f64(&b, o.rd_time), b[o.rd_objective], le_i32(&b, o.rd_leave)))
    }

    /// Player entities: health, shield, status effects, ability slots, weapons, the game's own damage total.
    ///
    /// The entity list is an array of pointers. Walking all of it costs one small read per entity, so that happens
    /// about once a second (census + finding the players); in between only the known player entities are re-read
    /// and re-validated (type and id), falling back to a full walk when one of them moved.
    fn entities(&mut self, m: &mut dyn Mem, hdr: &[u8], wall: f64, full: bool, nclients: usize) -> Option<Ents> {
        let n = le_i64(hdr, self.o.r_entity_count);
        let p = le_u64(hdr, self.o.r_entities);
        if !(0 < n && n <= MAX_ENTITIES && p != 0) {
            self.player_ptrs.clear();
            self.props_seen.clear();
            return None;
        }
        let pointer = std::mem::size_of::<u64>();
        let mut res = Ents { n, ..Default::default() };
        // a player without a known entity (just joined, or its slot was reused) also triggers the walk, but not on
        // every frame: the walk is one read per entity
        if full || (self.player_ptrs.len() < nclients && wall - self.last_walk >= WALK_RETRY) {
            self.last_walk = wall;
            let raw = m.read(p, n as usize * pointer)?;
            let (mut census, mut found, mut bosses, mut props) = (BTreeMap::new(), Vec::new(), Vec::new(), Vec::new());
            for i in 0..n as usize {
                let q = le_u64(&raw, i * pointer);
                if q == 0 {
                    continue;
                }
                let Some(e) = m.read(q, self.o.ge_head) else { continue };
                let et = e[self.o.ge_type];
                *census.entry(et).or_insert(0u32) += 1;
                let eid = le_u32(&e, self.o.ge_id);
                if et == self.c.et_player {
                    upsert(&mut found, q, eid);
                } else if et == self.c.et_enemy && le_u32(&e, self.o.ge_flags) & self.c.flag_boss != 0 {
                    upsert(&mut bosses, q, eid); // boss: sampled below, like a player
                } else if et == self.c.et_destructible && !self.props_seen.contains(&eid) {
                    // destructible level prop: its tag
                    if let Some(big) = m.read(q, self.o.ge_prop_window) {
                        self.props_seen.insert(eid);
                        props.push((eid, le_u32(&big, self.o.ge_data_hash), le_i32(&big, self.o.ge_tag),
                            le_i32(&big, self.o.ge_prop), le_f32(&big, self.o.ge_hp_max)));
                    }
                }
            }
            self.player_ptrs = found;
            self.boss_ptrs = bosses;
            res.census = Some(census);
            res.full = true;
            res.props = Some(props);
        }
        let o = &self.o;
        let mut keep = Vec::new();
        for &(q, eid) in &self.player_ptrs {
            let e = match m.read(q, o.ge_size) {
                Some(e) if e[o.ge_type] == self.c.et_player && le_u32(&e, o.ge_id) == eid => e,
                _ => continue, // the slot was reused: the next sample walks the list again
            };
            keep.push((q, eid));
            let sub = le_u64(&e, o.ge_subclass);
            let pg = if sub != 0 { m.read(sub, o.pg_size) } else { None };
            res.players.push(PlayerEnt {
                id: eid, client: le_u32(&e, o.ge_client),
                hp: le_f32(&e, o.ge_hp), hpmax: le_f32(&e, o.ge_hp_max),
                sh: le_f32(&e, o.ge_shield), shmax: le_f32(&e, o.ge_shield_max),
                flags: le_u32(&e, o.ge_flags), downs: e[o.ge_downed], aw: e[o.ge_active_weapon],
                dmg: pg.map(|pg| le_f32(&pg, o.pg_total)),
                fx: effects(o, m, &e), ab: abilities(o, m, &e), wp: weapons(o, m, &e),
            });
        }
        self.player_ptrs = keep;
        let mut keep = Vec::new();
        for &(q, eid) in &self.boss_ptrs {
            let e = match m.read(q, o.ge_size) {
                Some(e) if e[o.ge_type] == self.c.et_enemy && le_u32(&e, o.ge_id) == eid => e,
                _ => continue,
            };
            keep.push((q, eid));
            // the enemy part of the entity points at its enemy data: active mods mask, stage index, mod level
            let sub = le_u64(&e, o.ge_subclass);
            let ed = if sub != 0 { m.u64(sub + o.eg_data as u64) } else { 0 };
            let eb = if ed != 0 { m.read(ed, o.ed_size) } else { None };
            res.bosses.push(BossEnt {
                id: eid, hash: le_u32(&e, o.ge_data_hash),
                hp: le_f32(&e, o.ge_hp), hpmax: le_f32(&e, o.ge_hp_max),
                sh: le_f32(&e, o.ge_shield), shmax: le_f32(&e, o.ge_shield_max),
                flags: le_u32(&e, o.ge_flags), spawn: le_f64(&e, o.ge_spawn), death: le_f64(&e, o.ge_death),
                mods: eb.map(|eb| (le_u32(&eb, o.ed_mask), eb[o.ed_stage], eb[o.ed_level])), fx: effects(o, m, &e),
            });
        }
        self.boss_ptrs = keep;
        Some(res)
    }
}

/// Keeps one entry per pointer, in first-seen order (the Python capture uses an insertion-ordered dict).
fn upsert(list: &mut Vec<(u64, u32)>, q: u64, eid: u32) {
    match list.iter_mut().find(|(p, _)| *p == q) {
        Some(e) => e.1 = eid,
        None => list.push((q, eid)),
    }
}

/// Elements of a native vector (first element, past the end) embedded in a struct.
fn vec_of(o: &Off, m: &mut dyn Mem, e: &[u8], at: usize, stride: usize, cap: usize) -> Option<(Vec<u8>, usize)> {
    let (b, end) = (le_u64(e, at + o.vec_begin), le_u64(e, at + o.vec_end));
    let n = if b != 0 && end >= b { ((end - b) / stride as u64) as usize } else { 0 };
    if !(0 < n && n <= cap) {
        return None;
    }
    Some((m.read(b, n * stride)?, n))
}

fn effects(o: &Off, m: &mut dyn Mem, e: &[u8]) -> Vec<Effect> {
    let Some((raw, n)) = vec_of(o, m, e, o.ge_effects, o.se_stride, MAX_EFFECTS) else { return Vec::new() };
    (0..n).map(|i| {
        let s = i * o.se_stride;
        // who applied it: the attack type and the definition it came from
        (le_u16(&raw, s + o.se_id), le_u32(&raw, s + o.se_set_hash), raw[s + o.se_type],
         le_f32(&raw, s + o.se_apply), le_f32(&raw, s + o.se_duration), raw[s + o.se_state], raw[s + o.se_susp],
         raw[s + o.se_level], le_u32(&raw, s + o.se_giver), raw[s + o.se_attack_type], le_u32(&raw, s + o.se_attack_hash))
    }).collect()
}

fn abilities(o: &Off, m: &mut dyn Mem, e: &[u8]) -> Vec<Ability> {
    let Some((raw, n)) = vec_of(o, m, e, o.ge_abilities, o.as_stride, MAX_ABILITIES) else { return Vec::new() };
    (0..n).map(|i| {
        let s = i * o.as_stride;
        (le_u32(&raw, s + o.as_hash), le_f32(&raw, s + o.as_trigger),
         (le_f32(&raw, s + o.as_cooldown), le_f32(&raw, s + o.as_cooldown_start)),
         (le_u16(&raw, s + o.as_remaining), le_u16(&raw, s + o.as_total)), raw[s + o.as_state])
    }).collect()
}

fn weapons(o: &Off, m: &mut dyn Mem, e: &[u8]) -> Vec<Weapon> {
    let Some((raw, n)) = vec_of(o, m, e, o.ge_weapons, o.ws_stride, MAX_WEAPONS) else { return Vec::new() };
    (0..n).map(|i| {
        let s = i * o.ws_stride;
        (le_u32(&raw, s + o.ws_hash), le_u32(&raw, s + o.ws_index), le_u16(&raw, s + o.ws_ammo), raw[s + o.ws_state],
         le_u16(&raw, s + o.ws_fire))
    }).collect()
}
