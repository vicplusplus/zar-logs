//! The layout generator: which structures the meter reads, and how their description is assembled.
//!
//! The meter reads the game's memory, so it needs to know where things are: the offset of every field of the
//! structs it decodes, their sizes, the numeric values of the enums in them, the global that leads to the running
//! game (`GameRunner.Instance`), the size of each game event's payload, and how the runtime lays out strings,
//! arrays and lists. All of that changes with a game update and none of it is published, but it can all be
//! derived from the game's files without running the game. The result is one JSON document per game build, so
//! that the app's own source carries no offset, size or enum value of the game.
//!
//! Up to format version 10 the document was exactly the one the Python meter generates (`zarmeter/layout.py`).
//! From version 11 on it is a superset: every key of version 10 keeps its name and value, sizes that the Python
//! generator could not compute are filled in from the binary's own size table, and there is more of everything
//! (`runtime`, more structs, all enums of the simulation's namespace).
//!
//! Offsets and sizes of value types are exported "in place" (the managed object header subtracted), because the
//! meter reads them embedded in arrays and other structs; offsets and sizes of classes count from the object
//! pointer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::il2cpp::{Kind, Types, ATTR_LITERAL, ATTR_STATIC, T_ARRAY, T_CLASS, T_GENERICINST, T_OBJECT, T_PTR, T_STRING, T_SZARRAY, T_VALUETYPE, T_VAR};
use crate::json::J;
use crate::meta::{Meta, NO_TYPE};
use crate::pe::{Module, Pe};
use crate::x86;

/// Format version of the exported document; readers reject any other.
pub const VERSION: i64 = 12;

/// Namespace of the structs shared with the game's native simulation; dropped from exported names.
const NBI: &str = "NativeBuildInterop.";
const NULLABLE: &str = "System.Nullable`1";
const LIST: &str = "System.Collections.Generic.List`1";
// element types of `char` and `int` fields
const T_CHAR: u8 = 3;
const T_INT: u8 = 8;

macro_rules! nbi {
    ($name:literal) => {
        concat!("NativeBuildInterop.", $name)
    };
}

/// Value types, read in place.
const STRUCTS: &[&str] = &[
    nbi!("ClientUpdateResult"), nbi!("GameEvent"), nbi!("HitEventData"), nbi!("HitEventDamageInfo"),
    nbi!("EntityKillAttributionInfo"), nbi!("EntityKillAttributedAttack"), nbi!("HealEventData"),
    nbi!("ActivateAbilityEventData"), nbi!("GroupedKillsEventData"), nbi!("ClientUpdateResultClientInfo"),
    nbi!("ClientUpdateResultAttributedAttack"), nbi!("ClientUpdateResultAbilityUse"), nbi!("AbilityUseData"),
    nbi!("ClientUpdateResultHealApplied"), nbi!("GameEntity"), nbi!("PlayerGameEntity"), nbi!("HealthState"),
    nbi!("ShieldState"), nbi!("StatusEffectStack"), nbi!("StatusEffect"), nbi!("StatusEffectData"),
    nbi!("WorldLoadDataId"), nbi!("AbilitySlot"), nbi!("WeaponState"), nbi!("WeaponReloadEventData"),
    nbi!("WeaponReloadBeginEventData"), nbi!("WeaponFireBeginEventData"), nbi!("WeaponDryFireEventData"),
    nbi!("WeaponForceSwitchEventData"), nbi!("FireChainAttackEventData"), nbi!("DamageLevelChangedEventData"),
    nbi!("GameObjective"), nbi!("ExtractionResultData"), "BuildDataResult", "LoadoutGear", "LoadoutGearMod",
    "GameResultData", nbi!("GameEntityData"),
    // since version 11: the enemy side of an entity, the rows of the resistance tables, positions
    nbi!("EnemyGameEntity"), nbi!("EnemyData"), nbi!("DamageResistanceData"), nbi!("EffectResistanceData"),
    nbi!("EffectResistanceDataEntry"), "CVec3",
];

/// Reference types (offsets from the object pointer).
const CLASSES: &[&str] = &[
    "GameRunner", "WorldController", "Assets.DataDefinitions.WeaponDefinitionSO", "AbilityDefinitionSO",
    "Assets.DataDefinitions.EntityDefinitionSO", "StatusEffectSetDefinitionSO", "StatGroupKills",
    "BossModDefinitionSO", "EnemyModDefinitionSO", "LevelMode", "WorldLoadDataBuilder", "PlayerInfo",
    "WorldDataBuildParams", "SkillPoint", "AbilityUpgradesApplied", "AbilityEnhancements",
    // since version 12: the loaded level itself, which says whether it is the hub or a mission
    "NinjaKiwi.ZombieAssault.LevelEditor.Level",
];

/// The enums of version 10, first and in this order so that old and new documents line up. (`Collection` never
/// matched: that enum is nested, see `export_enums`.)
const ENUMS: &[&str] = &[
    nbi!("GameEventType"), nbi!("EventSource"), nbi!("EntityKillAttributedAttackType"),
    nbi!("HitEventDamageInfoFlags"), nbi!("DamageType"), nbi!("WorldState"), nbi!("ClientState"),
    nbi!("GameEntityType"), nbi!("MissionType"), nbi!("StatusEffectType"), nbi!("StatusEffectState"),
    nbi!("AbilitySlotState"), nbi!("GameEntityFlags"), nbi!("WeaponStateType"), nbi!("StatusEffectGroups"),
    nbi!("HealthStateFlags"), nbi!("GameResultType"), nbi!("GameObjectiveState"), nbi!("GameObjectiveType"),
    nbi!("LevelDifficulty"), nbi!("LevelModifier"), nbi!("Collection"), nbi!("GameEntityDataFlags"),
];

/// Enums wanted by name wherever they live (the simulation's namespace is exported whole; these are for the
/// ones outside it).
const EXTRA_ENUMS: &[&str] = &["ItemQuality", "GearType", "WeaponCategory", "LevelDifficulty", "LevelType"];

/// Size (= alignment) of the primitive element types: bool, char, the integers, float, double, IntPtr, UIntPtr.
fn primitive_size(ty: u8) -> Option<i64> {
    Some(match ty {
        2 | 4 | 5 => 1,
        3 | 6 | 7 => 2,
        8 | 9 | 12 => 4,
        10 | 11 | 13 | 0x18 | 0x19 => 8,
        _ => return None,
    })
}

fn round_up(x: i64, align: i64) -> i64 {
    (x + align - 1) / align * align
}

pub fn dll_path(game_dir: &Path) -> PathBuf {
    game_dir.join("GameAssembly.dll")
}

pub fn meta_path(game_dir: &Path) -> PathBuf {
    game_dir.join("zombieassault_Data").join("il2cpp_data").join("Metadata").join("global-metadata.dat")
}

/// (size, modification time in whole seconds since 1970) of a file: what identifies a game build in the cache.
pub fn stamp(path: &Path) -> Result<(u64, u64), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let md = std::fs::metadata(path).map_err(err)?;
    let mtime = md.modified().map_err(err)?.duration_since(UNIX_EPOCH).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((md.len(), mtime.as_secs()))
}

pub(crate) struct Gen<'a> {
    pub(crate) meta: &'a Meta,
    pub(crate) pe: &'a Pe,
    pub(crate) types: Types<'a>,
    /// Assembly name -> its method pointer table.
    modules: HashMap<String, Module>,
    /// Full name -> first type definition with that name.
    by_full: HashMap<&'a str, usize>,
    /// Size of the header every managed object starts with (class pointer + monitor).
    pub(crate) header: i64,
    pointer: i64,
}

/// (size, alignment); `None` when it cannot be derived.
type Size = Option<(i64, i64)>;

/// An instance field of a type: where the binary's field offset table puts it (from the object pointer).
pub(crate) struct FieldAt {
    pub(crate) name: String,
    pub(crate) ty: usize,
    pub(crate) off: i64,
    pub(crate) kind: Kind,
}

/// A field placed by rule instead of read from the binary (see `Gen::sequential`).
struct Placed {
    name: String,
    off: i64,
    type_name: String,
    /// element type of the field as declared (`T_VAR` for a field of the type parameter)
    ty: u8,
}

/// Layout of one `Nullable<T>`, in place.
struct NullableLayout {
    fields: Vec<Placed>,
    size: i64,
    align: i64,
}

/// What `export_nullables` found, for the `runtime` section.
#[derive(Default)]
struct NullableUse {
    exported: Vec<String>,
    confirmed: usize,
    checked: usize,
    /// fields whose `T` has explicit layout
    skipped: usize,
}

impl<'a> Gen<'a> {
    pub(crate) fn new(meta: &'a Meta, pe: &'a Pe) -> Result<Gen<'a>, String> {
        let mut by_full = HashMap::with_capacity(meta.types.len());
        for (i, t) in meta.types.iter().enumerate() {
            by_full.entry(t.full.as_str()).or_insert(i);
        }
        let types = Types::new(pe, meta)?;
        // Both come from the size table rather than from a constant: an object with no fields is exactly a
        // header, and an IntPtr object is a header plus one pointer.
        let instance = |full: &str| -> Result<i64, String> {
            let def = *by_full.get(full).ok_or_else(|| format!("type {full} not found in the metadata"))?;
            Ok(types.type_sizes(def)?.ok_or_else(|| format!("the size table has no entry for {full}"))?.instance)
        };
        let header = instance("System.Object")?;
        let pointer = instance("System.IntPtr")? - header;
        if header <= 0 || pointer <= 0 {
            return Err(format!("implausible object header ({header}) or pointer size ({pointer}) in the size table"));
        }
        Ok(Gen { meta, pe, types, modules: pe.code_gen_modules()?, by_full, header, pointer })
    }

    pub(crate) fn def(&self, full: &str) -> Result<usize, String> {
        self.by_full.get(full).copied().ok_or_else(|| format!("type {full} not found in the metadata"))
    }

    /// The non-static fields of a type with their recorded offsets; empty when the binary records none.
    pub(crate) fn instance_fields(&self, type_def: usize) -> Result<Vec<FieldAt>, String> {
        let t = &self.meta.types[type_def];
        let Some(offsets) = self.types.field_offsets(type_def, t)? else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for (fi, off) in self.meta.type_fields(t).zip(offsets) {
            let f = self.meta.field(fi)?;
            let kind = self.types.kind(f.ty as usize)?;
            if kind.attrs & ATTR_STATIC == 0 {
                out.push(FieldAt { name: f.name, ty: f.ty as usize, off: off as i64, kind });
            }
        }
        Ok(out)
    }

    // ---- sizes: what a field occupies ----------------------------------------------------------------------

    /// (size, alignment) of a field of this type as it is stored inside an object or struct.
    ///
    /// Value types take their size from the binary's size table and their alignment from their fields; that is
    /// enough to place fields the way the runtime does (see `sequential`).
    fn stored(&self, k: &Kind, depth: u32) -> Result<Size, String> {
        if let Some(s) = primitive_size(k.ty) {
            return Ok(Some((s, s)));
        }
        let reference = Some((self.pointer, self.pointer));
        match k.ty {
            T_PTR | T_CLASS | T_SZARRAY | T_STRING | T_OBJECT | T_ARRAY => Ok(reference),
            T_GENERICINST => {
                let (definition, _) = self.types.generic_inst(k)?;
                if self.types.kind_ptr(definition)?.ty != T_VALUETYPE {
                    return Ok(reference);
                }
                // a generic struct is stored inline and its size depends on its arguments: only Nullable<T>
                // is worked out here
                match self.nullable_arg(k)? {
                    Some(arg) if depth < 8 => Ok(self.nullable(arg, depth + 1)?.map(|n| (n.size, n.align))),
                    _ => Ok(None),
                }
            }
            T_VALUETYPE if depth < 8 => {
                let def = k.data as usize;
                let Some(sizes) = self.meta.types.get(def).map(|_| self.types.type_sizes(def)).transpose()?.flatten() else { return Ok(None) };
                let Some(align) = self.align_of(def, depth + 1)? else { return Ok(None) };
                Ok((sizes.instance > self.header).then_some((sizes.instance - self.header, align)))
            }
            _ => Ok(None),
        }
    }

    /// Alignment of a value type: that of its most demanding field.
    fn align_of(&self, type_def: usize, depth: u32) -> Result<Option<i64>, String> {
        let t = &self.meta.types[type_def];
        let mut align = 1;
        for fi in self.meta.type_fields(t) {
            let k = self.types.kind(self.meta.field_type(fi)? as usize)?;
            if k.attrs & ATTR_STATIC != 0 {
                continue;
            }
            let Some((_, a)) = self.stored(&k, depth)? else { return Ok(None) };
            align = align.max(a);
        }
        Ok(Some(align))
    }

    /// Size of a type from the size table: without the object header for a value type read in place, from the
    /// object pointer otherwise.
    pub(crate) fn table_size(&self, type_def: usize, in_place: bool) -> Result<Option<i64>, String> {
        Ok(self.types.type_sizes(type_def)?.map(|s| if in_place { s.instance - self.header } else { s.instance }))
    }

    // ---- sizes: the way the Python generator computed them -------------------------------------------------
    //
    // Up to version 10 a struct's size was worked out from its fields (end of the last field, rounded up), which
    // fails for structs with fixed buffers. The size table is the source now; this stays as an independent
    // check, and a disagreement is exported (`size_from_fields`) instead of hidden.

    fn fields_size_align(&self, type_index: usize, depth: u32) -> Result<Size, String> {
        let k = self.types.kind(type_index)?;
        if let Some(s) = primitive_size(k.ty) {
            return Ok(Some((s, s)));
        }
        match k.ty {
            T_PTR | T_CLASS | T_SZARRAY | T_STRING | T_OBJECT | T_GENERICINST | T_ARRAY => {
                if k.ty == T_GENERICINST && self.types.name(type_index)?.starts_with("System.Nullable<") {
                    return Ok(None);
                }
                Ok(Some((self.pointer, self.pointer)))
            }
            T_VALUETYPE if depth < 8 => self.size_from_fields(k.data as usize, depth + 1),
            _ => Ok(None),
        }
    }

    fn size_from_fields(&self, type_def: usize, depth: u32) -> Result<Size, String> {
        let t = self.meta.types.get(type_def).ok_or_else(|| format!("type definition {type_def} does not exist"))?;
        let instance = self.instance_fields(type_def)?;
        match instance.as_slice() {
            [] => return Ok(None),
            // an enum is as big as its underlying integer
            [f] if f.name == "value__" => return self.fields_size_align(f.ty, depth),
            _ => {}
        }
        // `fixed T name[N]` becomes a struct with a single element field; its length is not in the field tables
        if t.name.contains("FixedBuffer") {
            return Ok(None);
        }
        let (mut end, mut align) = (0, 1);
        for f in &instance {
            let Some((s, a)) = self.fields_size_align(f.ty, depth)? else { return Ok(None) };
            end = end.max(f.off - self.header + s);
            align = align.max(a);
        }
        Ok(Some((round_up(end, align), align)))
    }

    // ---- layouts the binary does not record ----------------------------------------------------------------

    /// Offsets of the instance fields of a type by the runtime's own rule: in declaration order, each at the
    /// next multiple of its alignment, starting at `start`. Stops at the first field whose size is unknown.
    ///
    /// Needed for generic types (`List<T>`, `Nullable<T>`): the binary records field offsets per type
    /// definition, and a generic definition has none because they depend on the type arguments. `argument` is
    /// what a field of the type parameter `T` holds.
    fn sequential(&self, type_def: usize, start: i64, argument: Option<u64>, depth: u32) -> Result<(Vec<Placed>, i64, i64), String> {
        let t = &self.meta.types[type_def];
        let (mut pos, mut align, mut out) = (start, 1, Vec::new());
        for fi in self.meta.type_fields(t) {
            let f = self.meta.field(fi)?;
            let k = self.types.kind(f.ty as usize)?;
            if k.attrs & ATTR_STATIC != 0 {
                continue;
            }
            let (size, type_name) = match (k.ty, argument) {
                (T_VAR, Some(arg)) => (self.stored(&self.types.kind_ptr(arg)?, depth)?, self.types.name_of_ptr(arg)?),
                _ => (self.stored(&k, depth)?, self.types.name(f.ty as usize)?),
            };
            let Some((s, a)) = size else { break };
            pos = round_up(pos, a);
            out.push(Placed { name: f.name, off: pos, type_name, ty: k.ty });
            pos += s;
            align = align.max(a);
        }
        Ok((out, pos, align))
    }

    /// Does the rule of `sequential` reproduce the offsets the binary records for this class? `None` when the
    /// class cannot serve as a test (not derived directly from object, nothing recorded, a field of unknown
    /// size).
    ///
    /// Known limit: a packed explicit-layout struct held by value is placed without alignment, which this
    /// rule does not model (one class in the build this was written against). The layouts derived with the
    /// rule (`List<T>`, `Nullable<T>` of a sequential `T`) contain no such field.
    fn rule_holds(&self, type_def: usize) -> Result<Option<bool>, String> {
        let t = &self.meta.types[type_def];
        if t.parent == NO_TYPE || self.types.kind(t.parent as usize)?.ty != T_OBJECT {
            return Ok(None);
        }
        let Some(offsets) = self.types.field_offsets(type_def, t)? else { return Ok(None) };
        let (mut pos, mut holds, mut any) = (self.header, true, false);
        for (fi, off) in self.meta.type_fields(t).zip(offsets) {
            let k = self.types.kind(self.meta.field_type(fi)? as usize)?;
            if k.attrs & ATTR_STATIC != 0 {
                continue;
            }
            // classes nested in generic types have a table of zeros
            if (off as i64) < self.header {
                return Ok(None);
            }
            let Some((s, a)) = self.stored(&k, 0)? else { return Ok(None) };
            pos = round_up(pos, a);
            holds &= pos == off as i64;
            pos += s;
            any = true;
        }
        Ok(any.then_some(holds))
    }

    /// (classes whose recorded offsets the rule reproduces, classes tested). Evidence for the layouts computed
    /// with the rule; a class that cannot be read is simply not counted.
    fn rule_evidence(&self) -> (usize, usize) {
        let results = (0..self.meta.types.len()).filter_map(|def| self.rule_holds(def).ok().flatten());
        results.fold((0, 0), |(ok, n), holds| (ok + holds as usize, n + 1))
    }

    /// The `T` of a `Nullable<T>` field type.
    fn nullable_arg(&self, k: &Kind) -> Result<Option<u64>, String> {
        if k.ty != T_GENERICINST {
            return Ok(None);
        }
        let (definition, args) = self.types.generic_inst(k)?;
        let d = self.types.kind_ptr(definition)?;
        let is_nullable = d.ty == T_VALUETYPE && self.meta.types.get(d.data as usize).is_some_and(|t| t.full == NULLABLE);
        Ok(match args.as_slice() {
            [arg] if is_nullable => Some(*arg),
            _ => None,
        })
    }

    /// Layout of `Nullable<T>` for the `T` at `arg`: its two fields (`hasValue`, then the value at T's
    /// alignment) placed by the sequential rule.
    fn nullable(&self, arg: u64, depth: u32) -> Result<Option<NullableLayout>, String> {
        let Some(&def) = self.by_full.get(NULLABLE) else { return Ok(None) };
        let (fields, end, align) = self.sequential(def, 0, Some(arg), depth)?;
        // all of its fields must have been placed, the value included
        let complete = fields.len() == self.meta.type_fields(&self.meta.types[def]).len() && !fields.is_empty();
        Ok(complete.then(|| NullableLayout { fields, size: round_up(end, align), align }))
    }

    // ---- export --------------------------------------------------------------------------------------------

    /// `{size, fields: {name: [offset, type name]}}` of the instance fields of a type; classes also get
    /// `valuetype`. Optional keys, present only when they say something: `fixed_buffers`, `size_from_fields`,
    /// `native_size`.
    fn export_struct(&self, type_def: usize, in_place: bool) -> Result<J, String> {
        let t = &self.meta.types[type_def];
        let (mut fields, mut buffers) = (J::obj(), J::obj());
        if let Some(offsets) = self.types.field_offsets(type_def, t)? {
            for (fi, off) in self.meta.type_fields(t).zip(offsets) {
                let f = self.meta.field(fi)?;
                let k = self.types.kind(f.ty as usize)?;
                if k.attrs & (ATTR_STATIC | ATTR_LITERAL) != 0 {
                    continue;
                }
                let off = if in_place { off as i64 - self.header } else { off as i64 };
                let ty = self.types.name(f.ty as usize)?;
                if let Some(buffer) = self.fixed_buffer(&k)? {
                    buffers.set(f.name.clone(), buffer);
                }
                fields.set(f.name, J::Arr(vec![J::Int(off), J::Str(ty)]));
            }
        }
        let size = self.table_size(type_def, in_place)?;
        let mut out = J::Obj(vec![("size".into(), size.map_or(J::Null, J::Int)), ("fields".into(), fields)]);
        if !in_place {
            out.set("valuetype", J::Bool(self.is_valuetype(type_def)?));
        }
        if buffers != J::obj() {
            out.set("fixed_buffers", buffers);
        }
        if in_place {
            // the two independent sources of a size the game's files have, shown only when they disagree with
            // the exported one
            if let Some((from_fields, _)) = self.size_from_fields(type_def, 0)? {
                if size != Some(from_fields) {
                    out.set("size_from_fields", J::Int(from_fields));
                }
            }
            if let Some(native) = self.types.type_sizes(type_def)?.and_then(|s| s.native) {
                if size != Some(native) {
                    out.set("native_size", J::Int(native));
                }
            }
        }
        Ok(out)
    }

    /// `{size, element}` when a field is a C# fixed buffer (`fixed byte name[24]`), which the compiler turns
    /// into a nested struct holding only the first element; its length exists nowhere but in that struct's size.
    fn fixed_buffer(&self, k: &Kind) -> Result<Option<J>, String> {
        let def = k.data as usize;
        if k.ty != T_VALUETYPE || !self.meta.types.get(def).is_some_and(|t| t.name.contains("FixedBuffer")) {
            return Ok(None);
        }
        let Some(size) = self.table_size(def, true)? else { return Ok(None) };
        let element = match self.instance_fields(def)?.first() {
            Some(f) => J::Str(self.types.name(f.ty)?),
            None => J::Null,
        };
        Ok(Some(J::Obj(vec![("size".into(), J::Int(size)), ("element".into(), element)])))
    }

    /// `{name: offset}` of the static fields of a type, relative to the class's static field block.
    fn statics(&self, full: &str) -> Result<J, String> {
        let type_def = self.def(full)?;
        let t = &self.meta.types[type_def];
        let mut out = J::obj();
        if let Some(offsets) = self.types.field_offsets(type_def, t)? {
            for (fi, off) in self.meta.type_fields(t).zip(offsets) {
                let f = self.meta.field(fi)?;
                let a = self.types.kind(f.ty as usize)?.attrs;
                if a & ATTR_STATIC != 0 && a & ATTR_LITERAL == 0 {
                    out.set(f.name, J::Int(off as i64));
                }
            }
        }
        Ok(out)
    }

    /// Kept as the Python generator defines it (base class is ValueType or Enum). In this metadata version an
    /// enum's "parent" is its underlying integer type, so this is true for structs only.
    fn is_valuetype(&self, type_def: usize) -> Result<bool, String> {
        let parent = self.meta.types[type_def].parent;
        Ok(parent != NO_TYPE && matches!(self.types.name(parent as usize)?.as_str(), "System.ValueType" | "System.Enum"))
    }

    /// Adds a `Nullable<T>` struct for every nullable field of the exported types, and checks each against the
    /// one thing the binary does record about it: where the field after it starts.
    ///
    /// Not for a `T` with explicit layout. Such structs are not placed like others when they are fields (the
    /// binary shows packed ones directly after a bool, unaligned), the size of a `Nullable` of them does not say
    /// where inside it the value sits, and nothing the meter reads needs one; so rather than export a guess,
    /// they are left out and counted in the `runtime` note.
    fn export_nullables(&self, exported: &[usize], structs: &mut J) -> Result<NullableUse, String> {
        let mut found = NullableUse::default();
        for &def in exported {
            let fields = self.instance_fields(def)?;
            for f in &fields {
                let Some(arg) = self.nullable_arg(&f.kind)? else { continue };
                let value = self.types.kind_ptr(arg)?;
                if value.ty == T_VALUETYPE && self.meta.types.get(value.data as usize).is_some_and(|t| t.explicit_layout()) {
                    found.skipped += 1;
                    continue;
                }
                let Some(layout) = self.nullable(arg, 0)? else { continue };
                let name = self.types.name(f.ty)?;
                let key = name.strip_prefix("System.").unwrap_or(&name).replace(NBI, "");
                if !structs.has(&key) {
                    let mut members = J::obj();
                    for m in &layout.fields {
                        members.set(m.name.clone(), J::Arr(vec![J::Int(m.off), J::Str(m.type_name.clone())]));
                    }
                    structs.set(key.clone(), J::Obj(vec![("size".into(), J::Int(layout.size)), ("fields".into(), members)]));
                    found.exported.push(key);
                }
                let Some(next) = fields.iter().filter(|n| n.off > f.off).min_by_key(|n| n.off) else { continue };
                if let Some((_, align)) = self.stored(&next.kind, 0)? {
                    found.checked += 1;
                    found.confirmed += (round_up(f.off + layout.size, align) == next.off) as usize;
                }
            }
        }
        Ok(found)
    }

    /// Every enum of the simulation's namespace plus the few wanted from elsewhere, `{name: {member: value}}`.
    fn export_enums(&self) -> Result<J, String> {
        let mut enums = J::obj();
        for e in ENUMS {
            if self.by_full.contains_key(e) {
                enums.set(e.replace(NBI, ""), self.types.enum_members(e)?);
            }
        }
        for (def, t) in self.meta.types.iter().enumerate() {
            let key = if t.full.strip_prefix(NBI) == Some(t.name.as_str()) {
                t.name.clone()
            } else if t.declaring != NO_TYPE && t.full == t.name {
                // an enum nested in a type of the namespace has no namespace of its own and usually a name
                // that is not unique (Flags, Type): exported as Outer.Inner
                match self.types.name(t.declaring as usize)?.strip_prefix(NBI) {
                    Some(outer) => format!("{outer}.{}", t.name),
                    None => continue,
                }
            } else {
                continue;
            };
            if !enums.has(&key) && self.is_enum(def)? {
                enums.set(key, self.types.constants(&[def])?);
            }
        }
        for e in EXTRA_ENUMS {
            match self.by_full.get(e) {
                Some(&def) if !enums.has(e) && self.is_enum(def)? => enums.set(*e, self.types.constants(&[def])?),
                _ => {}
            }
        }
        Ok(enums)
    }

    /// An enum is a value type whose only instance field is `value__`.
    fn is_enum(&self, type_def: usize) -> Result<bool, String> {
        let t = &self.meta.types[type_def];
        for fi in self.meta.type_fields(t) {
            if self.meta.field(fi)?.name == "value__" {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ---- how the runtime lays out its own objects ----------------------------------------------------------

    /// The `runtime` section: offsets inside managed strings, arrays, lists and nullables, each with a `source`
    /// saying which table of the game's files it was read from (or that it is an ABI fact no file records).
    fn runtime(&self, nullables: &NullableUse) -> Result<J, String> {
        let obj = |items: Vec<(&str, J)>| J::Obj(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        let text = |s: String| J::Str(s);
        let (header, pointer) = (self.header, self.pointer);
        let mut rt = J::obj();

        rt.set("object", obj(vec![
            ("header_size", J::Int(header)),
            ("pointer_size", J::Int(pointer)),
            ("source", text("type size table of GameAssembly.dll: instance size of System.Object (an object without fields is only a header), and of System.IntPtr minus that".into())),
        ]));

        // System.String: its length and its first character are ordinary fields with recorded offsets. Found by
        // type rather than by name, since the names differ between class library versions.
        let string = self.instance_fields(self.def("System.String")?)?;
        let field_of = |ty: u8| string.iter().find(|f| f.kind.ty == ty);
        let (Some(length), Some(chars)) = (field_of(T_INT), field_of(T_CHAR)) else {
            return Err("System.String has no int length / char field with a recorded offset".into());
        };
        rt.set("string", obj(vec![
            ("length", J::Int(length.off)),
            ("chars", J::Int(chars.off)),
            ("char_size", J::Int(primitive_size(T_CHAR).unwrap_or(2))),
            ("source", text(format!("field offset table of GameAssembly.dll: System.String.{} and .{}", length.name, chars.name))),
        ]));

        // Arrays have no fields in the metadata, but the class library carries its own mirror of the array
        // header (System.Array.RawData, used for unsafe casts), and that is an ordinary class with recorded
        // offsets. Without it, the header follows from the two sizes above.
        let raw_data = self.meta.types.iter().enumerate().find(|(_, t)| {
            t.name == "RawData" && t.declaring != NO_TYPE && self.types.name(t.declaring as usize).is_ok_and(|n| n == "System.Array")
        });
        let mirror = match raw_data {
            Some((def, _)) => {
                let fields = self.instance_fields(def)?;
                let at = |name: &str| fields.iter().find(|f| f.name == name).map(|f| f.off);
                at("Bounds").zip(at("Count")).zip(at("Data"))
            }
            None => None,
        };
        let array_size = self.by_full.get("System.Array").map(|&d| self.table_size(d, false)).transpose()?.flatten();
        let (bounds, length, elements, source) = match mirror {
            Some(((bounds, count), data)) => {
                let agrees = if array_size == Some(data) { "; elements also equals the instance size of System.Array in the type size table" } else { "" };
                (bounds, count, data, format!("field offset table of GameAssembly.dll: System.Array.RawData (Bounds, Count, Data), the class library's own mirror of the array header{agrees}"))
            }
            None => (header, header + pointer, header + 2 * pointer, "il2cpp runtime ABI (object header, bounds pointer, length, elements); not recorded in this build's files".to_string()),
        };
        rt.set("array", obj(vec![("bounds", J::Int(bounds)), ("length", J::Int(length)), ("elements", J::Int(elements)), ("source", text(source))]));

        // List<T>: generic, so nothing recorded; its fields hold no T by value, so the layout is the same for
        // every T and follows from the field list.
        let (ok, tested) = self.rule_evidence();
        let rule = format!("IL2CPP's sequential field layout rule (declaration order, natural alignment), which reproduces the recorded offsets of {ok} of the {tested} non-generic classes deriving from object in this build");
        let (list, _, _) = self.sequential(self.def(LIST)?, header, None, 0)?;
        // the backing array is its only array field, the element count its first int
        let first = |ty: u8| list.iter().find(|f| f.ty == ty);
        let (Some(items), Some(size)) = (first(T_SZARRAY), first(T_INT)) else {
            return Err(format!("{LIST} has no array / int field to take as items / size"));
        };
        rt.set("list", obj(vec![
            ("items", J::Int(items.off)),
            ("size", J::Int(size.off)),
            ("source", text(format!("{LIST} is generic, so GameAssembly.dll records no offsets for it; computed for its fields {} and {} from the metadata's field list with {rule}", items.name, size.name))),
        ]));

        // Nullable<T>: generic and holding a T by value, so one layout per T; those the exported types use are
        // in `structs`.
        let has_value = self.by_full.get(NULLABLE).map(|&d| self.sequential(d, 0, None, 0)).transpose()?.and_then(|(f, ..)| f.into_iter().next());
        if let Some(Placed { name, off, .. }) = has_value {
            rt.set("nullable", obj(vec![
                ("has_value", J::Int(off)),
                ("structs", J::Arr(nullables.exported.iter().cloned().map(J::Str).collect())),
                ("source", text(format!("{NULLABLE} is generic, so nothing is recorded; from its field list ({name} first, then the value at the alignment of T) with the same sequential rule. The layouts of the Nullable<T> fields of the exported types are in structs; {} of {} are confirmed by the recorded offset of the field that follows them. Not exported: {} such field(s) whose T has explicit layout, where the files do not settle the position of the value", nullables.confirmed, nullables.checked, nullables.skipped))),
            ]));
        }

        // A native std::vector seen from managed code: the structs declare it as a fixed buffer of bytes, which
        // gives its size but not its members.
        let buffers = [(nbi!("GameEntity"), "weapons_"), (nbi!("GameEntity"), "activated_abilities_"), (nbi!("StatusEffectStack"), "effects_")];
        let mut mirrors = Vec::new();
        for (owner, field) in buffers {
            let Some(&def) = self.by_full.get(owner) else { continue };
            for f in self.instance_fields(def)?.iter().filter(|f| f.name == field) {
                if self.fixed_buffer(&f.kind)?.is_some_and(|b| b == J::Obj(vec![("size".into(), J::Int(3 * pointer)), ("element".into(), J::Str("byte".into()))])) {
                    mirrors.push(format!("{}.{field}", owner.replace(NBI, "")));
                }
            }
        }
        let confirmed = if mirrors.is_empty() { "not confirmed by this build's files".to_string() } else { format!("its size is confirmed by the fixed buffers that mirror such vectors ({})", mirrors.join(", ")) };
        rt.set("native_vector", obj(vec![
            ("begin", J::Int(0)),
            ("end", J::Int(pointer)),
            ("capacity", J::Int(2 * pointer)),
            ("size", J::Int(3 * pointer)),
            ("source", text(format!("MSVC std::vector ABI (three pointers: first element, one past the last, end of storage), not recorded in the game's files; {confirmed}"))),
        ]));

        // What the TypeInfo slot holds before the runtime has resolved it (see `is_typeinfo_slot`).
        rt.set("typeinfo_slot", obj(vec![
            ("unresolved_mask", J::Int(1)),
            ("source", text("il2cpp runtime ABI: until first use the slot holds a metadata token with bit 0 set instead of a class pointer; the same encoding this generator reads in GameAssembly.dll to find the slot".into())),
        ]));
        Ok(rt)
    }

    // ---- facts that only exist in code ---------------------------------------------------------------------

    /// Address of the first method called `method_name` on a type with the short name `type_name`.
    pub(crate) fn method(&self, type_name: &str, method_name: &str) -> Result<u64, String> {
        Ok(self.find_method(type_name, method_name)?.1)
    }

    /// (metadata index, address) of that method.
    pub(crate) fn find_method(&self, type_name: &str, method_name: &str) -> Result<(usize, u64), String> {
        for (ti, t) in self.meta.types.iter().enumerate().filter(|(_, t)| t.name == type_name) {
            for mi in self.meta.type_methods(t) {
                if self.meta.method_name(mi)? == method_name {
                    let va = self.types.method_va(&self.modules, ti, mi).map_err(|e| format!("{type_name}::{method_name}: {e}"))?;
                    return Ok((mi, va));
                }
            }
        }
        Err(format!("method {type_name}::{method_name} not found in the metadata"))
    }

    fn code(&self, va: u64, len: usize) -> Result<&[u8], String> {
        let off = self.pe.off(va)?;
        Ok(&self.pe.d[off..self.pe.d.len().min(off + len)])
    }

    /// Is `va` the global that will hold the `Il2CppClass*` of `type_name` at run time? In the file such a slot
    /// holds a tagged token instead of a pointer: bit 0 set, the usage kind in bits 29-31 (1 = TypeInfo) and an
    /// index into the Il2CppType table in between. The runtime swaps it for the real pointer on first use.
    fn is_typeinfo_slot(&self, va: u64, type_name: &str) -> bool {
        let Some(Ok(v)) = self.pe.va2off(va).map(|o| self.pe.u64_at(o)) else { return false };
        if v & 1 == 0 || v >> 32 != 0 || (v >> 29) & 7 != 1 {
            return false;
        }
        let type_index = ((v & 0x1FFF_FFFF) >> 1) as usize;
        self.types.name(type_index).is_ok_and(|n| n == type_name)
    }

    /// (RVA of the `<type>_TypeInfo` slot, offset of `static_fields` in `Il2CppClass`), both read from a static
    /// getter, which compiles to
    ///
    /// ```text
    /// mov rax, [rip + TypeInfo]      ; the class
    /// mov rax, [rax + static_fields] ; its static field block
    /// mov rax, [rax + field]         ; the field (offset 0 for the getter chosen)
    /// ```
    ///
    /// `Il2CppClass` is a runtime struct whose layout differs between Unity versions, which is why the offset is
    /// read here instead of being a constant.
    fn typeinfo(&self, type_name: &str, method_name: &str) -> Result<(u64, i64), String> {
        let va = self.method(type_name, method_name)?;
        let code = self.code(va, 0x80)?;
        let (mut slot, mut static_fields, mut prev) = (None, None, None);
        let mut pos = 0;
        while let Some(ins) = x86::decode(&code[pos..]) {
            if ins.is_ret() {
                break;
            }
            if let Some(target) = ins.rip_target(va + pos as u64) {
                if self.is_typeinfo_slot(target, type_name) {
                    slot = Some(target);
                }
            }
            // two consecutive loads through rax, the second at offset 0: the first one's offset is static_fields
            match ins.mov_rax_from_rax() {
                Some(disp) => {
                    if prev.is_some() && disp == 0 {
                        static_fields = prev;
                    }
                    prev = Some(disp);
                }
                None => prev = None,
            }
            pos += ins.len;
        }
        match (slot, static_fields) {
            (Some(slot), Some(sf)) if sf != 0 => Ok((slot - self.pe.base, sf)),
            _ => Err(format!("could not locate the TypeInfo slot via {type_name}::{method_name}")),
        }
    }

    /// `GameEventType -> size of that event's record in ClientUpdateResult.event_data`.
    ///
    /// The game has this as a `switch` in `WorldController::GetGameEventExtraDataSize`, compiled to a two-level
    /// jump table:
    ///
    /// ```text
    /// movzx eax, cl ; cmp eax, N ; ja default
    /// lea   rdx, [image base]
    /// movzx eax, byte [rdx + rax + INDEX]   ; event type -> case number
    /// mov   ecx, [rdx + rax*4 + TABLE]      ; case number -> RVA of the case
    /// add   rcx, rdx ; jmp rcx
    /// ```
    ///
    /// and every case is `mov eax, size ; ret` (or `xor eax, eax` for 0). So the sizes are read by following
    /// both tables for each event type and decoding the first instruction of the case.
    fn event_extra_sizes(&self) -> Result<J, String> {
        const SHAPE: &str = "GetGameEventExtraDataSize: unexpected code shape";
        let va = self.method("WorldController", "GetGameEventExtraDataSize")?;
        let code = self.code(va, 0x40)?;
        let (mut n, mut index, mut table) = (None, None, None);
        let mut pos = 0;
        while let Some(ins) = x86::decode(&code[pos..]) {
            if n.is_none() {
                n = ins.cmp_imm();
            }
            index = ins.movzx_indexed().or(index);
            table = ins.mov_load_scaled4().or(table);
            if ins.is_jmp() {
                break;
            }
            pos += ins.len;
        }
        let (Some(n), Some(index), Some(table)) = (n, index, table) else { return Err(SHAPE.into()) };
        if !(0..=0xFFFF).contains(&n) {
            return Err(SHAPE.into());
        }
        let index_off = self.pe.off(self.pe.base.wrapping_add(index as u64))?;
        let table_off = self.pe.off(self.pe.base.wrapping_add(table as u64))?;
        let mut out = J::obj();
        for ty in 0..=n as usize {
            let case = crate::bytes::u8_at(&self.pe.d, index_off + ty)? as usize;
            let target = self.pe.base + crate::bytes::u32_at(&self.pe.d, table_off + 4 * case)? as u64;
            let stub = x86::decode(self.code(target, 8)?);
            let size = match stub {
                Some(ins) if ins.mov_imm().is_some() => ins.mov_imm().unwrap(),
                Some(ins) if ins.is_xor() => 0,
                _ => return Err(format!("GetGameEventExtraDataSize: unexpected stub for type {ty}")),
            };
            out.set(ty.to_string(), J::Int(size));
        }
        Ok(out)
    }

    fn build(&self, build_stamp: J) -> Result<J, String> {
        let (slot_rva, static_fields) = self.typeinfo("GameRunner", "get_Instance")?;
        let statics = self.statics("GameRunner")?;
        let event_extra = self.event_extra_sizes()?;

        let mut structs = J::obj();
        let mut exported = Vec::new();
        for s in STRUCTS {
            if let Some(&def) = self.by_full.get(s) {
                structs.set(s.replace(NBI, ""), self.export_struct(def, true)?);
                exported.push(def);
            }
        }
        // The header of the native "world load data" blob is a nested type simply called Header, like dozens of
        // others; the one wanted is the one that describes the entity tables.
        for (def, t) in self.meta.types.iter().enumerate().filter(|(_, t)| t.name == "Header") {
            let mut names = self.meta.type_fields(t).map(|fi| self.meta.field(fi));
            if names.any(|f| f.is_ok_and(|f| f.name == "entity_data_offset")) {
                structs.set("LoadHeader", self.export_struct(def, true)?);
                exported.push(def);
                break;
            }
        }
        for c in CLASSES {
            if let Some(&def) = self.by_full.get(c) {
                structs.set(c.rsplit('.').next().unwrap_or(c), self.export_struct(def, false)?);
                exported.push(def);
            }
        }
        let nullables = self.export_nullables(&exported, &mut structs)?;
        Ok(J::Obj(vec![
            ("version".into(), J::Int(VERSION)),
            ("build".into(), build_stamp),
            ("gamerunner_typeinfo_rva".into(), J::UInt(slot_rva)),
            ("class_static_fields".into(), J::Int(static_fields)),
            ("gamerunner_statics".into(), statics),
            ("structs".into(), structs),
            ("enums".into(), self.export_enums()?),
            ("event_extra".into(), event_extra),
            ("runtime".into(), self.runtime(&nullables)?),
        ]))
    }
}

/// `{name: {member: value}}` for each requested enum, looked up by short name (`DamageType`) or full name
/// (`NativeBuildInterop.DamageType`). Every type with that short or full name contributes, and every field with
/// a constant counts as a member, so this also works for classes that only hold `const`s.
pub fn enums(game_dir: &Path, names: &[&str]) -> Result<J, String> {
    let meta = Meta::load(&meta_path(game_dir))?;
    let pe = Pe::load(&dll_path(game_dir))?;
    let types = Types::new(&pe, &meta)?;
    // one pass over the type definitions instead of one per requested name
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, t) in meta.types.iter().enumerate() {
        by_name.entry(t.name.as_str()).or_default().push(i);
        if t.full != t.name {
            by_name.entry(t.full.as_str()).or_default().push(i);
        }
    }
    let missing: Vec<&str> = names.iter().copied().filter(|n| !by_name.contains_key(n)).collect();
    if !missing.is_empty() {
        return Err(format!("no type named {} in the game's metadata", missing.join(", ")));
    }
    let mut out = J::obj();
    for name in names {
        out.set(*name, types.constants(&by_name[name])?);
    }
    Ok(out)
}

/// Run `f` with the game's two files loaded and joined.
pub(crate) fn with_game<T>(game_dir: &Path, f: impl FnOnce(&Gen) -> Result<T, String>) -> Result<T, String> {
    let meta = Meta::load(&meta_path(game_dir))?;
    let pe = Pe::load(&dll_path(game_dir))?;
    f(&Gen::new(&meta, &pe)?)
}

/// The whole layout document for the game installed in `game_dir`.
pub fn build(game_dir: &Path) -> Result<J, String> {
    let (dll, meta_file) = (dll_path(game_dir), meta_path(game_dir));
    let (dll_size, dll_mtime) = stamp(&dll)?;
    let (meta_size, meta_mtime) = stamp(&meta_file)?;
    let build_stamp = J::Obj(vec![
        ("dll_size".into(), J::UInt(dll_size)),
        ("dll_mtime".into(), J::UInt(dll_mtime)),
        ("meta_size".into(), J::UInt(meta_size)),
        ("meta_mtime".into(), J::UInt(meta_mtime)),
    ]);
    let meta = Meta::load(&meta_file)?;
    let pe = Pe::load(&dll)?;
    Gen::new(&meta, &pe)?.build(build_stamp)
}
