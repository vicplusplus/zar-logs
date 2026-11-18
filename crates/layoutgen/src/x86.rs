//! A minimal x86-64 instruction decoder: instruction lengths, memory operands and immediates.
//!
//! Two facts the layout needs are not in any table, only in compiled code: which global holds the `GameRunner`
//! class pointer (read from a static getter) and how many bytes each game event carries (read from a `switch`
//! compiled to a jump table). Both are found by walking a few dozen bytes of straight-line code and recognising
//! four or five instruction shapes.
//!
//! Walking code needs the length of *every* instruction on the way, not only the interesting ones, so this decodes
//! the whole legacy encoding space (prefixes, REX, the one-byte map and the 0F / 0F38 / 0F3A maps, ModRM + SIB +
//! displacement + immediate) but knows no mnemonics beyond the handful of predicates at the bottom. VEX/EVEX
//! (AVX) encodings are deliberately not handled: decoding stops there, which the callers treat as "pattern not
//! found" rather than guessing a length and reading the following bytes out of step.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Map {
    /// one-byte opcodes
    One,
    /// `0F xx`
    Two,
    /// `0F 38 xx`
    Three38,
    /// `0F 3A xx`
    Three3A,
}

/// A ModRM memory operand.
#[derive(Clone, Copy, Debug)]
pub struct Mem {
    /// `[rip + disp]`
    pub rip: bool,
    /// Register number 0-15 (rax = 0); `None` for RIP-relative and absolute operands.
    pub base: Option<u8>,
    pub has_index: bool,
    /// Register number of the index; `None` without one (also for the empty scaled slot `has_index` reports).
    pub index: Option<u8>,
    pub scale: u8,
    pub disp: i64,
    /// 32-bit addressing (67 prefix): the registers are eax.., not rax..
    pub addr32: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Insn {
    pub len: usize,
    pub map: Map,
    pub op: u8,
    /// The REX prefix byte, 0 without one.
    pub rex: u8,
    /// 66 prefix: 16-bit operands, or the "packed double / integer" variant of an SSE instruction.
    pub opsize16: bool,
    /// F2 or F3 when present: REP/REPNE, or the scalar variant of an SSE instruction.
    pub rep: Option<u8>,
    /// LOCK / REP / REPNE present. Such an instruction is never one of the plain forms matched below.
    prefixed: bool,
    pub modrm: Option<u8>,
    pub mem: Option<Mem>,
    /// The immediate as stored (not sign-extended), `imm_len` bytes of it.
    pub imm: u64,
    pub imm_len: u8,
}

#[derive(Clone, Copy)]
enum Imm {
    No,
    B,
    W,
    /// 32 bits, or 16 with an operand-size prefix
    Z,
    /// like Z, but 64 bits with REX.W (`mov r64, imm64`)
    V,
    Rel32,
    /// absolute address (`mov al, [moffs]`)
    Moffs,
    /// `enter imm16, imm8`
    Enter,
    /// F6 / F7: only `test` (reg field 0 or 1) has an immediate
    TestB,
    TestZ,
}

/// The longest legal instruction; anything longer is not code.
const MAX_LEN: usize = 15;

/// Decode the instruction at the start of `code`. `None` when the bytes run out or the encoding is not covered.
pub fn decode(code: &[u8]) -> Option<Insn> {
    let mut i = 0;
    let (mut opsize16, mut addr32, mut lock, mut rep) = (false, false, false, None);
    let mut rex = 0u8;
    let mut op = loop {
        let b = *code.get(i)?;
        i += 1;
        match b {
            // a REX prefix only counts directly before the opcode, hence the resets
            0x66 => (opsize16, rex) = (true, 0),
            0x67 => (addr32, rex) = (true, 0),
            0xF0 => (lock, rex) = (true, 0),
            0xF2 | 0xF3 => (rep, rex) = (Some(b), 0),
            0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 => rex = 0,
            0x40..=0x4F => rex = b,
            _ => break b,
        }
        if i >= MAX_LEN {
            return None;
        }
    };
    let mut map = Map::One;
    if op == 0x0F {
        op = *code.get(i)?;
        i += 1;
        map = match op {
            0x38 => Map::Three38,
            0x3A => Map::Three3A,
            _ => Map::Two,
        };
        if map != Map::Two {
            op = *code.get(i)?;
            i += 1;
        }
    }
    let (has_modrm, imm_kind) = match map {
        Map::One => one_byte(op)?,
        Map::Two => two_byte(op)?,
        Map::Three38 => (true, Imm::No),
        Map::Three3A => (true, Imm::B),
    };
    let rex_w = rex & 8 != 0;

    let (mut modrm, mut mem) = (None, None);
    if has_modrm {
        let m = *code.get(i)?;
        i += 1;
        modrm = Some(m);
        let (mode, rm) = (m >> 6, m & 7);
        if mode != 3 {
            let rex_b = (rex & 1) << 3;
            let mut base = Some(rm | rex_b);
            let (mut rip, mut has_index, mut index_reg, mut scale) = (false, false, None, 1u8);
            let mut disp_len = match mode {
                1 => 1,
                2 => 4,
                _ => 0,
            };
            if rm == 4 {
                let sib = *code.get(i)?;
                i += 1;
                let (ss, index, sib_base) = (sib >> 6, ((sib >> 3) & 7) | ((rex & 2) << 2), sib & 7);
                scale = 1 << ss;
                // index 4 means "none"; with a scale it is still an (empty) scaled index, which disassemblers
                // show as a pseudo register (`riz*4`)
                has_index = index != 4 || ss != 0;
                index_reg = (index != 4).then_some(index);
                if sib_base == 5 && mode == 0 {
                    base = None;
                    disp_len = 4;
                } else {
                    base = Some(sib_base | rex_b);
                }
            } else if rm == 5 && mode == 0 {
                base = None;
                rip = !addr32;
                disp_len = 4;
            }
            let d = code.get(i..i + disp_len)?;
            let disp = match disp_len {
                1 => d[0] as i8 as i64,
                4 => i32::from_le_bytes(d.try_into().unwrap()) as i64,
                _ => 0,
            };
            i += disp_len;
            mem = Some(Mem { rip, base, has_index, index: index_reg, scale, disp, addr32 });
        }
    }

    let z = if opsize16 && !rex_w { 2 } else { 4 };
    let is_test = modrm.is_some_and(|m| (m >> 3) & 7 < 2);
    let imm_len = match imm_kind {
        Imm::No => 0,
        Imm::B => 1,
        Imm::W => 2,
        Imm::Z => z,
        Imm::V if rex_w => 8,
        Imm::V => z,
        Imm::Rel32 => 4,
        Imm::Moffs if addr32 => 4,
        Imm::Moffs => 8,
        Imm::Enter => 3,
        Imm::TestB if is_test => 1,
        Imm::TestZ if is_test => z,
        Imm::TestB | Imm::TestZ => 0,
    };
    let mut raw = [0u8; 8];
    raw[..imm_len].copy_from_slice(code.get(i..i + imm_len)?);
    i += imm_len;
    if i > MAX_LEN {
        return None;
    }
    let prefixed = lock || rep.is_some();
    Some(Insn { len: i, map, op, rex, opsize16, rep, prefixed, modrm, mem, imm: u64::from_le_bytes(raw), imm_len: imm_len as u8 })
}

/// (has ModRM, immediate) per one-byte opcode, 64-bit mode. `None`: not valid there, or VEX/EVEX.
fn one_byte(op: u8) -> Option<(bool, Imm)> {
    use Imm::*;
    Some(match op {
        // add/or/adc/sbb/and/sub/xor/cmp: six forms each; the two remaining slots are invalid in 64-bit mode
        0x00..=0x3F => match op & 7 {
            0..=3 => (true, No),
            4 => (false, B),
            5 => (false, Z),
            _ => return None,
        },
        0x50..=0x5F => (false, No), // push/pop reg
        0x63 => (true, No),         // movsxd
        0x68 => (false, Z),
        0x69 => (true, Z),
        0x6A => (false, B),
        0x6B => (true, B),
        0x6C..=0x6F => (false, No),
        0x70..=0x7F => (false, B), // jcc rel8
        0x80 | 0x83 => (true, B),
        0x81 => (true, Z),
        0x84..=0x8F => (true, No), // test/xchg/mov/lea/pop
        0x90..=0x99 | 0x9B..=0x9F => (false, No),
        0xA0..=0xA3 => (false, Moffs),
        0xA4..=0xA7 | 0xAA..=0xAF => (false, No), // string ops
        0xA8 => (false, B),
        0xA9 => (false, Z),
        0xB0..=0xB7 => (false, B), // mov r8, imm8
        0xB8..=0xBF => (false, V), // mov r, imm
        0xC0 | 0xC1 | 0xC6 => (true, B),
        0xC2 | 0xCA => (false, W),
        0xC3 | 0xC9 | 0xCB | 0xCC | 0xCF => (false, No),
        0xC7 => (true, Z),
        0xC8 => (false, Enter),
        0xCD => (false, B),
        0xD0..=0xD3 | 0xD8..=0xDF => (true, No), // shifts, x87
        0xD7 => (false, No),
        0xE0..=0xE7 | 0xEB => (false, B), // loop/jrcxz/in/out/jmp rel8
        0xE8 | 0xE9 => (false, Rel32),
        0xEC..=0xEF | 0xF1 | 0xF4 | 0xF5 | 0xF8..=0xFD => (false, No),
        0xF6 => (true, TestB),
        0xF7 => (true, TestZ),
        0xFE | 0xFF => (true, No),
        _ => return None,
    })
}

/// The same for `0F xx`.
fn two_byte(op: u8) -> Option<(bool, Imm)> {
    use Imm::*;
    Some(match op {
        0x00..=0x03 | 0x0D => (true, No),
        0x05..=0x09 | 0x0B | 0x0E => (false, No),
        0x10..=0x23 | 0x28..=0x2F => (true, No), // SSE moves, hint nops, mov cr/dr, SSE convert/compare
        0x30..=0x35 | 0x37 => (false, No),
        0x40..=0x6F => (true, No), // cmovcc, SSE/MMX
        0x70..=0x73 => (true, B),
        0x74..=0x76 | 0x78 | 0x79 | 0x7C..=0x7F => (true, No),
        0x77 => (false, No),
        0x80..=0x8F => (false, Rel32), // jcc rel32
        0x90..=0x9F => (true, No),     // setcc
        0xA0..=0xA2 | 0xA8..=0xAA => (false, No),
        0xA3 | 0xA5 | 0xAB | 0xAD..=0xAF => (true, No),
        0xA4 | 0xAC => (true, B), // shld/shrd imm8
        0xB0..=0xB9 | 0xBB..=0xBF => (true, No),
        0xBA => (true, B),
        0xC0 | 0xC1 | 0xC3 | 0xC7 => (true, No),
        0xC2 | 0xC4..=0xC6 => (true, B),
        0xC8..=0xCF => (false, No), // bswap
        0xD0..=0xFF => (true, No),
        _ => return None,
    })
}

impl Insn {
    fn plain(&self, map: Map) -> bool {
        self.map == map && !self.prefixed
    }

    /// ModRM `reg` field, the `/n` of opcode extensions.
    pub fn ext(&self) -> Option<u8> {
        self.modrm.map(|m| (m >> 3) & 7)
    }

    /// The immediate, sign-extended from its stored width.
    pub fn imm_signed(&self) -> i64 {
        match self.imm_len {
            1 => self.imm as u8 as i8 as i64,
            2 => self.imm as u16 as i16 as i64,
            4 => self.imm as u32 as i32 as i64,
            _ => self.imm as i64,
        }
    }

    /// Address a RIP-relative operand refers to; `addr` is the address of this instruction.
    pub fn rip_target(&self, addr: u64) -> Option<u64> {
        let m = self.mem.filter(|m| m.rip)?;
        Some(addr.wrapping_add(self.len as u64).wrapping_add(m.disp as u64))
    }

    /// `ret` / `ret imm16`
    pub fn is_ret(&self) -> bool {
        self.plain(Map::One) && matches!(self.op, 0xC3 | 0xC2)
    }

    /// `jmp rel` / `jmp reg` / `jmp [mem]`
    pub fn is_jmp(&self) -> bool {
        self.plain(Map::One) && (matches!(self.op, 0xE9 | 0xEB) || (self.op == 0xFF && self.ext() == Some(4)))
    }

    /// `mov rax, [rax + disp]`: the displacement.
    pub fn mov_rax_from_rax(&self) -> Option<i64> {
        let m = self.mem?;
        let rax_dest = self.rex & 8 != 0 && self.rex & 4 == 0 && self.ext() == Some(0);
        (self.plain(Map::One) && self.op == 0x8B && rax_dest && m.base == Some(0) && !m.addr32).then_some(m.disp)
    }

    /// `cmp x, imm`: the immediate, as the unsigned value of the operand's width when it fills the operand and
    /// sign-extended when it is narrower (the convention of the disassembler the Python generator uses).
    pub fn cmp_imm(&self) -> Option<i64> {
        let cmp = matches!(self.op, 0x3C | 0x3D) || (matches!(self.op, 0x80 | 0x81 | 0x83) && self.ext() == Some(7));
        let narrower = self.op == 0x83 || (matches!(self.op, 0x3D | 0x81) && self.rex & 8 != 0);
        (self.plain(Map::One) && cmp).then(|| if narrower { self.imm_signed() } else { self.imm as i64 })
    }

    /// `movzx reg, [.. + index ..]`: the displacement.
    pub fn movzx_indexed(&self) -> Option<i64> {
        let m = self.mem?;
        (self.plain(Map::Two) && matches!(self.op, 0xB6 | 0xB7) && m.has_index).then_some(m.disp)
    }

    /// `mov reg, [.. + index*4 ..]`: the displacement.
    pub fn mov_load_scaled4(&self) -> Option<i64> {
        let m = self.mem?;
        (self.plain(Map::One) && matches!(self.op, 0x8A | 0x8B | 0x8E) && m.scale == 4).then_some(m.disp)
    }

    /// `mov x, imm`: the immediate. Not the 64-bit `mov r64, imm64`, which is a different instruction to a
    /// disassembler (`movabs`) and never what a "return small constant" stub uses.
    pub fn mov_imm(&self) -> Option<i64> {
        if !self.plain(Map::One) {
            return None;
        }
        let rex_w = self.rex & 8 != 0;
        match self.op {
            0xB0..=0xB7 => Some(self.imm as i64),
            0xB8..=0xBF if !rex_w => Some(self.imm as i64),
            0xC6 if self.ext() == Some(0) => Some(self.imm as i64),
            0xC7 if self.ext() == Some(0) => Some(if rex_w { self.imm_signed() } else { self.imm as i64 }),
            _ => None,
        }
    }

    /// Any integer `xor` (in practice `xor eax, eax`).
    pub fn is_xor(&self) -> bool {
        self.plain(Map::One) && (matches!(self.op, 0x30..=0x35) || (matches!(self.op, 0x80 | 0x81 | 0x83) && self.ext() == Some(6)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lengths(mut code: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        while let Some(i) = decode(code) {
            out.push(i.len);
            code = &code[i.len..];
        }
        assert!(code.is_empty(), "stopped with {} bytes left", code.len());
        out
    }

    /// The shape of an IL2CPP static property getter (made-up displacements).
    const GETTER: &[u8] = &[
        0x48, 0x83, 0xEC, 0x28, // sub rsp, 28h
        0x80, 0x3D, 0x10, 0x00, 0x20, 0x00, 0x00, // cmp byte [rip+200010h], 0
        0x75, 0x18, // jne
        0x48, 0x8D, 0x0D, 0x00, 0x01, 0x00, 0x00, // lea rcx, [rip+100h]
        0xE8, 0x11, 0x22, 0x33, 0x00, // call
        0xF0, 0x83, 0x0C, 0x24, 0x00, // lock or dword [rsp], 0
        0xC6, 0x05, 0x10, 0x00, 0x20, 0x00, 0x01, // mov byte [rip+200010h], 1
        0x48, 0x8B, 0x05, 0xE8, 0x00, 0x00, 0x00, // mov rax, [rip+0E8h]
        0x83, 0xB8, 0xE4, 0x00, 0x00, 0x00, 0x00, // cmp dword [rax+0E4h], 0
        0x75, 0x0F, // jne
        0x48, 0x8B, 0xC8, // mov rcx, rax
        0x48, 0x8B, 0x80, 0xA0, 0x00, 0x00, 0x00, // mov rax, [rax+0A0h]
        0x48, 0x8B, 0x00, // mov rax, [rax]
        0x48, 0x83, 0xC4, 0x28, // add rsp, 28h
        0xC3, // ret
    ];

    #[test]
    fn getter_lengths_and_operands() {
        assert_eq!(lengths(GETTER), [4, 7, 2, 7, 5, 5, 7, 7, 7, 2, 3, 7, 3, 4, 1]);
        let lea = decode(&GETTER[13..]).unwrap();
        assert_eq!(lea.rip_target(0x1000), Some(0x1000 + 7 + 0x100));
        let load = decode(&GETTER[37..]).unwrap();
        assert_eq!(load.rip_target(0x2000), Some(0x2000 + 7 + 0xE8));
        assert_eq!(load.mov_rax_from_rax(), None); // base is rip, not rax
        assert_eq!(decode(&GETTER[53..]).unwrap().mov_rax_from_rax(), None); // mov rcx, rax
        assert_eq!(decode(&GETTER[56..]).unwrap().mov_rax_from_rax(), Some(0xA0));
        assert_eq!(decode(&GETTER[63..]).unwrap().mov_rax_from_rax(), Some(0));
        assert!(decode(&GETTER[70..]).unwrap().is_ret());
        assert!(decode(&GETTER[25..]).unwrap().cmp_imm().is_none()); // lock or
    }

    #[test]
    fn jump_table_switch() {
        let code: &[u8] = &[
            0x0F, 0xB6, 0xC1, // movzx eax, cl
            0x83, 0xF8, 0x28, // cmp eax, 28h
            0x77, 0x63, // ja
            0x48, 0x8D, 0x15, 0x31, 0xF0, 0xFF, 0xFF, // lea rdx, [rip-0FCFh]
            0x0F, 0xB6, 0x84, 0x02, 0x64, 0x10, 0x00, 0x00, // movzx eax, byte [rdx+rax+1064h]
            0x8B, 0x8C, 0x82, 0x30, 0x10, 0x00, 0x00, // mov ecx, [rdx+rax*4+1030h]
            0x48, 0x03, 0xCA, // add rcx, rdx
            0xFF, 0xE1, // jmp rcx
        ];
        assert_eq!(lengths(code), [3, 3, 2, 7, 8, 7, 3, 2]);
        assert_eq!(decode(code).unwrap().movzx_indexed(), None); // register form
        assert_eq!(decode(&code[3..]).unwrap().cmp_imm(), Some(0x28));
        assert_eq!(decode(&code[15..]).unwrap().movzx_indexed(), Some(0x1064));
        assert_eq!(decode(&code[15..]).unwrap().mov_load_scaled4(), None);
        assert_eq!(decode(&code[23..]).unwrap().mov_load_scaled4(), Some(0x1030));
        assert!(decode(&code[33..]).unwrap().is_jmp());
    }

    #[test]
    fn return_stubs() {
        assert_eq!(decode(&[0xB8, 0xA0, 0x06, 0x00, 0x00, 0xC3]).unwrap().mov_imm(), Some(0x6A0));
        assert_eq!(decode(&[0xB8, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap().mov_imm(), Some(0xFFFF_FFFF));
        assert!(decode(&[0x33, 0xC0, 0xC3]).unwrap().is_xor());
        let movabs = decode(&[0x48, 0xB8, 1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!((movabs.len, movabs.mov_imm()), (10, None));
    }

    #[test]
    fn stops_instead_of_guessing() {
        assert!(decode(&[0xC5, 0xF8, 0x77]).is_none()); // VEX (vzeroupper)
        assert!(decode(&[0x48, 0x8B, 0x80, 0xA0]).is_none()); // cut off inside the displacement
        assert!(decode(&[]).is_none());
    }
}
