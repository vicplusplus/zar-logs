//! The half of the IL2CPP type system that lives in GameAssembly.dll: the `Il2CppType` table, the per-type field
//! offset tables and the per-assembly method pointers, joined with the names from `global-metadata.dat`.
//!
//! The metadata file says "type 812 has a field `health` of type #20417"; this module answers what type #20417
//! is (a name, and a kind that determines its size), at which offset the field sits, and what constant an enum
//! member has. All of it hangs off one struct in the DLL, `Il2CppMetadataRegistration`, a list of
//! (count, pointer) pairs that is located by a count we already know: the number of type definitions.

use std::collections::HashMap;

use crate::bytes::{u32_at, u64_at};
use crate::json::J;
use crate::meta::{Meta, TypeDef};
use crate::pe::{Module, Pe};

// Il2CppTypeEnum values (the ECMA-335 element types)
pub const T_STRING: u8 = 0x0E;
pub const T_PTR: u8 = 0x0F;
pub const T_VALUETYPE: u8 = 0x11;
pub const T_CLASS: u8 = 0x12;
pub const T_VAR: u8 = 0x13;
pub const T_ARRAY: u8 = 0x14;
pub const T_GENERICINST: u8 = 0x15;
pub const T_OBJECT: u8 = 0x1C;
pub const T_SZARRAY: u8 = 0x1D;
pub const T_MVAR: u8 = 0x1E;

/// Field attribute bits carried in the low 16 bits of an `Il2CppType`.
pub const ATTR_STATIC: u16 = 0x10;
pub const ATTR_LITERAL: u16 = 0x40;

/// Slots of `Il2CppMetadataRegistration`, as (count, pointer) pairs of 8 bytes each.
const REG_TYPES: usize = 6;
const REG_FIELD_OFFSETS: usize = 10;
const REG_TYPE_SIZES: usize = 12;

/// One row of the type size table (`Il2CppTypeDefinitionSizes`).
#[derive(Clone, Copy)]
pub struct TypeSizes {
    /// Size of an object of the type on the managed heap, object header included. For a value type this is the
    /// size of its boxed form: header + the struct itself, trailing padding included.
    pub instance: i64,
    /// Size of the type when marshalled to native code; `None` for types that cannot be (anything holding
    /// managed references). For the structs shared with the native simulation this is the native `sizeof`.
    pub native: Option<i64>,
}

fn primitive(ty: u8) -> Option<&'static str> {
    Some(match ty {
        1 => "void",
        2 => "bool",
        3 => "char",
        4 => "sbyte",
        5 => "byte",
        6 => "short",
        7 => "ushort",
        8 => "int",
        9 => "uint",
        10 => "long",
        11 => "ulong",
        12 => "float",
        13 => "double",
        14 => "string",
        0x16 => "typedref",
        0x18 => "IntPtr",
        0x19 => "UIntPtr",
        0x1C => "object",
        _ => return None,
    })
}

/// One `Il2CppType`: 8 bytes of payload (a type definition index or a pointer, depending on the kind) and a
/// bit field.
#[derive(Clone, Copy)]
pub struct Kind {
    pub ty: u8,
    pub data: u64,
    pub attrs: u16,
    pub byref: bool,
}

pub struct Types<'a> {
    pe: &'a Pe,
    meta: &'a Meta,
    types_count: u64,
    /// File offset of the array of pointers to every `Il2CppType`.
    types_off: usize,
    /// File offset of the array of per-type-definition pointers to `int32[fieldCount]`.
    field_offsets_off: usize,
    /// File offset of the array of per-type-definition pointers to `Il2CppTypeDefinitionSizes`.
    sizes_off: usize,
}

impl<'a> Types<'a> {
    pub fn new(pe: &'a Pe, meta: &'a Meta) -> Result<Types<'a>, String> {
        // The registration struct has no name or signature. But two of its counts (field offset tables, type
        // definition sizes) both equal the number of type definitions, two slots apart and each followed by a
        // pointer into the image; that combination is unique.
        let n = meta.types.len() as u64;
        let needle = n.to_le_bytes();
        let backed = |va: u64| pe.va2off(va).is_some_and(|o| o != 0);
        let mut reg = None;
        for off in pe.find_all(&needle) {
            if off % 8 != 0 || off < 8 * REG_FIELD_OFFSETS {
                continue;
            }
            let start = off - 8 * REG_FIELD_OFFSETS;
            let slot = |i: usize| u64_at(&pe.d, start + 8 * i);
            let (Ok(sizes_n), Ok(offsets), Ok(sizes)) = (slot(REG_TYPE_SIZES), slot(REG_FIELD_OFFSETS + 1), slot(REG_TYPE_SIZES + 1)) else { continue };
            if sizes_n == n && backed(offsets) && backed(sizes) {
                reg = Some((slot(REG_TYPES)?, slot(REG_TYPES + 1)?, offsets, sizes));
                break;
            }
        }
        let (types_count, types_va, offsets_va, sizes_va) = reg.ok_or("Il2CppMetadataRegistration not found in GameAssembly.dll")?;
        Ok(Types {
            pe,
            meta,
            types_count,
            types_off: pe.off(types_va)?,
            field_offsets_off: pe.off(offsets_va)?,
            sizes_off: pe.off(sizes_va)?,
        })
    }

    pub fn kind(&self, type_index: usize) -> Result<Kind, String> {
        if type_index as u64 >= self.types_count {
            return Err(format!("type index {type_index} is outside the Il2CppType table ({} entries)", self.types_count));
        }
        let va = u64_at(&self.pe.d, self.types_off + 8 * type_index)?;
        self.kind_at(self.pe.off(va)?)
    }

    /// The `Il2CppType` at an address (generic arguments and element types are referenced by pointer, not index).
    pub fn kind_ptr(&self, va: u64) -> Result<Kind, String> {
        self.kind_at(self.pe.off(va)?)
    }

    /// The two halves of a generic instance type such as `List<int>`: the address of the generic type's own
    /// `Il2CppType` (`List`1`) and the addresses of the argument types. From `Il2CppGenericClass { Il2CppType*
    /// type; Il2CppGenericInst* class_inst; ... }` and `Il2CppGenericInst { uint32 argc; Il2CppType** argv }`.
    pub fn generic_inst(&self, k: &Kind) -> Result<(u64, Vec<u64>), String> {
        let g = self.pe.off(k.data)?;
        let inst = u64_at(&self.pe.d, g + 8)?;
        let mut args = Vec::new();
        if let Some(io) = self.pe.va2off(inst) {
            let argc = u64_at(&self.pe.d, io)? & 0xFFFF_FFFF;
            if let Some(ao) = self.pe.va2off(u64_at(&self.pe.d, io + 8)?).filter(|_| argc < 16) {
                for i in 0..argc as usize {
                    args.push(u64_at(&self.pe.d, ao + 8 * i)?);
                }
            }
        }
        Ok((u64_at(&self.pe.d, g)?, args))
    }

    /// Instance size of a type definition from the binary's own size table; `None` where the table has nothing
    /// (generic type definitions: their size depends on the type arguments).
    pub fn type_sizes(&self, type_def_index: usize) -> Result<Option<TypeSizes>, String> {
        let row = u64_at(&self.pe.d, self.sizes_off + 8 * type_def_index)?;
        let Some(off) = self.pe.va2off(row) else { return Ok(None) };
        let instance = u32_at(&self.pe.d, off)? as i64;
        let native = crate::bytes::i32_at(&self.pe.d, off + 4)? as i64;
        Ok((instance != 0).then_some(TypeSizes { instance, native: (native >= 0).then_some(native) }))
    }

    /// Name of the type at an address, in the same form as [`Types::name`].
    pub fn name_of_ptr(&self, va: u64) -> Result<String, String> {
        self.name_ptr(va, 0)
    }

    fn kind_at(&self, off: usize) -> Result<Kind, String> {
        let bits = u32_at(&self.pe.d, off + 8)?;
        Ok(Kind { ty: (bits >> 16) as u8, data: u64_at(&self.pe.d, off)?, attrs: bits as u16, byref: bits >> 29 & 1 != 0 })
    }

    /// C#-style name of a type: `int`, `NativeBuildInterop.GameEntity*`, `System.Collections.Generic.List<Loot>`.
    /// These strings are part of the exported layout, so their exact form matters.
    pub fn name(&self, type_index: usize) -> Result<String, String> {
        if type_index as u64 >= self.types_count {
            return Ok(format!("T{type_index}"));
        }
        self.name_ptr(u64_at(&self.pe.d, self.types_off + 8 * type_index)?, 0)
    }

    fn name_ptr(&self, va: u64, depth: u32) -> Result<String, String> {
        // generic arguments can nest (and, in a damaged table, loop); "?" marks where the walk gave up
        if depth > 6 {
            return Ok("?".into());
        }
        let Some(off) = self.pe.va2off(va) else { return Ok("?".into()) };
        let k = self.kind_at(off)?;
        let mut r = if let Some(p) = primitive(k.ty) {
            p.to_string()
        } else {
            match k.ty {
                T_VALUETYPE | T_CLASS => match self.meta.types.get(k.data as usize) {
                    Some(t) => t.full.clone(),
                    None => format!("td{}", k.data),
                },
                T_SZARRAY => self.name_ptr(k.data, depth + 1)? + "[]",
                T_PTR => self.name_ptr(k.data, depth + 1)? + "*",
                T_GENERICINST => {
                    let (definition, args) = self.generic_inst(&k)?;
                    let base = self.name_ptr(definition, depth + 1)?;
                    let args = args.iter().map(|&a| self.name_ptr(a, depth + 1)).collect::<Result<Vec<_>, _>>()?;
                    // "List`1" -> "List<...>"
                    format!("{}<{}>", base.split('`').next().unwrap_or(""), args.join(", "))
                }
                T_VAR => "T".into(),
                T_MVAR => "TM".into(),
                T_ARRAY => "array[,]".into(),
                other => format!("ty{other:#x}"),
            }
        };
        if k.byref {
            r.push('&');
        }
        Ok(r)
    }

    /// Offset of each field of a type, in field order. `None` when the type has no offset table. Offsets count
    /// from the start of the managed object, so a value type's first field is at 0x10 (after the object header
    /// it would have when boxed).
    pub fn field_offsets(&self, type_def_index: usize, t: &TypeDef) -> Result<Option<Vec<i32>>, String> {
        let n = self.meta.field_count(t);
        let table = u64_at(&self.pe.d, self.field_offsets_off + 8 * type_def_index)?;
        let Some(off) = self.pe.va2off(table).filter(|_| n != 0) else { return Ok(None) };
        (0..n).map(|i| crate::bytes::i32_at(&self.pe.d, off + 4 * i)).collect::<Result<_, _>>().map(Some)
    }

    /// Native address of a method, from its assembly's method pointer table (`modules`: see
    /// `Pe::code_gen_modules`; passed in because finding them costs several searches of the DLL that callers
    /// who only want names and constants should not pay for).
    pub fn method_va(&self, modules: &HashMap<String, Module>, type_def_index: usize, method_index: usize) -> Result<u64, String> {
        let row = (self.meta.method_token(method_index)? & 0x00FF_FFFF) as u64;
        let mut va = None;
        for image in &self.meta.images {
            if !(image.type_start..image.type_start + image.type_count).contains(&type_def_index) {
                continue;
            }
            let Some(module) = modules.get(&image.name).filter(|m| m.method_count != 0) else { continue };
            let Some(table) = self.pe.va2off(module.method_pointers) else { continue };
            va = Some(if 0 < row && row <= module.method_count { u64_at(&self.pe.d, table + 8 * (row as usize - 1))? } else { 0 });
        }
        match va {
            Some(v) if v != 0 => Ok(v),
            // generic and abstract methods have no body of their own
            Some(_) => Err("the method has no native code".into()),
            None => Err("the method's assembly has no method pointer table in GameAssembly.dll".into()),
        }
    }

    /// The constant of a field (an enum member's value, a `const`), or `None` when it has none.
    pub fn default_value(&self, field_index: usize) -> Result<Option<J>, String> {
        let Some((type_index, data)) = self.meta.field_default(field_index) else { return Ok(None) };
        let ty = self.kind(type_index as usize)?.ty;
        let bytes = |n: usize| data.get(..n).ok_or_else(|| "constant data is cut off".to_string());
        Ok(Some(match ty {
            2 | 5 => J::Int(bytes(1)?[0] as i64),
            4 => J::Int(bytes(1)?[0] as i8 as i64),
            3 | 7 => J::Int(u16::from_le_bytes(bytes(2)?.try_into().unwrap()) as i64),
            6 => J::Int(i16::from_le_bytes(bytes(2)?.try_into().unwrap()) as i64),
            // 32-bit integers are stored in a variable-length form; signed ones with the sign in the low bit
            8 => {
                let (v, _) = compressed_u32(data)?;
                J::Int(match v {
                    u32::MAX => i32::MIN as i64,
                    v if v & 1 != 0 => -(((v >> 1) + 1) as i64),
                    v => (v >> 1) as i64,
                })
            }
            9 => J::Int(compressed_u32(data)?.0 as i64),
            10 => J::Int(i64::from_le_bytes(bytes(8)?.try_into().unwrap())),
            11 => J::UInt(u64::from_le_bytes(bytes(8)?.try_into().unwrap())),
            12 => J::Float(f32::from_le_bytes(bytes(4)?.try_into().unwrap()) as f64),
            13 => J::Float(f64::from_le_bytes(bytes(8)?.try_into().unwrap())),
            T_STRING => {
                let (len, used) = compressed_u32(data)?;
                let s = data.get(used..used + len as usize).ok_or("constant string is cut off")?;
                J::Str(String::from_utf8_lossy(s).into_owned())
            }
            other => J::Str(format!("?ty{other:#x}")),
        }))
    }

    /// `{member: value}` of an enum (or of any type that holds constants), by short or full type name.
    pub fn enum_members(&self, name: &str) -> Result<J, String> {
        let defs: Vec<usize> = self.meta.types.iter().enumerate().filter(|(_, t)| t.name == name || t.full == name).map(|(i, _)| i).collect();
        self.constants(&defs)
    }

    /// `{field: constant}` of every field with a constant in the given type definitions, merged in that order.
    /// Several definitions because a name does not identify one type: nested types have no namespace, so
    /// unrelated types share names, and lookups by name deliberately take all of them (a later one wins on a
    /// member both have).
    pub fn constants(&self, type_defs: &[usize]) -> Result<J, String> {
        let mut out = J::obj();
        for &def in type_defs {
            for fi in self.meta.type_fields(&self.meta.types[def]) {
                if let Some(v) = self.default_value(fi)? {
                    out.set(self.meta.field(fi)?.name, v);
                }
            }
        }
        Ok(out)
    }
}

/// IL2CPP's compressed unsigned integer: (value, bytes used). The length is in the leading bits of the first
/// byte; 0xFE and 0xFF are reserved single-byte encodings of the two largest values.
fn compressed_u32(d: &[u8]) -> Result<(u32, usize), String> {
    let at = |i: usize| d.get(i).map(|&b| b as u32).ok_or_else(|| "constant data is cut off".to_string());
    let b = at(0)?;
    Ok(if b < 0x80 {
        (b, 1)
    } else if b & 0xC0 == 0x80 {
        ((b & 0x3F) << 8 | at(1)?, 2)
    } else if b & 0xE0 == 0xC0 {
        ((b & 0x1F) << 24 | at(1)? << 16 | at(2)? << 8 | at(3)?, 4)
    } else if b == 0xF0 {
        (at(1)? | at(2)? << 8 | at(3)? << 16 | at(4)? << 24, 5)
    } else if b == 0xFE {
        (u32::MAX - 1, 1)
    } else {
        (u32::MAX, 1)
    })
}

#[cfg(test)]
mod tests {
    use super::compressed_u32;

    #[test]
    fn compressed_integers() {
        assert_eq!(compressed_u32(&[0x7F]).unwrap(), (0x7F, 1));
        assert_eq!(compressed_u32(&[0x81, 0x02]).unwrap(), (0x102, 2));
        assert_eq!(compressed_u32(&[0xC1, 0x02, 0x03, 0x04]).unwrap(), (0x0102_0304, 4));
        assert_eq!(compressed_u32(&[0xF0, 0x78, 0x56, 0x34, 0x12]).unwrap(), (0x1234_5678, 5));
        assert_eq!(compressed_u32(&[0xFF]).unwrap(), (u32::MAX, 1));
        assert!(compressed_u32(&[0x81]).is_err());
    }
}
