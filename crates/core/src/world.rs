//! Slow-changing world data read once per mission: definition tables (for names), what every entity can take
//! (health, resistances) and every player's loadout.
//!
//! All of it comes from objects the game builds when a level loads:
//!
//! ```text
//! GameRunner.worldController
//!   .buildDataResult (Nullable<BuildDataResult>)   lists of the definitions baked into this mission; list index ==
//!                                                  native data index (WorldLoadDataId.index)
//!   .loadDataPtr                                   native "world load data" blob: one uint[] of identity hashes per
//!                                                  collection, the GameEntityData rows and the resistance tables
//!   .loadDataBuilder.buildParams.players           the PlayerInfo list the mission was built from (all players)
//! ```
//!
//! Every offset, struct size and container layout comes from the layout of the running game build; the numbers
//! in this file are caps against garbage and the size of a float.
//!
//! The walk runs on its own thread with its own reader, so thousands of small reads never delay the per-frame
//! event read.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{channel, Receiver};

use crate::layout::{Layout, St};
use crate::mem::{le_f32, le_i32, le_u32, le_u64, Mem};
use crate::records::*;

// ---- the meter's own choices (not game values) ----
/// Longest list read (definitions of one kind in a mission) and longest text, against garbage.
const MAX_LIST: i32 = 4096;
const MAX_TEXT: usize = 512;
const MAX_ACCOUNT: usize = 64;
/// Caps on the row counts of the load-data tables.
const MAX_ENTITY_ROWS: usize = 8192;
const MAX_TABLE_ROWS: usize = 4096;
/// Entity rows read per chunk.
const CHUNK: usize = 128;
/// The resistance tables hold single-precision floats (the game declares them as raw bytes).
const FLOAT: usize = std::mem::size_of::<f32>();

/// Python's `round(x, nd)`: correctly rounded on the exact binary value.
fn rnd(x: f64, nd: usize) -> f64 {
    if !x.is_finite() { x } else { format!("{x:.nd$}").parse().unwrap_or(x) }
}

/// How the runtime lays out managed objects, from the layout.
#[derive(Clone)]
struct Runtime {
    pointer: usize,
    header: usize,
    str_len: usize,
    str_chars: usize,
    char_size: usize,
    arr_len: usize,
    arr_data: usize,
    list_items: usize,
    list_size: usize,
}

#[derive(Clone)]
struct Offsets {
    rt: Runtime,
    wc: St, b: St, w: St, a: St, e: St, ss: St, sg: St, bm: St, lm: St, lb: St, bp: St, pi: St, lg: St, lgm: St,
    em: St, sp: St, up: St, en: St, nullable: St, hdr: St, ged: St, ed: St,
    /// the loaded level (a game build without the class leaves the level's type unknown)
    lv: Option<St>,
    /// bytes per row of the damage-resistance and effect-resistance tables
    res_row: usize, eres_row: usize,
}

impl Offsets {
    fn new(l: &Layout) -> Offsets {
        let c = |n: &str| l.st(n).clone();
        Offsets {
            rt: Runtime {
                pointer: l.rt("object", "pointer_size"), header: l.rt("object", "header_size"),
                str_len: l.rt("string", "length"), str_chars: l.rt("string", "chars"), char_size: l.rt("string", "char_size"),
                arr_len: l.rt("array", "length"), arr_data: l.rt("array", "elements"),
                list_items: l.rt("list", "items"), list_size: l.rt("list", "size"),
            },
            wc: c("WorldController"), b: c("BuildDataResult"), w: c("WeaponDefinitionSO"), a: c("AbilityDefinitionSO"),
            e: c("EntityDefinitionSO"), ss: c("StatusEffectSetDefinitionSO"), sg: c("StatGroupKills"), bm: c("BossModDefinitionSO"),
            lm: c("LevelMode"), lb: c("WorldLoadDataBuilder"), bp: c("WorldDataBuildParams"), pi: c("PlayerInfo"), lg: c("LoadoutGear"),
            lgm: c("LoadoutGearMod"), em: c("EnemyModDefinitionSO"), sp: c("SkillPoint"), up: c("AbilityUpgradesApplied"),
            en: c("AbilityEnhancements"), nullable: c("Nullable<BuildDataResult>"), hdr: c("LoadHeader"), ged: c("GameEntityData"),
            ed: c("EnemyData"), lv: l.try_st("Level").cloned(),
            res_row: l.st("DamageResistanceData").size(), eres_row: l.st("EffectResistanceData").size(),
        }
    }
}

/// What one walk produced. `retry` = the level was not built yet: start again on a later frame.
struct Walk {
    generation: u64,
    retry: bool,
    recs: Vec<Rec>,
}

pub struct World {
    off: Offsets,
    gr_wc: u64,
    /// (worldController, loadDataPtr) of the world the walk was started for
    pub key: Option<(u64, u64)>,
    pub done: bool,
    generation: u64,
    pending: Option<Receiver<Walk>>,
}

impl World {
    pub fn new(lay: &Layout) -> World {
        World { off: Offsets::new(lay), gr_wc: lay.st("GameRunner").o("worldController") as u64, key: None, done: false,
            generation: 0, pending: None }
    }

    /// Called once per new frame. Starts the walk when a new world appears and hands over its records.
    pub fn step(&mut self, mem: &mut dyn Mem, gr: u64, out: &mut Vec<Rec>) {
        let wc = mem.u64(gr + self.gr_wc);
        let blob = if wc != 0 { mem.u64(wc + self.off.wc.o("loadDataPtr") as u64) } else { 0 };
        let key = Some((wc, blob));
        if key != self.key {
            self.key = key;
            self.done = false;
            self.pending = None;
            self.generation += 1;
            if wc != 0 && blob != 0 {
                self.start(mem, wc, blob);
            } else {
                self.done = true;
            }
        }
        self.collect(out);
    }

    /// Run the walk again for the current world (e.g. an unknown id showed up).
    pub fn rescan(&mut self, mem: &mut dyn Mem) {
        if let (true, Some((wc, blob))) = (self.done, self.key) {
            if wc != 0 && blob != 0 {
                self.done = false;
                self.generation += 1;
                self.start(mem, wc, blob);
            }
        }
    }

    fn start(&mut self, mem: &mut dyn Mem, wc: u64, blob: u64) {
        let (off, generation) = (self.off.clone(), self.generation);
        let (tx, rx) = channel();
        self.pending = Some(rx);
        match mem.fork() {
            Some(mut reader) => {
                std::thread::Builder::new().name("zar-logs-world".into()).spawn(move || {
                    let _ = tx.send(walk(&off, reader.as_mut(), wc, blob, generation));
                }).ok();
            }
            None => {
                let _ = tx.send(walk(&off, mem, wc, blob, generation));
            }
        }
    }

    fn collect(&mut self, out: &mut Vec<Rec>) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(w) if w.generation == self.generation => {
                self.pending = None;
                if w.retry {
                    self.key = None; // not built yet: try again on a later frame
                } else {
                    self.done = true;
                }
                out.extend(w.recs);
            }
            Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pending = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }
}

fn walk(off: &Offsets, m: &mut dyn Mem, wc: u64, blob: u64, generation: u64) -> Walk {
    let mut recs = Vec::new();
    let Some(defs) = defs_job(off, m, wc, blob) else {
        return Walk { generation, retry: true, recs };
    };
    recs.push(Rec::Defs(Box::new(defs)));
    let players = players_job(off, m, wc);
    if !players.is_empty() {
        recs.push(Rec::Builds { players });
    }
    Walk { generation, retry: false, recs }
}

// ---- managed containers ----

/// (bytes, count) of a `List<T>` or `T[]` whose elements are `stride` bytes. A list keeps its items array where
/// an array keeps its (unused) bounds pointer, so a non-null value there means "list".
fn array(rt: &Runtime, m: &mut dyn Mem, obj: u64, stride: usize) -> Option<(Vec<u8>, usize)> {
    if obj == 0 {
        return None;
    }
    let head = m.read(obj, rt.arr_data)?;
    let items = le_u64(&head, rt.list_items);
    let (arr, n) = if items != 0 { (items, le_i32(&head, rt.list_size)) } else { (obj, le_i32(&head, rt.arr_len)) };
    if !(0 < n && n <= MAX_LIST) {
        return None;
    }
    let raw = m.read(arr + rt.arr_data as u64, n as usize * stride)?;
    Some((raw, n as usize))
}

/// Element pointers of a `List<T>` / `T[]` of references.
fn refs(rt: &Runtime, m: &mut dyn Mem, obj: u64) -> Vec<u64> {
    match array(rt, m, obj, rt.pointer) {
        Some((raw, n)) => (0..n).map(|i| le_u64(&raw, i * rt.pointer)).collect(),
        None => Vec::new(),
    }
}

/// A managed string: its length, then its UTF-16 characters.
fn mstr(rt: &Runtime, m: &mut dyn Mem, obj: u64, maxlen: usize) -> String {
    if obj == 0 {
        return String::new();
    }
    let n = m.u32(obj + rt.str_len as u64) as usize;
    if n == 0 || n > maxlen {
        return String::new();
    }
    match m.read(obj + rt.str_chars as u64, n * rt.char_size) {
        Some(b) => String::from_utf16_lossy(&b.chunks_exact(rt.char_size).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()),
        None => String::new(),
    }
}

fn strs(rt: &Runtime, m: &mut dyn Mem, obj: u64) -> Vec<String> {
    refs(rt, m, obj).into_iter().map(|p| mstr(rt, m, p, MAX_TEXT)).collect()
}

/// The identity hashes of one collection of the load-data blob ("weapon_data", "entity_data", ...), by the header's
/// own count and offset fields for it.
fn hashes(o: &Offsets, m: &mut dyn Mem, blob: u64, collection: &str) -> Vec<u32> {
    let n = m.u32(blob + o.hdr.o(&format!("{collection}_count")) as u64);
    let off = m.u32(blob + o.hdr.o(&format!("{collection}_hash_offset")) as u64);
    if !(0 < n && n <= MAX_LIST as u32 && 0 < off && off < 1 << 28) {
        return Vec::new();
    }
    let word = std::mem::size_of::<u32>();
    match m.read(blob + off as u64, word * n as usize) {
        Some(raw) => (0..n as usize).map(|i| le_u32(&raw, i * word)).collect(),
        None => Vec::new(),
    }
}

// ---- definition tables ----

fn defs_job(o: &Offsets, m: &mut dyn Mem, wc: u64, blob: u64) -> Option<Defs> {
    let rt = &o.rt;
    // Nullable<BuildDataResult>: a has-value flag, then the value
    let b_at = wc + o.wc.o("buildDataResult") as u64;
    let has = m.read(b_at + o.nullable.o("hasValue") as u64, 1)?;
    if has[0] == 0 {
        return None;
    }
    let head = m.read(b_at + o.nullable.o("value") as u64, o.b.size())?;
    let ptr = |name: &str| le_u64(&head, o.b.o(name));
    // the text a field of an object points at
    let text_at = |m: &mut dyn Mem, obj: u64, field: usize| { let t = m.u64(obj + field as u64); mstr(rt, m, t, MAX_TEXT) };
    let lm = ptr("levelMode");
    // the level object says what it is itself (hub, mission, boss raid): a story mission has no mode, like the hub
    let level_type = match (&o.lv, o.b.has("level")) {
        (Some(lv), true) if lv.has("levelType") => Some(ptr("level")).filter(|p| *p != 0)
            .and_then(|p| m.read(p + lv.o("levelType") as u64, 1)).map(|b| b[0]),
        _ => None,
    };
    let mut rec = Defs {
        level: mstr(rt, m, ptr("levelId"), MAX_TEXT),
        level_type,
        mode: mstr(rt, m, ptr("modeId"), MAX_TEXT),
        difficulty: head[o.b.o("levelDifficulty")],
        modifier: head[o.b.o("levelModifier")],
        mode_title: if lm != 0 { text_at(m, lm, o.lm.o("title")) } else { String::new() },
        avg_level: le_i32(&head, o.b.o("averagePlayerLevel")),
        ..Default::default()
    };

    let hw = hashes(o, m, blob, "weapon_data");
    // a weapon definition's own id and the id of the definition it was copied from, side by side
    let (w_id, w_orig) = (o.w.o("id"), o.w.o("originalWeaponID"));
    let w_window = w_id.max(w_orig) + std::mem::size_of::<u32>() - w_id.min(w_orig);
    for (i, p) in refs(rt, m, ptr("weaponDefinitions")).into_iter().enumerate() {
        let d = if p != 0 { m.read(p + w_id.min(w_orig) as u64, w_window) } else { None };
        let (own, orig) = d.map(|d| (le_u32(&d, w_id - w_id.min(w_orig)), le_u32(&d, w_orig - w_id.min(w_orig)))).unwrap_or((0, 0));
        let title = if p != 0 { text_at(m, p, o.w.o("title")) } else { String::new() };
        rec.weapons.push((hw.get(i).copied().unwrap_or(0), own, orig, title));
    }

    let ha = hashes(o, m, blob, "ability_data");
    for (i, p) in refs(rt, m, ptr("abilityDefinitions")).into_iter().enumerate() {
        let orig = if p != 0 { m.u32(p + o.a.o("originalAbilityID") as u64) } else { 0 };
        let title = if p != 0 { text_at(m, p, o.a.o("title")) } else { String::new() };
        rec.abilities.push((ha.get(i).copied().unwrap_or(0), orig, title));
    }

    let mut groups: HashMap<u64, i32> = HashMap::new();
    for (i, p) in refs(rt, m, ptr("statGroups")).into_iter().enumerate() {
        match if p != 0 { m.read(p, o.sg.size()) } else { None } {
            Some(sg) => {
                let (sid, t, ts) = (le_u64(&sg, o.sg.o("id")), le_u64(&sg, o.sg.o("title")), le_u64(&sg, o.sg.o("titleSingular")));
                rec.statgroups.push((mstr(rt, m, sid, MAX_TEXT), mstr(rt, m, t, MAX_TEXT), mstr(rt, m, ts, MAX_TEXT), sg[o.sg.o("isBoss")]));
                groups.insert(p, i as i32);
            }
            None => rec.statgroups.push((String::new(), String::new(), String::new(), 0)),
        }
    }
    let he = hashes(o, m, blob, "entity_data");
    let ksg = o.e.o("killStatGroup");
    for (i, p) in refs(rt, m, ptr("entityDefinitions")).into_iter().enumerate() {
        let h = he.get(i).copied().unwrap_or(0);
        match if p != 0 { m.read(p, ksg + rt.pointer) } else { None } {
            Some(d) => rec.entities.push((h, le_u32(&d, o.e.o("id")), le_u32(&d, o.e.o("originalID")), d[o.e.o("type")], d[o.e.o("isBoss")],
                groups.get(&le_u64(&d, ksg)).copied().unwrap_or(-1))),
            None => rec.entities.push((h, 0, 0, 0, 0, -1)),
        }
    }

    let hs = hashes(o, m, blob, "status_effect_set_data");
    for (i, p) in refs(rt, m, ptr("statusEffectSetDefinitions")).into_iter().enumerate() {
        let own = if p != 0 { m.u32(p + o.ss.o("id") as u64) } else { 0 };
        let orig = if p != 0 { m.u32(p + o.ss.o("originalSetID") as u64) } else { 0 };
        rec.sets.push((hs.get(i).copied().unwrap_or(0), own, orig));
    }
    for p in refs(rt, m, ptr("bossMods")) {
        rec.bossmods.push(if p != 0 { text_at(m, p, o.bm.o("displayName")) } else { String::new() });
    }
    for p in refs(rt, m, ptr("enemyModDefinitions")) {
        rec.enemymods.push(if p != 0 { text_at(m, p, o.em.o("displayName")) } else { String::new() });
    }
    rec.defense = defense(o, m, blob);
    // GameEntity.tag_id indexes this list (pods, objectives ...)
    rec.tags = strs(rt, m, ptr("tags"));
    rec.tags.truncate(MAX_TEXT);
    Some(rec)
}

/// Health, shield and resistances of every damageable entity definition of this mission, as the simulation uses
/// them: GameEntityData rows of the load-data blob, which index shared tables of damage resistances (one float per
/// damage type) and effect resistances (apply chance + strength per effect category). Elite and stage variants
/// are separate rows, so their modifiers and scaled health are already in.
fn defense(o: &Offsets, m: &mut dyn Mem, blob: u64) -> Option<Defense> {
    let (h, g) = (&o.hdr, &o.ged);
    let hdr = m.read(blob, h.size())?;
    // (offset of the table in the blob, row count), from the header's fields for that collection
    let table = |name: &str| (le_u32(&hdr, h.o(&format!("{name}_offset"))) as u64, le_u32(&hdr, h.o(&format!("{name}_count"))) as usize);
    let ((eo, en), (no, nn)) = (table("entity_data"), table("enemy_data"));
    let ((ro, rn), (fo, fn_)) = (table("damage_resistance_data"), table("effect_resistance_data"));
    if !(0 < en && en <= MAX_ENTITY_ROWS) || rn > MAX_TABLE_ROWS || fn_ > MAX_TABLE_ROWS || nn > MAX_ENTITY_ROWS {
        return None;
    }
    let hashes = hashes(o, m, blob, "entity_data");
    let res = if rn > 0 { m.read(blob + ro, rn * o.res_row)? } else { Vec::new() };
    let eres = if fn_ > 0 { m.read(blob + fo, fn_ * o.eres_row)? } else { Vec::new() };
    let enemy_row = o.ed.size();
    let enemy = if nn > 0 { m.read(blob + no, nn * enemy_row)? } else { Vec::new() };
    let stride = g.size();
    let mut ents: BTreeMap<u32, EntDefense> = BTreeMap::new();
    let mut base = 0usize;
    while base < en {
        let n = (en - base).min(CHUNK);
        let raw = m.read(blob + eo + (base * stride) as u64, n * stride)?;
        for i in 0..n {
            let at = i * stride;
            let f = |name: &str| le_f32(&raw, at + g.o(name));
            let index = |name: &str, rows: usize| { let v = le_u32(&raw, at + g.o(name)) as usize; if v < rows { v as i32 } else { -1 } };
            let (hp, sh) = (f("max_health"), f("max_shield_health"));
            let hash = hashes.get(base + i).copied().unwrap_or(0);
            if hash == 0 || ents.contains_key(&hash) || !(hp > 0.0) {
                continue;
            }
            let ei = le_u32(&raw, at + g.o("enemy_data_index")) as usize;
            let (mask, stage, lvl) = if ei < nn {
                let e = ei * enemy_row;
                (le_u32(&enemy, e + o.ed.o("active_mods_mask")), enemy[e + o.ed.o("stage_index")], enemy[e + o.ed.o("mod_level")])
            } else {
                (0, 0, 0)
            };
            ents.insert(hash, EntDefense {
                hp: rnd(hp, 2), shield: rnd(sh, 2), res: index("damage_resistance_index", rn),
                shield_res: index("shield_damage_resistance_index", rn), effect_res: index("effect_resistance_index", fn_),
                explosion: rnd(f("explosion_resistance"), 4), dodge: rnd(f("dodge_chance"), 4),
                elemental: rnd(f("elemental_effictiveness_reduction"), 4), regen: rnd(f("health_regen_percent"), 5),
                mods: mask, stage, mod_level: lvl, entity_type: raw[at + g.o("entity_type")],
            });
        }
        base += n;
    }
    // the four difficulty multipliers sit side by side in the header, starting at the base one
    let mult_at = h.o("base_difficulty_multiplier");
    let mult_n = (h.o("players_obj_difficulty_multiplier") - mult_at) / FLOAT + 1;
    let floats = |raw: &[u8], row: usize, width: usize| (0..width / FLOAT).map(|k| rnd(le_f32(raw, row * width + k * FLOAT), 4)).collect::<Vec<_>>();
    Some(Defense {
        res: (0..rn).map(|i| floats(&res, i, o.res_row)).collect(),
        eres: (0..fn_).map(|i| floats(&eres, i, o.eres_row)).collect(),
        ents,
        mult: (0..mult_n).map(|k| rnd(le_f32(&hdr, mult_at + k * FLOAT), 4)).collect(),
    })
}

// ---- loadouts ----

fn mods(o: &Offsets, m: &mut dyn Mem, arr: u64) -> Vec<GearMod> {
    let stride = o.lgm.size();
    let Some((raw, n)) = array(&o.rt, m, arr, stride) else { return Vec::new() };
    (0..n).map(|i| {
        let at = i * stride;
        GearMod {
            id: mstr(&o.rt, m, le_u64(&raw, at + o.lgm.o("itemId")), MAX_TEXT),
            quality: raw[at + o.lgm.o("quality")],
            offset: le_i32(&raw, at + o.lgm.o("powerLevelOffset")),
            enh: mstr(&o.rt, m, le_u64(&raw, at + o.lgm.o("enhancementId")), MAX_TEXT),
        }
    }).collect()
}

fn players_job(o: &Offsets, m: &mut dyn Mem, wc: u64) -> Vec<PlayerInfo> {
    let rt = &o.rt;
    let lb = m.u64(wc + o.wc.o("loadDataBuilder") as u64);
    let bp = if lb != 0 { m.u64(lb + o.lb.o("buildParams") as u64) } else { 0 };
    if bp == 0 {
        return Vec::new();
    }
    let list = m.u64(bp + o.bp.o("players") as u64);
    // the part of a PlayerInfo object that holds what is read here
    let window = ["nkapiId", "displayName", "level", "className", "subclassName", "loadout", "armourMods", "abilityLoadout",
        "abilityUpgrades", "abilityEnhancements", "skillPoints"].iter().map(|f| o.pi.o(f)).max().unwrap_or(0) + rt.pointer;
    let mut players = Vec::new();
    for p in refs(rt, m, list) {
        if p == 0 {
            continue;
        }
        let Some(d) = m.read(p, window) else { continue };
        let q = |name: &str| le_u64(&d, o.pi.o(name));
        let mut info = PlayerInfo {
            nk: mstr(rt, m, q("nkapiId"), MAX_ACCOUNT),
            name: mstr(rt, m, q("displayName"), MAX_TEXT),
            level: le_i32(&d, o.pi.o("level")),
            cls: mstr(rt, m, q("className"), MAX_TEXT),
            sub: mstr(rt, m, q("subclassName"), MAX_TEXT),
            ..Default::default()
        };
        let gear_stride = o.lg.size();
        if let Some((raw, n)) = array(rt, m, q("loadout"), gear_stride) {
            for i in 0..n {
                let at = i * gear_stride;
                info.gear.push(Gear {
                    slot: i,
                    id: mstr(rt, m, le_u64(&raw, at + o.lg.o("itemId")), MAX_TEXT),
                    level: le_i32(&raw, at + o.lg.o("level")),
                    kind: raw[at + o.lg.o("type")],
                    quality: raw[at + o.lg.o("quality")],
                    mods: mods(o, m, le_u64(&raw, at + o.lg.o("mods"))),
                });
            }
        }
        info.armour_mods = mods(o, m, q("armourMods"));
        info.abilities = strs(rt, m, q("abilityLoadout"));
        for a in refs(rt, m, q("abilityUpgrades")) {
            if a != 0 {
                let (ab, ids) = (m.u64(a + o.up.o("abilityId") as u64), m.u64(a + o.up.o("upgradeIds") as u64));
                info.upgrades.push(AbilityUpgrades { ability: mstr(rt, m, ab, MAX_TEXT), ids: strs(rt, m, ids) });
            }
        }
        for a in refs(rt, m, q("abilityEnhancements")) {
            if a != 0 {
                let (ab, id) = (m.u64(a + o.en.o("abilityId") as u64), m.u64(a + o.en.o("enhancementIdEquipped") as u64));
                info.enhancements.push(AbilityEnhancement { ability: mstr(rt, m, ab, MAX_TEXT), id: mstr(rt, m, id, MAX_TEXT) });
            }
        }
        // SkillPoint is a struct stored in place: its field offsets are given for a boxed copy, header included
        let (sp_stride, sp_id, sp_points) = (o.sp.size() - rt.header, o.sp.o("id") - rt.header, o.sp.o("points") - rt.header);
        if let Some((raw, n)) = array(rt, m, q("skillPoints"), sp_stride) {
            for i in 0..n {
                let sid = mstr(rt, m, le_u64(&raw, i * sp_stride + sp_id), MAX_TEXT);
                if !sid.is_empty() {
                    info.skills.push((sid, le_i32(&raw, i * sp_stride + sp_points)));
                }
            }
        }
        players.push(info);
    }
    players
}
