//! Parser for `global-metadata.dat` (IL2CPP metadata version 107, Unity 6000.5).
//!
//! This file holds the *names* of everything in the game's managed code: type definitions, their fields and
//! methods, the assemblies ("images") they belong to, and the constant values of enum members. It holds no
//! addresses and no field offsets; those live in GameAssembly.dll (see `pe` and `il2cpp`), and the two files are
//! joined by index: type N here is type N there.
//!
//! The header is a table of 32 sections, each `(file offset, byte size, record count)`. Record sizes and field
//! positions below were inferred by inspection for this metadata version (there is no public spec for v107), which
//! is why the version is checked up front: on another version the same offsets would read garbage silently.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;

use crate::bytes::{cstr, u16_at, u32_at};

const MAGIC: u32 = 0xFAB1_1BAF;
const VERSION: u32 = 107;

// section numbers in the header table
const S_STRINGS: usize = 2;
const S_METHODS: usize = 5;
const S_FIELD_DEFAULTS: usize = 7;
const S_PARAMS: usize = 10;
const S_DEFAULT_DATA: usize = 8;
const S_FIELDS: usize = 11;
const S_TYPEDEFS: usize = 19;
const S_IMAGES: usize = 21;

// record sizes
const TYPEDEF: usize = 66;
const METHOD: usize = 30;
const FIELD: usize = 10;
const FIELD_DEFAULT: usize = 10;
const PARAM: usize = 10;
const IMAGE: usize = 36;

/// "No parent" / "no declaring type" in the 16-bit type index fields.
pub const NO_TYPE: u16 = 0xFFFF;

pub struct TypeDef {
    pub name: String,
    /// `Namespace.Name`, or just the name without a namespace. Nested types have no namespace of their own, so
    /// several unrelated nested types can share one full name (there are many called `Header`).
    pub full: String,
    /// Index into the binary's Il2CppType table (not into the type definitions).
    pub parent: u16,
    /// The type this one is nested in (same kind of index), or `NO_TYPE`.
    pub declaring: u16,
    /// ECMA-335 TypeAttributes.
    flags: u32,
    field_start: u32,
    field_count: u16,
    method_start: u32,
    method_count: u16,
}

impl TypeDef {
    /// `[StructLayout(LayoutKind.Explicit)]`: every field's offset is declared, as for the structs generated
    /// from the native simulation's headers.
    pub fn explicit_layout(&self) -> bool {
        self.flags & 0x18 == 0x10
    }
}

pub struct Field {
    pub name: String,
    /// Index into the binary's Il2CppType table.
    pub ty: u16,
}

pub struct Image {
    pub name: String,
    pub type_start: usize,
    pub type_count: usize,
}

pub struct Meta {
    d: Vec<u8>,
    secs: [(usize, usize); 32], // (offset, record count)
    pub types: Vec<TypeDef>,
    pub images: Vec<Image>,
    /// field index -> (type index of the constant, offset into the default-value data)
    defaults: HashMap<u32, (u16, u32)>,
}

impl Meta {
    pub fn load(path: &Path) -> Result<Meta, String> {
        let d = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Meta::parse(d).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse(d: Vec<u8>) -> Result<Meta, String> {
        if u32_at(&d, 0)? != MAGIC {
            return Err("not an IL2CPP metadata file".into());
        }
        let version = u32_at(&d, 4)?;
        if version != VERSION {
            return Err(format!("metadata version {version}; this parser only knows the layout of version {VERSION}"));
        }
        let mut secs = [(0, 0); 32];
        for (i, s) in secs.iter_mut().enumerate() {
            *s = (u32_at(&d, 8 + 12 * i)? as usize, u32_at(&d, 8 + 12 * i + 8)? as usize);
        }
        let mut m = Meta { d, secs, types: Vec::new(), images: Vec::new(), defaults: HashMap::new() };

        let (o, n) = m.secs[S_TYPEDEFS];
        m.types.reserve(n);
        for i in 0..n {
            let r = o + TYPEDEF * i;
            let name = m.string(u32_at(&m.d, r)?)?;
            let ns = m.string(u32_at(&m.d, r + 4)?)?;
            let full = if ns.is_empty() { name.clone() } else { format!("{ns}.{name}") };
            m.types.push(TypeDef {
                name,
                full,
                declaring: u16_at(&m.d, r + 10)?,
                flags: u32_at(&m.d, r + 16)?,
                parent: u16_at(&m.d, r + 12)?,
                field_start: u32_at(&m.d, r + 20)?,
                method_start: u32_at(&m.d, r + 24)?,
                method_count: u16_at(&m.d, r + 42)?,
                field_count: u16_at(&m.d, r + 46)?,
            });
        }

        let (o, n) = m.secs[S_IMAGES];
        for i in 0..n {
            let r = o + IMAGE * i;
            let image = Image {
                name: m.string(u32_at(&m.d, r)?)?,
                type_start: u16_at(&m.d, r + 8)? as usize,
                type_count: u16_at(&m.d, r + 10)? as usize,
            };
            m.images.push(image);
        }

        let (o, n) = m.secs[S_FIELD_DEFAULTS];
        m.defaults.reserve(n);
        for i in 0..n {
            let r = o + FIELD_DEFAULT * i;
            m.defaults.insert(u32_at(&m.d, r)?, (u16_at(&m.d, r + 4)?, u32_at(&m.d, r + 6)?));
        }
        Ok(m)
    }

    fn string(&self, index: u32) -> Result<String, String> {
        cstr(&self.d, self.secs[S_STRINGS].0 + index as usize)
    }

    /// Global field indices of a type, clamped to the table (a type without fields carries a junk start index).
    pub fn type_fields(&self, t: &TypeDef) -> Range<usize> {
        clamp(t.field_start, t.field_count, self.secs[S_FIELDS].1)
    }

    pub fn field_count(&self, t: &TypeDef) -> usize {
        t.field_count as usize
    }

    pub fn field(&self, index: usize) -> Result<Field, String> {
        let r = self.secs[S_FIELDS].0 + FIELD * index;
        Ok(Field { name: self.string(u32_at(&self.d, r)?)?, ty: u16_at(&self.d, r + 4)? })
    }

    /// Type index of a field, without the cost of decoding its name.
    pub fn field_type(&self, index: usize) -> Result<u16, String> {
        u16_at(&self.d, self.secs[S_FIELDS].0 + FIELD * index + 4)
    }

    /// Global method indices of a type.
    pub fn type_methods(&self, t: &TypeDef) -> Range<usize> {
        clamp(t.method_start, t.method_count, self.secs[S_METHODS].1)
    }

    pub fn method_name(&self, index: usize) -> Result<String, String> {
        self.string(u32_at(&self.d, self.secs[S_METHODS].0 + METHOD * index)?)
    }

    /// The method's metadata token; its low 24 bits are the 1-based row in its assembly's method pointer table.
    pub fn method_token(&self, index: usize) -> Result<u32, String> {
        u32_at(&self.d, self.secs[S_METHODS].0 + METHOD * index + 18)
    }

    /// ECMA-335 MethodAttributes of a method; bit 0x10 is `static`.
    pub fn method_is_static(&self, index: usize) -> Result<bool, String> {
        Ok(u16_at(&self.d, self.secs[S_METHODS].0 + METHOD * index + 22)? & 0x10 != 0)
    }

    /// Type index of what a method returns.
    pub fn method_return(&self, index: usize) -> Result<u16, String> {
        u16_at(&self.d, self.secs[S_METHODS].0 + METHOD * index + 6)
    }

    /// (name, type index) of a method's parameters, in order.
    pub fn method_params(&self, index: usize) -> Result<Vec<(String, u16)>, String> {
        let r = self.secs[S_METHODS].0 + METHOD * index;
        let (start, count) = (u32_at(&self.d, r + 12)? as usize, u16_at(&self.d, r + 28)? as usize);
        (start..start + count)
            .map(|i| {
                let p = self.secs[S_PARAMS].0 + PARAM * i;
                Ok((self.string(u32_at(&self.d, p)?)?, u16_at(&self.d, p + 8)?))
            })
            .collect()
    }

    /// Where the constant of a field is stored: (type index of the constant, bytes from that position on).
    /// `None` for a field without a constant.
    pub fn field_default(&self, field_index: usize) -> Option<(u16, &[u8])> {
        let &(ty, data) = self.defaults.get(&(field_index as u32))?;
        if data == u32::MAX {
            return None;
        }
        // a position outside the file yields no bytes, which the decoder reports as cut-off data
        Some((ty, self.d.get(self.secs[S_DEFAULT_DATA].0 + data as usize..).unwrap_or(&[])))
    }
}

fn clamp(start: u32, count: u16, table_len: usize) -> Range<usize> {
    if count == 0 {
        return 0..0;
    }
    let start = (start as usize).min(table_len);
    start..(start + count as usize).min(table_len)
}
