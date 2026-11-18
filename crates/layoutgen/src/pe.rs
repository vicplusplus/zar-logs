//! Reader for `GameAssembly.dll`: the PE section table (to turn virtual addresses into file offsets) and the
//! search for IL2CPP's per-assembly method pointer tables.
//!
//! The DLL is read as a plain file, never loaded or executed. Everything IL2CPP registers at startup (type table,
//! field offsets, method pointers) sits in its data sections as ordinary pointers, written for the preferred image
//! base, so "follow the pointer" is `va2off` plus a read. None of those tables is exported by name; they are found
//! by their shape, starting from data we can recognise (an assembly name, a known count).

use std::collections::HashMap;
use std::path::Path;

use memchr::memmem;

use crate::bytes::{cstr, u16_at, u32_at, u64_at};

struct Section {
    va: u64,
    vsize: u64,
    raw_off: u64,
    raw_size: u64,
    writable: bool,
}

pub struct Pe {
    pub d: Vec<u8>,
    /// Preferred image base; every pointer stored in the file is relative to it.
    pub base: u64,
    sections: Vec<Section>,
}

/// One assembly's `Il2CppCodeGenModule`: how many methods it has and where their native addresses are listed.
#[derive(Clone, Copy)]
pub struct Module {
    pub method_count: u64,
    pub method_pointers: u64,
}

impl Pe {
    /// A one-section, read-only image holding `code` at `base + 0x1000`, for tests of what reads code.
    #[cfg(test)]
    pub fn test_image(base: u64, code: Vec<u8>) -> Pe {
        let size = code.len() as u64;
        Pe { d: code, base, sections: vec![Section { va: 0x1000, vsize: size, raw_off: 0, raw_size: size, writable: false }] }
    }

    pub fn load(path: &Path) -> Result<Pe, String> {
        let d = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Pe::parse(d).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse(d: Vec<u8>) -> Result<Pe, String> {
        let pe = u32_at(&d, 0x3C)? as usize;
        if !d.starts_with(b"MZ") || u32_at(&d, pe)? != 0x0000_4550 {
            return Err("not a PE image".into());
        }
        let nsec = u16_at(&d, pe + 6)? as usize;
        let opt_size = u16_at(&d, pe + 20)? as usize;
        if u16_at(&d, pe + 24)? != 0x20B {
            return Err("not a 64-bit (PE32+) image".into());
        }
        let base = u64_at(&d, pe + 24 + 24)?;
        let table = pe + 24 + opt_size;
        let mut sections = Vec::with_capacity(nsec);
        for i in 0..nsec {
            let s = table + 40 * i + 8;
            sections.push(Section {
                vsize: u32_at(&d, s)? as u64,
                va: u32_at(&d, s + 4)? as u64,
                raw_size: u32_at(&d, s + 8)? as u64,
                raw_off: u32_at(&d, s + 12)? as u64,
                writable: u32_at(&d, s + 28)? & 0x8000_0000 != 0,
            });
        }
        Ok(Pe { d, base, sections })
    }

    /// File offset of a relative virtual address; `None` outside every section, and also for the part of a section
    /// that only exists in memory (zero-initialised data has no bytes in the file).
    pub fn rva2off(&self, rva: u64) -> Option<usize> {
        for s in &self.sections {
            if s.va <= rva && rva < s.va + s.vsize.max(s.raw_size) {
                if rva - s.va >= s.raw_size {
                    return None;
                }
                return Some((s.raw_off + rva - s.va) as usize);
            }
        }
        None
    }

    /// Is `va` in a section the running game cannot write to (code, constants)? Only there do the bytes in the
    /// file say what the running game sees; everything else is initial state that the runtime overwrites.
    pub fn read_only(&self, va: u64) -> bool {
        let Some(rva) = va.checked_sub(self.base) else { return false };
        self.sections.iter().find(|s| s.va <= rva && rva < s.va + s.vsize.max(s.raw_size)).is_some_and(|s| !s.writable)
    }

    pub fn va2off(&self, va: u64) -> Option<usize> {
        self.rva2off(va.checked_sub(self.base)?)
    }

    pub fn off2va(&self, off: usize) -> Option<u64> {
        let off = off as u64;
        self.sections.iter().find(|s| s.raw_off <= off && off < s.raw_off + s.raw_size).map(|s| self.base + s.va + off - s.raw_off)
    }

    /// File offset of an address that must be backed by file bytes.
    pub fn off(&self, va: u64) -> Result<usize, String> {
        self.va2off(va).ok_or_else(|| format!("address {va:#x} is not inside the file image"))
    }

    pub fn u64_at(&self, off: usize) -> Result<u64, String> {
        u64_at(&self.d, off)
    }

    /// Every file offset where `needle` occurs, overlapping matches included.
    pub fn find_all<'a>(&'a self, needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
        let finder = memmem::Finder::new(needle);
        let mut from = 0;
        std::iter::from_fn(move || {
            let at = from + finder.find(self.d.get(from..)?)?;
            from = at + 1;
            Some(at)
        })
    }

    /// All `Il2CppCodeGenModule`s by assembly name ("Assembly-CSharp.dll" -> its method pointer table).
    ///
    /// Found from the outside in, because nothing points at the module list by name: the string "mscorlib.dll"
    /// is the name of one module -> a struct `{name*, methodCount, methodPointers*, ...}` that points at that
    /// string is that module -> an array that points at that struct is the list of all modules, which is then
    /// walked in both directions for as long as its entries look like modules.
    pub fn code_gen_modules(&self) -> Result<HashMap<String, Module>, String> {
        let mut res = HashMap::new();
        for name_off in self.find_all(b"mscorlib.dll\0") {
            let Some(name_va) = self.off2va(name_off) else { continue };
            let name_ptr = name_va.to_le_bytes();
            for module_off in self.find_all(&name_ptr) {
                let (Ok(count), Ok(pointers)) = (self.u64_at(module_off + 8), self.u64_at(module_off + 16)) else { continue };
                if !(0 < count && count < 200_000 && self.backed(pointers)) {
                    continue; // some other pointer to the same string
                }
                let Some(module_va) = self.off2va(module_off) else { continue };
                let module_ptr = module_va.to_le_bytes();
                for hit in self.find_all(&module_ptr) {
                    let mut a = hit;
                    while a >= 8 && self.is_module(self.u64_at(a - 8)?) {
                        a -= 8;
                    }
                    loop {
                        let p = self.u64_at(a)?;
                        if !self.is_module(p) {
                            break;
                        }
                        let o = self.off(p)?;
                        let name = cstr(&self.d, self.off(self.u64_at(o)?)?)?;
                        res.insert(name, Module { method_count: self.u64_at(o + 8)?, method_pointers: self.u64_at(o + 16)? });
                        a += 8;
                    }
                    // a lone pointer to the mscorlib module (not the list) yields one entry; the real list has
                    // one per assembly
                    if res.len() > 5 {
                        return Ok(res);
                    }
                }
            }
        }
        Ok(res)
    }

    fn backed(&self, va: u64) -> bool {
        self.va2off(va).is_some_and(|o| o != 0)
    }

    /// Does `p` point at a struct whose first member points at an assembly name?
    fn is_module(&self, p: u64) -> bool {
        let Some(o) = self.va2off(p) else { return false };
        let Ok(name) = self.u64_at(o) else { return false };
        let Some(no) = self.va2off(name) else { return false };
        let window = &self.d[no..self.d.len().min(no + 80)];
        let s = window.split(|&b| b == 0).next().unwrap_or(&[]);
        s.ends_with(b".dll") || s.ends_with(b".exe") || s == b"__Generated"
    }
}
