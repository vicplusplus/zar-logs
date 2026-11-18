//! SerializedFile: Unity's object container (the `CAB-<hash>` entry of a bundle).
//!
//! A SerializedFile is a table of objects (a texture, a sprite, one ScriptableObject...) and, when the build kept
//! them, a *type tree* per object type that describes the object's bytes field by field. With type trees the data can
//! be read without knowing the game's classes, which is what [`crate::typetree`] does.
//!
//! Layout of format 22/23 (Unity 2020+ / Unity 6), the only ones read here:
//!
//! ```text
//! header, big endian: 8 unused bytes, u32 format, 4 unused bytes, u8 endianness of the rest (0 = little), 3 reserved,
//!                     u32 metadata size, i64 file size, i64 data offset, i64 unused
//! metadata, in the declared endianness:
//!   unity version\0, i32 platform, u8 "has type trees"
//!   i32 type count, then per type:
//!       i32 class id (114 = MonoBehaviour, 115 = MonoScript, 28 = Texture2D, 213 = Sprite, 142 = AssetBundle ...)
//!       u8 stripped, i16 script index (into the script list below; only MonoBehaviour types have one)
//!       16 bytes script hash (MonoBehaviour only), 16 bytes type hash
//!       format 23: 16 bytes type tree hash, i32 type tree size; the tree follows when the size is not 0
//!       type tree blob (see below), then an i32 array of type dependencies
//!   i32 object count, then per object, aligned to 4:
//!       i64 path id (the object's identity), i64 offset from the data offset, u32 size, i32 index of its type
//!   i32 script count, then per script: i32 file index, (align 4) i64 path id of the MonoScript object
//!   i32 externals count, then per external file: \0, 16 bytes GUID, i32 type, path\0
//!   i32 count of "ref types": like types, but followed by class\0 namespace\0 assembly\0 instead of dependencies.
//!       They describe the classes stored through [SerializeReference] (the "managed references" of a MonoBehaviour).
//!   user information\0
//! object data, from the data offset
//! ```
//!
//! A type tree blob is a flattened tree: `"mhtt"`, i32 format (format 23 only), i32 node count, i32 string buffer
//! size, the nodes (32 bytes each: i16 version, u8 depth, u8 flags, u32 type name, u32 field name, i32 byte size,
//! i32 index, u32 meta flags, u64 hash), then the string buffer. A name is an offset into that buffer, or, with the
//! top bit set, an offset into a table of common names that is compiled into Unity and reproduced here. A node's
//! children are the following nodes that are one level deeper.
//!
//! A reference to another object (a `PPtr`) is a file index plus a path id: file 0 is this file, file n is the
//! (n-1)th external. A MonoBehaviour's class is not in its own data: its `m_Script` points at a MonoScript object,
//! which for this game lives in the separate `monoscripts` bundle.

use std::collections::HashMap;

use crate::bundle::{Bundle, Entry};
use crate::bytes::Cursor;

pub const CLASS_TEXTURE2D: i32 = 28;
pub const CLASS_TEXT_ASSET: i32 = 49;
pub const CLASS_MONO_BEHAVIOUR: i32 = 114;
pub const CLASS_MONO_SCRIPT: i32 = 115;
pub const CLASS_ASSET_BUNDLE: i32 = 142;
pub const CLASS_SPRITE: i32 = 213;
pub const CLASS_SPRITE_ATLAS: i32 = 687078895;

/// Field and type names that every type tree may refer to by offset instead of spelling them out.
const COMMON_STRINGS: &str = "AABB\0AnimationClip\0AnimationCurve\0AnimationState\0Array\0Base\0BitField\0bitset\0bool\0char\0ColorRGBA\0\
     Component\0data\0deque\0double\0dynamic_array\0FastPropertyName\0first\0float\0Font\0GameObject\0\
     Generic Mono\0GradientNEW\0GUID\0GUIStyle\0int\0list\0long long\0map\0Matrix4x4f\0MdFour\0MonoBehaviour\0\
     MonoScript\0m_ByteSize\0m_Curve\0m_EditorClassIdentifier\0m_EditorHideFlags\0m_Enabled\0m_ExtensionPtr\0\
     m_GameObject\0m_Index\0m_IsArray\0m_IsStatic\0m_MetaFlag\0m_Name\0m_ObjectHideFlags\0m_PrefabInternal\0\
     m_PrefabParentObject\0m_Script\0m_StaticEditorFlags\0m_Type\0m_Version\0Object\0pair\0PPtr<Component>\0\
     PPtr<GameObject>\0PPtr<Material>\0PPtr<MonoBehaviour>\0PPtr<MonoScript>\0PPtr<Object>\0PPtr<Prefab>\0\
     PPtr<Sprite>\0PPtr<TextAsset>\0PPtr<Texture>\0PPtr<Texture2D>\0PPtr<Transform>\0Prefab\0Quaternionf\0\
     Rectf\0RectInt\0RectOffset\0second\0set\0short\0size\0SInt16\0SInt32\0SInt64\0SInt8\0staticvector\0string\0\
     TextAsset\0TextMesh\0Texture\0Texture2D\0Transform\0TypelessData\0UInt16\0UInt32\0UInt64\0UInt8\0\
     unsigned int\0unsigned long long\0unsigned short\0vector\0Vector2f\0Vector3f\0Vector4f\0\
     m_ScriptingClassIdentifier\0Gradient\0Type*\0int2_storage\0int3_storage\0BoundsInt\0\
     m_CorrespondingSourceObject\0m_PrefabInstance\0m_PrefabAsset\0FileSize\0Hash128\0RenderingLayerMask\0\
     fixed_array\0EntityId\0LoadableObjectId\0LoadableSceneId\0";

/// How a node's bytes are read. Decided once from the type name, so the walker does not compare strings per value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Bool,
    Str,
    /// A length-prefixed run of raw bytes (pixels, vertex buffers).
    Typeless,
    Pair,
    /// One entry of a managed-reference registry: its `data` child is laid out as a ref type of the file.
    ReferencedObject,
    ReferencedObjectData,
    ManagedReferencesRegistry,
    Array,
    /// Anything else is a structure: its children in order.
    Class,
}

impl Kind {
    fn of(type_name: &str) -> Kind {
        match type_name {
            "SInt8" => Kind::I8,
            "UInt8" | "char" => Kind::U8,
            "short" | "SInt16" => Kind::I16,
            "unsigned short" | "UInt16" => Kind::U16,
            "int" | "SInt32" => Kind::I32,
            "unsigned int" | "UInt32" | "Type*" => Kind::U32,
            "long long" | "SInt64" => Kind::I64,
            "unsigned long long" | "UInt64" | "FileSize" => Kind::U64,
            "float" => Kind::F32,
            "double" => Kind::F64,
            "bool" => Kind::Bool,
            "string" => Kind::Str,
            "TypelessData" => Kind::Typeless,
            "pair" => Kind::Pair,
            "ReferencedObject" => Kind::ReferencedObject,
            "ReferencedObjectData" => Kind::ReferencedObjectData,
            "ManagedReferencesRegistry" => Kind::ManagedReferencesRegistry,
            "Array" => Kind::Array,
            _ => Kind::Class,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TypeNode {
    pub type_name: String,
    pub name: String,
    pub kind: Kind,
    pub level: u8,
    /// Meta flag 0x4000: the reader skips to a multiple of 4 after this value.
    pub aligned: bool,
    /// Index one past the last node of this node's subtree, so children are walked without rescanning levels.
    pub end: usize,
}

/// The nodes of one type in depth-first order; node 0 is the root.
#[derive(Debug, Clone, Default)]
pub struct TypeTree {
    pub nodes: Vec<TypeNode>,
}

impl TypeTree {
    /// Indices of the direct children of node `i`.
    pub fn children(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        children(&self.nodes, i)
    }
}

pub fn children(nodes: &[TypeNode], i: usize) -> impl Iterator<Item = usize> + '_ {
    let end = nodes[i].end;
    let mut next = i + 1;
    std::iter::from_fn(move || {
        (next < end).then(|| {
            let child = next;
            next = nodes[child].end;
            child
        })
    })
}

#[derive(Debug, Clone, Default)]
pub struct SerializedType {
    pub class_id: i32,
    /// Index into [`SerializedFile::scripts`], -1 for anything that is not a MonoBehaviour.
    pub script_index: i16,
    pub tree: Option<TypeTree>,
    /// Only set for ref types.
    pub class_name: String,
    pub namespace: String,
    pub assembly: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ObjectInfo {
    pub path_id: i64,
    /// From the start of the SerializedFile.
    pub offset: u64,
    pub size: u32,
    pub type_index: usize,
    pub class_id: i32,
}

/// Where a MonoBehaviour type's class is defined: an entry of the externals (0 = this file) and a path id there.
#[derive(Debug, Clone, Copy)]
pub struct ScriptRef {
    pub file_index: i32,
    pub path_id: i64,
}

#[derive(Debug, Default)]
pub struct SerializedFile {
    pub format: u32,
    pub unity_version: String,
    pub types: Vec<SerializedType>,
    pub objects: Vec<ObjectInfo>,
    pub scripts: Vec<ScriptRef>,
    /// Paths of the other files that PPtrs of this file can point into, e.g. `archive:/CAB-xx/CAB-xx`.
    pub externals: Vec<String>,
    pub ref_types: Vec<SerializedType>,
    by_path_id: HashMap<i64, usize>,
    /// Position of the file in its bundle's stream, so objects can be read from the bundle on demand.
    base: u64,
}

const HEADER_LEN: usize = 48;

impl SerializedFile {
    /// Reads the metadata of a SerializedFile entry. The object data stays in the bundle until asked for.
    pub fn open(bundle: &Bundle, entry: &Entry) -> Result<SerializedFile, String> {
        let fail = |what: String| format!("{}/{}: {}", bundle.name, entry.name, what);
        if (entry.size as usize) < HEADER_LEN {
            return Err(fail("too small for a SerializedFile".into()));
        }
        let header = bundle.read(entry.offset, HEADER_LEN)?;
        let mut r = Cursor::new(&header, true);
        r.bytes(8).map_err(&fail)?;
        let format = r.u32().map_err(&fail)?;
        if !(22..=23).contains(&format) {
            return Err(fail(format!("SerializedFile format {} is not supported (22 and 23 are)", format)));
        }
        r.bytes(4).map_err(&fail)?;
        let big_endian = r.u8().map_err(&fail)? != 0;
        r.bytes(3).map_err(&fail)?;
        let metadata_size = r.u32().map_err(&fail)? as u64;
        r.i64().map_err(&fail)?; // file size
        let data_offset = r.i64().map_err(&fail)? as u64;
        let metadata_end = HEADER_LEN as u64 + metadata_size;
        if metadata_end > entry.size {
            return Err(fail("metadata runs past the end of the file".into()));
        }
        let head = bundle.read(entry.offset, metadata_end as usize)?;
        let mut file = SerializedFile::parse(&head, format, big_endian, data_offset).map_err(&fail)?;
        file.base = entry.offset;
        for object in &file.objects {
            if object.offset + object.size as u64 > entry.size {
                return Err(fail(format!("object {} lies outside the file", object.path_id)));
            }
        }
        Ok(file)
    }

    fn parse(head: &[u8], format: u32, big_endian: bool, data_offset: u64) -> Result<SerializedFile, String> {
        let mut r = Cursor::new(head, big_endian);
        r.pos = HEADER_LEN;
        let unity_version = r.cstr()?;
        r.i32()?; // target platform
        let has_type_trees = r.u8()? != 0;

        let count = r.i32()?.max(0) as usize;
        let mut types = Vec::with_capacity(count);
        for _ in 0..count {
            types.push(read_type(&mut r, format, has_type_trees, false)?);
        }

        let count = r.i32()?.max(0) as usize;
        let mut objects = Vec::with_capacity(count);
        let mut by_path_id = HashMap::with_capacity(count);
        for _ in 0..count {
            r.align(4);
            let path_id = r.i64()?;
            let offset = (r.i64()? as u64).wrapping_add(data_offset);
            let size = r.u32()?;
            let type_index = r.i32()? as usize;
            let class_id = types.get(type_index).ok_or("object with an unknown type index")?.class_id;
            by_path_id.insert(path_id, objects.len());
            objects.push(ObjectInfo { path_id, offset, size, type_index, class_id });
        }

        let count = r.i32()?.max(0) as usize;
        let mut scripts = Vec::with_capacity(count);
        for _ in 0..count {
            let file_index = r.i32()?;
            r.align(4);
            scripts.push(ScriptRef { file_index, path_id: r.i64()? });
        }

        let count = r.i32()?.max(0) as usize;
        let mut externals = Vec::with_capacity(count);
        for _ in 0..count {
            r.cstr()?;
            r.bytes(16)?;
            r.i32()?;
            externals.push(r.cstr()?);
        }

        let count = r.i32()?.max(0) as usize;
        let mut ref_types = Vec::with_capacity(count);
        for _ in 0..count {
            ref_types.push(read_type(&mut r, format, has_type_trees, true)?);
        }

        Ok(SerializedFile { format, unity_version, types, objects, scripts, externals, ref_types, by_path_id, base: 0 })
    }

    pub fn object(&self, path_id: i64) -> Option<&ObjectInfo> {
        self.by_path_id.get(&path_id).map(|&i| &self.objects[i])
    }

    pub fn tree(&self, object: &ObjectInfo) -> Option<&TypeTree> {
        self.types[object.type_index].tree.as_ref()
    }

    /// The MonoScript a MonoBehaviour object's type was built from. It is the same reference the object carries in
    /// its `m_Script` field, available without reading the object.
    pub fn script(&self, object: &ObjectInfo) -> Option<ScriptRef> {
        let index = self.types[object.type_index].script_index;
        (index >= 0).then(|| self.scripts.get(index as usize).copied()).flatten()
    }

    /// The raw bytes of an object.
    pub fn bytes(&self, bundle: &Bundle, object: &ObjectInfo) -> Result<Vec<u8>, String> {
        bundle.read(self.base + object.offset, object.size as usize)
    }
}

fn read_type(r: &mut Cursor, format: u32, has_type_trees: bool, is_ref: bool) -> Result<SerializedType, String> {
    let class_id = r.i32()?;
    r.u8()?; // stripped
    let script_index = r.i16()?;
    if (is_ref && script_index >= 0) || class_id == CLASS_MONO_BEHAVIOUR {
        r.bytes(16)?;
    }
    r.bytes(16)?;
    let mut out = SerializedType { class_id, script_index, ..Default::default() };
    if has_type_trees {
        let mut has_tree = true;
        if format >= 23 {
            r.bytes(16)?;
            has_tree = r.i32()? != 0;
        }
        if has_tree {
            out.tree = Some(read_tree(r, format)?);
        }
        if is_ref {
            out.class_name = r.cstr()?;
            out.namespace = r.cstr()?;
            out.assembly = r.cstr()?;
        } else {
            let dependencies = r.i32()?.max(0) as usize;
            r.bytes(dependencies * 4)?;
        }
    }
    Ok(out)
}

fn read_tree(r: &mut Cursor, format: u32) -> Result<TypeTree, String> {
    if format >= 23 {
        if r.bytes(4)? != b"mhtt" {
            return Err("type tree without its \"mhtt\" marker".into());
        }
        r.i32()?;
    }
    let count = r.i32()?.max(0) as usize;
    let strings_len = r.i32()?.max(0) as usize;
    let mut raw = Cursor::new(r.bytes(count * 32)?, r.big_endian);
    let strings = r.bytes(strings_len)?;

    let name = |offset: u32| -> String {
        let (table, at) = if offset & 0x8000_0000 != 0 {
            (COMMON_STRINGS.as_bytes(), (offset & 0x7fff_ffff) as usize)
        } else {
            (strings, offset as usize)
        };
        match table.get(at..) {
            Some(rest) => {
                let len = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
                String::from_utf8_lossy(&rest[..len]).into_owned()
            }
            None => at.to_string(),
        }
    };

    let mut nodes: Vec<TypeNode> = Vec::with_capacity(count);
    for _ in 0..count {
        raw.i16()?; // version
        let level = raw.u8()?;
        raw.u8()?; // type flags
        let type_name = name(raw.u32()?);
        let field_name = name(raw.u32()?);
        raw.i32()?; // byte size
        raw.i32()?; // index
        let meta = raw.u32()?;
        raw.u64()?; // ref type hash
        nodes.push(TypeNode {
            kind: Kind::of(&type_name),
            type_name,
            name: field_name,
            level,
            aligned: meta & 0x4000 != 0,
            end: 0,
        });
    }
    // A subtree ends at the next node that is not deeper. Walking backwards, that node is found by hopping over the
    // subtrees of the following siblings, each already resolved.
    for i in (0..count).rev() {
        let mut end = i + 1;
        while end < count && nodes[end].level > nodes[i].level {
            end = nodes[end].end;
        }
        nodes[i].end = end;
    }
    Ok(TypeTree { nodes })
}
