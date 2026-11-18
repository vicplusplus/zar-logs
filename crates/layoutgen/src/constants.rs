//! Numbers that exist only in the game's machine code: the factors that turn a gear mod's "points" into a bonus.
//!
//! The gear maths needs four factors (percent of accuracy / reload time / crit chance / magazine size per mod
//! point) that are in no data file and no metadata table. They are operands in one managed method,
//! `PassiveBonusDefinition.TranslateModPointToRealBonus`, which gets the point count from
//! `BonusEvaluationContext.GetFinalModPointsValue`, switches on the bonus type and, per type, either looks the
//! value up in a crafting table (`GearModCrafting.SumPercentPerPointByQualityTier`) or multiplies the point count
//! by a constant. A game update can change those constants, so they are read from the binary per game build.
//!
//! Nothing here goes by address or by position in the method. The method and the two it calls are found by
//! name, the bonus type values come from the enum's member names, and the branch of each bonus type is found by
//! *running* the method's code abstractly with only that one input known: every jump that depends on the bonus
//! type is decided, every jump that depends on anything else (is there a crafting table? is the class
//! initialised?) is followed both ways. On the way the point count is tracked from the call that returns it,
//! through its conversion to float, to the instruction that scales it. That instruction's operand is the
//! constant.
//!
//! The interpreter knows the instruction forms compilers emit for such code and what each does to the registers
//! it tracks; anything else on a path it walks is an error, never skipped, because a skipped instruction could
//! have changed a tracked value. The same strictness applies to the result: exactly one scaling of the point
//! count must be reachable for a bonus type, with the sign and the table path the description of the constant
//! says. A caller that gets an error keeps the previous build's value and marks it unverified.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::bytes::u32_at;
use crate::il2cpp::T_VALUETYPE;
use crate::json::J;
use crate::layout::Gen;
use crate::pe::Pe;
use crate::x86::{self, Insn, Map, Mem};

const TYPE: &str = "PassiveBonusDefinition";
const METHOD: &str = "TranslateModPointToRealBonus";
const BONUS_ENUM: &str = "BonusType";
const POINTS_SOURCE: (&str, &str) = ("BonusEvaluationContext", "GetFinalModPointsValue");
const TABLE_LOOKUP: (&str, &str) = ("GearModCrafting", "SumPercentPerPointByQualityTier");

/// One constant: its key, the bonus type whose branch holds it, and the shape that branch must have.
struct Spec {
    name: &'static str,
    member: &'static str,
    /// The bonus type also has a path through the crafting table (the constant is then the factor of the path
    /// without one, which is the one enhancements take).
    table_path: bool,
    /// The game negates the scaled value (more points = less reload time).
    negated: bool,
}

const SPECS: &[Spec] = &[
    Spec { name: "accuracy_percent_per_mod_point", member: "WeaponAccuracy", table_path: false, negated: false },
    Spec { name: "enhancement_reload_percent_per_mod_point", member: "ReloadTime", table_path: true, negated: true },
    Spec { name: "enhancement_crit_percent_per_mod_point", member: "CritChance", table_path: true, negated: false },
    Spec { name: "enhancement_magazine_percent_per_mod_point", member: "WeaponMagazineCapacity", table_path: true, negated: false },
];

// ---- abstract values -----------------------------------------------------------------------------------------

/// What is known about a general register.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Val {
    /// `known` marks the bits of `v` that are certain (the bonus type arrives as one byte of a register).
    Bits { v: u64, known: u64 },
    /// The mod point count, as an integer.
    Points,
}

const UNKNOWN: Val = Val::Bits { v: 0, known: 0 };

/// What is known about the low float of an xmm register.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Xmm {
    Other,
    /// The point count, still an integer / converted to float.
    PointsInt,
    PointsFloat,
    /// The point count after scaling event `event`, and whether its sign was flipped since.
    Scaled { event: u8, negated: bool },
    /// A float loaded from read-only data.
    Const { bits: u32, rva: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Flags {
    zf: bool,
    sf: bool,
    cf: bool,
    of: bool,
    pf: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct State {
    gpr: [Val; 16],
    xmm: [Xmm; 16],
    /// `None`: set by something that was not computed from known values.
    flags: Option<Flags>,
}

fn mask(size: u8) -> u64 {
    if size >= 8 { !0 } else { (1u64 << (8 * size)) - 1 }
}

fn sign_extend(v: u64, size: u8) -> u64 {
    let shift = 64 - 8 * size.min(8) as u32;
    (((v << shift) as i64) >> shift) as u64
}

/// Without a REX prefix, byte register numbers 4-7 are ah/ch/dh/bh, which are not tracked.
fn high_byte(reg: u8, size: u8, rex: u8) -> bool {
    size == 1 && rex == 0 && (4..8).contains(&reg)
}

impl State {
    /// The register read at `size` bytes, if all of those bits are known.
    fn get(&self, reg: u8, size: u8, rex: u8) -> Option<u64> {
        match self.gpr[reg as usize] {
            Val::Bits { v, known } if !high_byte(reg, size, rex) && known & mask(size) == mask(size) => Some(v & mask(size)),
            _ => None,
        }
    }

    fn set(&mut self, reg: u8, size: u8, rex: u8, value: Option<u64>) {
        let known = value.map_or(UNKNOWN, |v| Val::Bits { v: v & mask(size), known: mask(size) });
        self.put(reg, size, rex, known);
    }

    /// Write the low `size` bytes of `value` to a register, with the processor's rules for the rest of it:
    /// a 32-bit write clears the upper half, 8- and 16-bit writes leave the other bits alone.
    fn put(&mut self, reg: u8, size: u8, rex: u8, value: Val) {
        if high_byte(reg, size, rex) {
            self.gpr[reg as usize - 4] = UNKNOWN;
            return;
        }
        let m = mask(size);
        self.gpr[reg as usize] = match value {
            Val::Points if size >= 4 => Val::Points,
            Val::Points => UNKNOWN,
            Val::Bits { v, known } if size >= 4 => Val::Bits { v: v & m, known: (known & m) | !m },
            Val::Bits { v, known } => match self.gpr[reg as usize] {
                Val::Bits { v: old, known: old_known } => Val::Bits { v: (old & !m) | (v & m), known: (old_known & !m) | (known & m) },
                Val::Points => Val::Bits { v: v & m, known: known & m },
            },
        };
    }

    /// What a read of `size` bytes of a register yields, as a value that can be written elsewhere.
    fn read(&self, reg: u8, size: u8, rex: u8) -> Val {
        if high_byte(reg, size, rex) {
            return UNKNOWN;
        }
        match self.gpr[reg as usize] {
            Val::Points if size >= 4 => Val::Points,
            Val::Points => UNKNOWN,
            Val::Bits { v, known } => Val::Bits { v: v & mask(size), known: known & mask(size) },
        }
    }

    fn address(&self, m: &Mem, next: u64) -> Option<u64> {
        if m.addr32 {
            return None;
        }
        let mut a = match (m.rip, m.base) {
            (true, _) => next,
            (false, Some(b)) => self.get(b, 8, 1)?,
            (false, None) => 0,
        };
        if let Some(i) = m.index {
            a = a.wrapping_add(self.get(i, 8, 1)?.wrapping_mul(m.scale as u64));
        }
        Some(a.wrapping_add(m.disp as u64))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Alu {
    Add,
    Or,
    And,
    Sub,
    Xor,
}

fn alu(op: Alu, a: u64, b: u64, size: u8) -> (u64, Flags) {
    let (m, sign) = (mask(size), 1u64 << (8 * size as u32 - 1));
    let (a, b) = (a & m, b & m);
    let (r, cf, of) = match op {
        Alu::Add => {
            let r = a.wrapping_add(b) & m;
            (r, r < a, (a ^ r) & (b ^ r) & sign != 0)
        }
        Alu::Sub => {
            let r = a.wrapping_sub(b) & m;
            (r, a < b, (a ^ b) & (a ^ r) & sign != 0)
        }
        Alu::Or => (a | b, false, false),
        Alu::And => (a & b, false, false),
        Alu::Xor => (a ^ b, false, false),
    };
    (r, Flags { zf: r == 0, sf: r & sign != 0, cf, of, pf: (r as u8).count_ones().is_multiple_of(2) })
}

/// Is the condition of a `jcc` (low four opcode bits) true?
fn condition(cc: u8, f: Flags) -> bool {
    let base = match cc >> 1 {
        0 => f.of,
        1 => f.cf,
        2 => f.zf,
        3 => f.cf || f.zf,
        4 => f.sf,
        5 => f.pf,
        6 => f.sf != f.of,
        _ => f.zf || f.sf != f.of,
    };
    base != (cc & 1 != 0)
}

// ---- what a run observes ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum How {
    /// `mulss` with a float from read-only data
    Mul { bits: u32, rva: u32 },
    /// `addss x, x`
    Double,
}

#[derive(Debug)]
struct Scale {
    at: u64,
    how: How,
    reg: u8,
}

#[derive(Default, Debug)]
struct Seen {
    scales: Vec<Scale>,
    /// (scaling event, address of the sign flip)
    flips: BTreeSet<(u8, u64)>,
    /// (scaling event, negated?) wherever a scaled value is read afterwards
    uses: BTreeSet<(u8, bool)>,
    /// arithmetic on the point count that is none of the expected forms: address -> instruction bytes
    unexpected: BTreeMap<u64, String>,
    points_calls: BTreeSet<u64>,
    table_calls: BTreeSet<u64>,
}

enum Next {
    Fall,
    Jump(u64),
    /// conditional jump: `Some(taken)` when the flags are known
    Branch(Option<bool>, u64),
    End,
}

struct Run<'a> {
    pe: &'a Pe,
    points_fn: u64,
    table_fn: u64,
    seen: Seen,
}

/// More than any real function needs; reached only if the walk does not converge.
const MAX_STEPS: usize = 400_000;
/// A jump further than this from the method's start has left the method (tail call).
const MAX_SPAN: u64 = 0x10000;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Run<'_> {
    /// Walk every path from `entry` that the initial state allows.
    fn explore(&mut self, entry: u64, initial: State) -> Result<(), String> {
        let mut work = vec![(entry, initial)];
        let mut visited = HashSet::new();
        let mut steps = 0;
        while let Some((mut pc, mut st)) = work.pop() {
            // a state already seen at this address continues the same way
            while (entry..entry + MAX_SPAN).contains(&pc) && visited.insert((pc, st.clone())) {
                steps += 1;
                if steps > MAX_STEPS {
                    return Err(format!("the walk through the method did not finish within {MAX_STEPS} steps"));
                }
                let pe = self.pe;
                let off = pe.off(pc)?;
                let code = &pe.d[off..pe.d.len().min(off + 15)];
                let ins = x86::decode(code).ok_or_else(|| format!("undecodable instruction at {pc:#x} ({})", hex(code)))?;
                let unsupported = || format!("instruction outside what the walk understands at {pc:#x}: {}", hex(&code[..ins.len]));
                let next = pc + ins.len as u64;
                match self.step(&mut st, &ins, pc, &code[..ins.len]).ok_or_else(unsupported)? {
                    Next::Fall => pc = next,
                    Next::Jump(t) => pc = t,
                    Next::Branch(Some(true), t) => pc = t,
                    Next::Branch(Some(false), _) => pc = next,
                    Next::Branch(None, t) => {
                        work.push((t, st.clone()));
                        pc = next;
                    }
                    Next::End => break,
                }
            }
        }
        Ok(())
    }

    fn scale(&mut self, at: u64, how: How, reg: u8) -> Xmm {
        let event = match self.seen.scales.iter().position(|s| s.at == at) {
            Some(i) => i,
            None => {
                self.seen.scales.push(Scale { at, how, reg });
                self.seen.scales.len() - 1
            }
        };
        Xmm::Scaled { event: event as u8, negated: false }
    }

    fn used(&mut self, tag: Xmm) {
        if let Xmm::Scaled { event, negated } = tag {
            self.seen.uses.insert((event, negated));
        }
    }

    /// Arithmetic that touches the point count in a way the expected code does not.
    fn guard(&mut self, tags: &[Xmm], pc: u64, bytes: &[u8]) {
        if tags.iter().any(|t| matches!(t, Xmm::PointsInt | Xmm::PointsFloat | Xmm::Scaled { .. })) {
            self.seen.unexpected.insert(pc, hex(bytes));
        }
    }

    fn call(&mut self, st: &mut State, target: Option<u64>, pc: u64) {
        // Windows x64: these do not survive a call
        for r in [0, 1, 2, 8, 9, 10, 11] {
            st.gpr[r] = UNKNOWN;
        }
        st.xmm[..6].fill(Xmm::Other);
        st.flags = None;
        if target == Some(self.points_fn) {
            st.gpr[0] = Val::Points;
            self.seen.points_calls.insert(pc);
        }
        if target == Some(self.table_fn) {
            self.seen.table_calls.insert(pc);
        }
    }

    /// The effect of one instruction on the tracked state; `None` for an instruction that is not modelled.
    fn step(&mut self, st: &mut State, ins: &Insn, pc: u64, bytes: &[u8]) -> Option<Next> {
        let next = pc + ins.len as u64;
        let rex = ins.rex;
        let (rex_w, rex_r, rex_b) = (rex & 8 != 0, (rex & 4) << 1, (rex & 1) << 3);
        let wide = if rex_w { 8 } else if ins.opsize16 { 2 } else { 4 };
        let size = |byte_op: bool| if byte_op { 1 } else { wide };
        let reg = ins.modrm.map(|m| ((m >> 3) & 7) | rex_r);
        // the register a ModRM names directly (mod = 3)
        let rm = ins.modrm.filter(|m| m >> 6 == 3).map(|m| (m & 7) | rex_b);
        let ext = ins.ext();
        let target = next.wrapping_add(ins.imm_signed() as u64);

        if ins.map == Map::Two {
            return match ins.op {
                0x80..=0x8F if ins.rep.is_none() => Some(Next::Branch(st.flags.map(|f| condition(ins.op & 15, f)), target)),
                0x90..=0x9F => {
                    if let Some(r) = rm {
                        st.set(r, 1, rex, None);
                    }
                    Some(Next::Fall)
                }
                0x40..=0x4F | 0xAF | 0xBC | 0xBD => {
                    // cmovcc, imul, bit scans: the destination is not followed
                    st.set(reg?, wide, rex, None);
                    if ins.op >= 0xAF {
                        st.flags = None;
                    }
                    Some(Next::Fall)
                }
                0xB6 | 0xB7 | 0xBE | 0xBF => {
                    let from = if ins.op & 1 == 0 { 1 } else { 2 };
                    let value = match rm {
                        Some(r) if ins.op < 0xBE => match st.read(r, from, rex) {
                            // zero extension: the upper bits are known zeros whatever the source was
                            Val::Bits { v, known } => Val::Bits { v, known: known | !mask(from) },
                            Val::Points => UNKNOWN,
                        },
                        Some(r) => st.get(r, from, rex).map_or(UNKNOWN, |v| Val::Bits { v: sign_extend(v, from), known: !0 }),
                        None => UNKNOWN,
                    };
                    st.put(reg?, wide, rex, value);
                    Some(Next::Fall)
                }
                0x18..=0x1F => Some(Next::Fall), // prefetch, multi-byte nop
                0x0B => Some(Next::End),         // ud2
                0xA3 | 0xAB | 0xB3 | 0xBB | 0xBA => {
                    // bit tests; all but bt (A3, BA /4) also write the bit back
                    if let Some(r) = rm.filter(|_| !(ins.op == 0xA3 || (ins.op == 0xBA && ext == Some(4)))) {
                        st.set(r, wide, rex, None);
                    }
                    st.flags = None;
                    Some(Next::Fall)
                }
                0xB0 | 0xB1 | 0xC0 | 0xC1 => {
                    // cmpxchg, xadd (the runtime's write barriers use lock cmpxchg)
                    let s = size(ins.op & 1 == 0);
                    st.set(0, s, rex, None);
                    st.set(reg?, s, rex, None);
                    if let Some(r) = rm {
                        st.set(r, s, rex, None);
                    }
                    st.flags = None;
                    Some(Next::Fall)
                }
                0xC8..=0xCF => {
                    st.set((ins.op & 7) | rex_b, wide, rex, None);
                    Some(Next::Fall)
                }
                _ => self.sse(st, ins, pc, bytes),
            };
        }
        if ins.map != Map::One || ins.rep.is_some() {
            return None;
        }
        match ins.op {
            // add/or/adc/sbb/and/sub/xor/cmp in their six encodings, and the same with an immediate
            0x00..=0x3F | 0x80 | 0x81 | 0x83 => {
                let (kind, form) = if ins.op < 0x40 { (ins.op >> 3, ins.op & 7) } else { (ext?, 8) };
                let s = size(matches!(form, 0 | 2 | 4) || ins.op == 0x80);
                // (destination register, source value); a memory operand on either side is not followed
                let imm = Some(ins.imm_signed() as u64 & mask(s));
                let (dst, src, same) = match form {
                    0 | 1 => (rm, st.get(reg?, s, rex), rm == reg),
                    2 | 3 => (reg, rm.and_then(|r| st.get(r, s, rex)), rm == reg),
                    4 | 5 => (Some(0), imm, false),
                    _ => (rm, imm, false),
                };
                let a = dst.and_then(|d| st.get(d, s, rex));
                let op = match kind {
                    0 => Some(Alu::Add),
                    1 => Some(Alu::Or),
                    4 => Some(Alu::And),
                    5 | 7 => Some(Alu::Sub),
                    6 => Some(Alu::Xor),
                    _ => None, // adc, sbb: depend on the carry
                };
                let result = match (op, a, src) {
                    // `xor r, r` and `sub r, r` are zero whatever r was
                    (Some(Alu::Xor | Alu::Sub), _, _) if same && dst.is_some() => Some(alu(Alu::Xor, 0, 0, s)),
                    (Some(op), Some(a), Some(b)) => Some(alu(op, a, b, s)),
                    _ => None,
                };
                st.flags = result.map(|r| r.1);
                if let Some(d) = dst.filter(|_| kind != 7) {
                    st.set(d, s, rex, result.map(|r| r.0));
                }
                Some(Next::Fall)
            }
            0x84 | 0x85 | 0xA8 | 0xA9 => {
                let s = size(ins.op & 1 == 0);
                let (a, b) = match ins.op {
                    0x84 | 0x85 => (rm.and_then(|r| st.get(r, s, rex)), st.get(reg?, s, rex)),
                    _ => (st.get(0, s, rex), Some(ins.imm_signed() as u64)),
                };
                st.flags = a.zip(b).map(|(a, b)| alu(Alu::And, a, b, s).1);
                Some(Next::Fall)
            }
            0xF6 | 0xF7 => {
                let s = size(ins.op == 0xF6);
                let a = rm.and_then(|r| st.get(r, s, rex));
                match ext? {
                    0 | 1 => st.flags = a.map(|a| alu(Alu::And, a, ins.imm_signed() as u64, s).1),
                    2 => {
                        if let Some(r) = rm {
                            st.set(r, s, rex, a.map(|a| !a));
                        }
                    }
                    3 => {
                        let r = a.map(|a| alu(Alu::Sub, 0, a, s));
                        st.flags = r.map(|r| r.1);
                        if let Some(d) = rm {
                            st.set(d, s, rex, r.map(|r| r.0));
                        }
                    }
                    _ => {
                        // mul, imul, div, idiv
                        st.set(0, 8, rex, None);
                        st.set(2, 8, rex, None);
                        st.flags = None;
                    }
                }
                Some(Next::Fall)
            }
            0x88..=0x8B => {
                let s = size(ins.op & 1 == 0);
                let to_reg = ins.op & 2 != 0;
                match (rm, to_reg) {
                    (Some(r), true) => st.put(reg?, s, rex, st.read(r, s, rex)),
                    (Some(r), false) => st.put(r, s, rex, st.read(reg?, s, rex)),
                    (None, true) => {
                        // a load: only read-only memory has the value the file shows (jump tables)
                        let value = st.address(ins.mem.as_ref()?, next).and_then(|a| self.load(a, s));
                        st.set(reg?, s, rex, value);
                    }
                    (None, false) => {} // a store: memory is not tracked
                }
                Some(Next::Fall)
            }
            0x8D => {
                let address = st.address(ins.mem.as_ref()?, next);
                st.set(reg?, wide, rex, address);
                Some(Next::Fall)
            }
            0x63 => {
                let value = rm.and_then(|r| st.get(r, 4, rex)).map(|v| sign_extend(v, 4));
                st.set(reg?, wide, rex, value);
                Some(Next::Fall)
            }
            0xB0..=0xBF => {
                let s = if ins.op < 0xB8 { 1 } else { wide };
                st.set((ins.op & 7) | rex_b, s, rex, Some(ins.imm));
                Some(Next::Fall)
            }
            0xC6 | 0xC7 if ext == Some(0) => {
                let s = size(ins.op == 0xC6);
                if let Some(r) = rm {
                    st.set(r, s, rex, Some(ins.imm_signed() as u64));
                }
                Some(Next::Fall)
            }
            0x98 => {
                let from = if rex_w { 4 } else { wide / 2 };
                let value = st.get(0, from, rex).map(|v| sign_extend(v, from));
                st.set(0, wide, rex, value);
                Some(Next::Fall)
            }
            0x99 => {
                st.set(2, wide, rex, None);
                Some(Next::Fall)
            }
            0x50..=0x57 | 0x68 | 0x6A | 0x90 => Some(Next::Fall), // push, nop
            0x58..=0x5F => {
                st.set((ins.op & 7) | rex_b, 8, rex, None);
                Some(Next::Fall)
            }
            0x86 | 0x87 | 0x91..=0x97 => {
                // xchg
                let s = size(ins.op == 0x86);
                let (a, b) = if ins.op >= 0x91 { (Some(0), Some((ins.op & 7) | rex_b)) } else { (reg, rm) };
                for r in [a, b].into_iter().flatten() {
                    st.set(r, s, rex, None);
                }
                Some(Next::Fall)
            }
            0xC0 | 0xC1 | 0xD0..=0xD3 => {
                // shifts and rotates
                if let Some(r) = rm {
                    st.set(r, size(ins.op & 1 == 0), rex, None);
                }
                st.flags = None;
                Some(Next::Fall)
            }
            0x69 | 0x6B => {
                st.set(reg?, wide, rex, None);
                st.flags = None;
                Some(Next::Fall)
            }
            0xFE | 0xFF => match ext? {
                0 | 1 => {
                    // inc, dec: the carry flag is not theirs, so the flags are only known if they were
                    let s = size(ins.op == 0xFE);
                    let op = if ext == Some(0) { Alu::Add } else { Alu::Sub };
                    let r = rm.and_then(|r| st.get(r, s, rex)).map(|a| alu(op, a, 1, s));
                    st.flags = r.zip(st.flags).map(|(r, old)| Flags { cf: old.cf, ..r.1 });
                    if let Some(d) = rm {
                        st.set(d, s, rex, r.map(|r| r.0));
                    }
                    Some(Next::Fall)
                }
                2 if ins.op == 0xFF => {
                    self.call(st, None, pc);
                    Some(Next::Fall)
                }
                // a jump through a register: the jump table of a switch, if the walk knows where it leads
                4 if ins.op == 0xFF => rm.and_then(|r| st.get(r, 8, rex)).map(Next::Jump),
                6 if ins.op == 0xFF => Some(Next::Fall),
                _ => None,
            },
            0xE8 => {
                self.call(st, Some(target), pc);
                Some(Next::Fall)
            }
            0xE9 | 0xEB => Some(Next::Jump(target)),
            0x70..=0x7F => Some(Next::Branch(st.flags.map(|f| condition(ins.op & 15, f)), target)),
            0xC3 | 0xC2 | 0xCC => Some(Next::End),
            _ => None,
        }
    }

    fn load(&self, va: u64, size: u8) -> Option<u64> {
        if !self.pe.read_only(va) || !matches!(size, 4 | 8) {
            return None;
        }
        let bytes = self.pe.d.get(self.pe.va2off(va)?..)?.get(..size as usize)?;
        let mut raw = [0u8; 8];
        raw[..bytes.len()].copy_from_slice(bytes);
        Some(u64::from_le_bytes(raw))
    }

    /// SSE instructions: what they do to the tracked low floats.
    fn sse(&mut self, st: &mut State, ins: &Insn, pc: u64, bytes: &[u8]) -> Option<Next> {
        let next = pc + ins.len as u64;
        let m = ins.modrm?;
        let reg = (((m >> 3) & 7) | ((ins.rex & 4) << 1)) as usize;
        let rm = (m >> 6 == 3).then_some(((m & 7) | ((ins.rex & 1) << 3)) as usize);
        let wide = if ins.rex & 8 != 0 { 8 } else { 4 };
        let (scalar, packed_int) = (ins.rep, ins.opsize16 && ins.rep.is_none());
        // the source operand: a register's tag, or a float constant when it is read-only memory
        let pe = self.pe;
        let from_memory = || -> Xmm {
            let constant = ins.mem.filter(|mem| mem.rip).map(|mem| next.wrapping_add(mem.disp as u64)).filter(|&va| pe.read_only(va));
            match constant.and_then(|va| Some((va, u32_at(&pe.d, pe.va2off(va)?).ok()?))) {
                Some((va, bits)) => Xmm::Const { bits, rva: (va - pe.base) as u32 },
                None => Xmm::Other,
            }
        };
        let src = rm.map_or_else(from_memory, |r| st.xmm[r]);
        let dst = st.xmm[reg];
        let sign_mask = |t: Xmm| matches!(t, Xmm::Const { bits: 0x8000_0000, .. });
        match ins.op {
            // moves into the register operand
            0x10 | 0x28 | 0x6F => {
                if (ins.op == 0x28 && scalar.is_some()) || (ins.op == 0x6F && !packed_int && scalar != Some(0xF3)) {
                    return None;
                }
                self.used(src);
                // a double loaded from memory is not a float constant
                st.xmm[reg] = if scalar == Some(0xF2) && rm.is_none() { Xmm::Other } else { src };
            }
            // moves out of it (to memory, which is not tracked, or to another register)
            0x11 | 0x29 | 0x7F | 0xD6 | 0x13 | 0x17 => {
                if (ins.op == 0x7F && !packed_int && scalar != Some(0xF3)) || (ins.op == 0xD6 && !packed_int) {
                    return None;
                }
                self.used(dst);
                if let Some(r) = rm {
                    st.xmm[r] = dst;
                }
            }
            // movd / movq xmm, r/m: where the point count enters a float register
            0x6E if packed_int => {
                let points = rm.is_some_and(|r| st.gpr[r] == Val::Points);
                st.xmm[reg] = if points { Xmm::PointsInt } else { Xmm::Other };
            }
            0x7E if packed_int => {
                self.used(dst);
                if let Some(r) = rm {
                    st.set(r as u8, wide, ins.rex, None);
                }
            }
            0x7E if scalar == Some(0xF3) => {
                self.used(src);
                st.xmm[reg] = if rm.is_some() { src } else { Xmm::Other };
            }
            // int -> float: cvtdq2ps on a moved value, or cvtsi2ss straight from the general register
            0x5B if scalar.is_none() && !ins.opsize16 && src == Xmm::PointsInt => st.xmm[reg] = Xmm::PointsFloat,
            0x2A if scalar == Some(0xF3) && rm.is_some_and(|r| st.gpr[r] == Val::Points) => st.xmm[reg] = Xmm::PointsFloat,
            0x2A if scalar.is_some() => {
                if rm.is_some_and(|r| st.gpr[r] == Val::Points) {
                    self.seen.unexpected.insert(pc, hex(bytes));
                }
                st.xmm[reg] = Xmm::Other;
            }
            // float -> int, compares, sign masks into a general register
            0x2C | 0x2D if scalar.is_some() => {
                self.guard(&[src], pc, bytes);
                st.set(reg as u8, wide, ins.rex, None);
            }
            0x2E | 0x2F => st.flags = None,
            0x50 | 0xD7 => st.set(reg as u8, 4, ins.rex, None),
            // xorps / xorpd: zeroing, or the sign flip of a scaled value
            0x57 if scalar.is_none() => {
                st.xmm[reg] = match dst {
                    _ if rm == Some(reg) => Xmm::Other,
                    Xmm::Scaled { event, negated } if sign_mask(src) => {
                        self.seen.flips.insert((event, pc));
                        Xmm::Scaled { event, negated: !negated }
                    }
                    _ => {
                        self.guard(&[dst, src], pc, bytes);
                        Xmm::Other
                    }
                };
            }
            // the two scalings
            0x58 if scalar == Some(0xF3) && dst == Xmm::PointsFloat && rm == Some(reg) => st.xmm[reg] = self.scale(pc, How::Double, reg as u8),
            0x59 if scalar == Some(0xF3) => {
                st.xmm[reg] = match (dst, src) {
                    (Xmm::PointsFloat, Xmm::Const { bits, rva }) | (Xmm::Const { bits, rva }, Xmm::PointsFloat) => self.scale(pc, How::Mul { bits, rva }, reg as u8),
                    _ => {
                        self.guard(&[dst, src], pc, bytes);
                        Xmm::Other
                    }
                };
            }
            // every other float arithmetic and conversion (square roots and conversions only read their source)
            0x51..=0x5F => {
                let reads_destination = !matches!(ins.op, 0x51..=0x53 | 0x5A | 0x5B);
                let read = if reads_destination { [dst, src] } else { [src, src] };
                self.guard(&read, pc, bytes);
                st.xmm[reg] = Xmm::Other;
            }
            // shuffles and partial loads move floats around without computing with them
            0x12 | 0x14 | 0x15 | 0x16 | 0xC6 | 0xC2 => {
                self.used(dst);
                self.used(src);
                st.xmm[reg] = Xmm::Other;
            }
            // SSE2 integer instructions (never applied to the floats followed here)
            0x60..=0x6D | 0x70..=0x76 | 0xD0..=0xD5 | 0xD8..=0xFE if packed_int => {
                self.guard(&[dst, src], pc, bytes);
                st.xmm[reg] = Xmm::Other;
                if let Some(r) = rm.filter(|_| (0x71..=0x73).contains(&ins.op)) {
                    st.xmm[r] = Xmm::Other;
                }
            }
            _ => return None,
        }
        Some(Next::Fall)
    }
}

// ---- from the walk to the constants ----------------------------------------------------------------------------

/// Where the method receives the bonus type: which register, and which byte of it.
struct Input {
    reg: u8,
    byte: u32,
    size: u8,
    describe: String,
}

const REG_NAMES: [&str; 16] = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];

/// Find the parameter that carries the bonus type from the method's signature in the metadata, and from the
/// Windows x64 calling convention the register it arrives in. The parameter is the enum itself or a small struct
/// with a field of it (`ModBonusType { BonusType type; DamageType damageType; }`), passed by value.
fn bonus_type_input(g: &Gen, method: usize, bonus_def: usize) -> Result<Input, String> {
    let pass_in_register = |size: Option<i64>| matches!(size, Some(1 | 2 | 4 | 8));
    let is_static = g.meta.method_is_static(method)?;
    let ret = g.types.kind(g.meta.method_return(method)? as usize)?;
    // a struct that does not fit a register is returned through a hidden first pointer argument
    let hidden_return = match ret.ty {
        T_VALUETYPE => !pass_in_register(g.table_size(ret.data as usize, true)?),
        crate::il2cpp::T_GENERICINST => return Err("the method returns a generic type; its calling convention is not worked out here".into()),
        _ => false,
    };
    for (i, (name, ty)) in g.meta.method_params(method)?.into_iter().enumerate() {
        let k = g.types.kind(ty as usize)?;
        if k.byref || k.ty != T_VALUETYPE {
            continue;
        }
        let def = k.data as usize;
        let (byte, field) = if def == bonus_def {
            (0, String::new())
        } else {
            let fields = g.instance_fields(def)?;
            let Some(f) = fields.iter().find(|f| f.kind.ty == T_VALUETYPE && f.kind.data as usize == bonus_def) else { continue };
            ((f.off - g.header) as u32, format!(", field {}", f.name))
        };
        if !pass_in_register(g.table_size(def, true)?) {
            return Err(format!("parameter {name} is a struct passed by reference, which the walk does not follow"));
        }
        let slot = hidden_return as usize + !is_static as usize + i;
        let Some(&reg) = [1u8, 2, 8, 9].get(slot) else { return Err(format!("parameter {name} is passed on the stack, which the walk does not follow")) };
        let size = g.table_size(bonus_def, true)?.filter(|s| matches!(s, 1 | 2 | 4)).ok_or("unexpected size of the BonusType enum")? as u8;
        return Ok(Input { reg, byte, size, describe: format!("{} (parameter {name}{field}, byte {byte})", REG_NAMES[reg as usize]) });
    }
    Err(format!("no parameter of {TYPE}.{METHOD} carries a {BONUS_ENUM}"))
}

/// The decimal number a 32-bit float constant stands for: the shortest one that reads back as the same float
/// (0.8, not the 0.800000011920929 that the float's exact value is as a double).
fn float_value(bits: u32) -> f64 {
    format!("{}", f32::from_bits(bits)).parse().unwrap_or(f64::NAN)
}

fn extract(spec: &Spec, value: i64, input: &Input, entry: u64, run: &mut Run) -> Result<(f64, J), String> {
    let mut initial = State { gpr: [UNKNOWN; 16], xmm: [Xmm::Other; 16], flags: None };
    let shift = 8 * input.byte;
    initial.gpr[input.reg as usize] = Val::Bits { v: (value as u64) << shift, known: mask(input.size) << shift };
    run.seen = Seen::default();
    run.explore(entry, initial)?;
    let seen = &run.seen;

    let at = |a: u64| format!("{a:#x}");
    let list = |addrs: &mut dyn Iterator<Item = u64>| addrs.map(at).collect::<Vec<_>>().join(", ");
    if seen.points_calls.is_empty() {
        return Err(format!("no call to {}.{} is reachable for {BONUS_ENUM}.{}, so the point count cannot be followed", POINTS_SOURCE.0, POINTS_SOURCE.1, spec.member));
    }
    if let Some((a, bytes)) = seen.unexpected.iter().next() {
        return Err(format!("the point count goes through arithmetic that is not a plain scaling at {a:#x} (instruction bytes {bytes})"));
    }
    let scale = match seen.scales.as_slice() {
        [one] => one,
        [] => return Err(format!("no scaling of the point count (mulss with a constant, or addss x, x) is reachable for {BONUS_ENUM}.{}", spec.member)),
        many => return Err(format!("{} different scalings of the point count are reachable for {BONUS_ENUM}.{} (at {}); expected one", many.len(), spec.member, list(&mut many.iter().map(|s| s.at)))),
    };
    if seen.table_calls.is_empty() == spec.table_path {
        return Err(match spec.table_path {
            true => format!("expected a second path through {}.{} for this bonus type, found none", TABLE_LOOKUP.0, TABLE_LOOKUP.1),
            false => format!("this bonus type now has a path through {}.{} (at {}); it had none", TABLE_LOOKUP.0, TABLE_LOOKUP.1, list(&mut seen.table_calls.iter().copied())),
        });
    }
    // the sign: every later read of the scaled value must agree on it
    let negated = match seen.uses.iter().map(|u| u.1).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().as_slice() {
        [one] => *one,
        [] => return Err(format!("the value scaled at {:#x} is never used", scale.at)),
        _ => return Err(format!("the value scaled at {:#x} is negated on some paths only", scale.at)),
    };
    if negated != spec.negated {
        return Err(match spec.negated {
            true => format!("expected the value scaled at {:#x} to be negated afterwards (xorps with the sign mask); it is not", scale.at),
            false => format!("the value scaled at {:#x} is negated afterwards (at {}); it was not", scale.at, list(&mut seen.flips.iter().map(|f| f.1))),
        });
    }

    let xmm = format!("xmm{}", scale.reg);
    let mut evidence = vec![
        ("method", J::Str(format!("{TYPE}.{METHOD} at {entry:#x}"))),
        ("bonus_type", J::Str(format!("{BONUS_ENUM}.{} = {value}, in {}", spec.member, input.describe))),
        ("at", J::Str(at(scale.at))),
    ];
    let number = match scale.how {
        How::Mul { bits, rva } => {
            let number = float_value(bits);
            if !(number.is_finite() && number > 0.0) {
                return Err(format!("the factor at {:#x} is {number}, not a positive number", run.pe.base + rva as u64));
            }
            evidence.push(("how", J::Str(format!("mulss {xmm}, dword ptr [{:#x}] (the point count, converted to float, times the float stored there)", run.pe.base + rva as u64))));
            evidence.push(("constant_at", J::Str(at(run.pe.base + rva as u64))));
            evidence.push(("float_bits", J::Str(format!("{bits:#010x}"))));
            number
        }
        How::Double => {
            evidence.push(("how", J::Str(format!("addss {xmm}, {xmm} (the point count, converted to float, added to itself: no stored constant)"))));
            2.0
        }
    };
    evidence.insert(0, ("value", J::Float(number)));
    evidence.push(("negated_after", match seen.flips.iter().next() {
        Some(flip) if negated => J::Str(format!("{} (xorps with the sign mask)", at(flip.1))),
        _ => J::Bool(false),
    }));
    evidence.push(("crafting_table_path", match seen.table_calls.iter().next() {
        Some(&call) => J::Str(format!("call {}.{} at {}", TABLE_LOOKUP.0, TABLE_LOOKUP.1, at(call))),
        None => J::Bool(false),
    }));
    Ok((number, J::Obj(evidence.into_iter().map(|(k, v)| (k.to_string(), v)).collect())))
}

/// `{name: number}` for the four constants, plus `evidence: {name: {...}}` (where each was found and how it
/// appears; addresses are for the DLL at its preferred image base). An error names every constant that could not
/// be read and what was found instead.
pub fn code_constants(g: &Gen) -> Result<J, String> {
    let (method, entry) = g.find_method(TYPE, METHOD)?;
    let bonus_def = g.def(BONUS_ENUM)?;
    let input = bonus_type_input(g, method, bonus_def)?;
    let J::Obj(members) = g.types.constants(&[bonus_def])? else { unreachable!("constants() returns an object") };
    let mut run = Run { pe: g.pe, points_fn: g.method(POINTS_SOURCE.0, POINTS_SOURCE.1)?, table_fn: g.method(TABLE_LOOKUP.0, TABLE_LOOKUP.1)?, seen: Seen::default() };

    let (mut out, mut evidence, mut errors) = (J::obj(), J::obj(), Vec::new());
    for spec in SPECS {
        let value = match members.iter().find(|(k, _)| k == spec.member) {
            Some((_, J::Int(v))) => *v,
            _ => {
                errors.push(format!("{}: the enum {BONUS_ENUM} has no member {}", spec.name, spec.member));
                continue;
            }
        };
        match extract(spec, value, &input, entry, &mut run) {
            Ok((number, ev)) => {
                out.set(spec.name, J::Float(number));
                evidence.set(spec.name, ev);
            }
            Err(e) => errors.push(format!("{}: {e}", spec.name)),
        }
    }
    if !errors.is_empty() {
        return Err(format!("{TYPE}.{METHOD} at {entry:#x} does not have the expected shape. {}", errors.join("; ")));
    }
    out.set("evidence", evidence);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x1_8000_0000;
    const ENTRY: u64 = BASE + 0x1000;
    const POINTS_FN: u64 = ENTRY + 0x200;

    /// A method of the shape the real one has, in miniature: get the points, switch on the low byte of r8,
    /// scale by a constant (type 5), double and negate (type 2), or return (anything else).
    fn miniature() -> Vec<u8> {
        let mut c = vec![
            0x41, 0x0F, 0xB7, 0xD8, // 00 movzx ebx, r8w
            0xE8, 0xF7, 0x01, 0x00, 0x00, // 04 call POINTS_FN
            0x8B, 0xF0, // 09 mov esi, eax
            0x80, 0xFB, 0x02, // 0b cmp bl, 2
            0x74, 0x1B, // 0e je 2b
            0x80, 0xFB, 0x05, // 10 cmp bl, 5
            0x75, 0x2F, // 13 jne 44
            0x66, 0x0F, 0x6E, 0xCE, // 15 movd xmm1, esi
            0x0F, 0x5B, 0xC9, // 19 cvtdq2ps xmm1, xmm1
            0xF3, 0x0F, 0x59, 0x0D, 0x2C, 0x00, 0x00, 0x00, // 1c mulss xmm1, [rip+2c] -> 50
            0xF3, 0x0F, 0x11, 0x0F, // 24 movss [rdi], xmm1
            0xC3, 0xCC, 0xCC, // 28 ret
            0x66, 0x0F, 0x6E, 0xCE, // 2b movd xmm1, esi
            0x0F, 0x5B, 0xC9, // 2f cvtdq2ps xmm1, xmm1
            0xF3, 0x0F, 0x58, 0xC9, // 32 addss xmm1, xmm1
            0x0F, 0x57, 0x0D, 0x23, 0x00, 0x00, 0x00, // 36 xorps xmm1, [rip+23] -> 60
            0xF3, 0x0F, 0x11, 0x0F, // 3d movss [rdi], xmm1
            0xC3, 0xCC, 0xCC, // 41 ret
            0xC3, // 44 ret
        ];
        c.resize(0x50, 0xCC);
        c.extend(1.25f32.to_le_bytes());
        c.resize(0x60, 0);
        c.extend([0x8000_0000u32; 4].iter().flat_map(|m| m.to_le_bytes()));
        c
    }

    /// Walk `code` with only the low byte of r8 known.
    fn walk(code: Vec<u8>, bonus_type: u64) -> Result<Seen, String> {
        let pe = Pe::test_image(BASE, code);
        let mut run = Run { pe: &pe, points_fn: POINTS_FN, table_fn: 0, seen: Seen::default() };
        let mut st = State { gpr: [UNKNOWN; 16], xmm: [Xmm::Other; 16], flags: None };
        st.gpr[8] = Val::Bits { v: bonus_type, known: 0xFF };
        run.explore(ENTRY, st)?;
        Ok(run.seen)
    }

    #[test]
    fn follows_the_dispatch_to_one_scaling() {
        let seen = walk(miniature(), 5).unwrap();
        assert_eq!(seen.scales.len(), 1);
        assert!(seen.scales[0].at == ENTRY + 0x1C && seen.scales[0].how == How::Mul { bits: 1.25f32.to_bits(), rva: 0x1050 });
        assert!(seen.flips.is_empty() && seen.unexpected.is_empty());
        assert_eq!(seen.uses.iter().copied().collect::<Vec<_>>(), [(0, false)]);
        assert_eq!(seen.points_calls.len(), 1);

        let seen = walk(miniature(), 2).unwrap();
        assert!(seen.scales.len() == 1 && seen.scales[0].at == ENTRY + 0x32 && seen.scales[0].how == How::Double);
        assert_eq!(seen.flips.iter().copied().collect::<Vec<_>>(), [(0, ENTRY + 0x36)]);
        assert_eq!(seen.uses.iter().copied().collect::<Vec<_>>(), [(0, true)]);

        // any other type takes neither branch
        assert!(walk(miniature(), 7).unwrap().scales.is_empty());
    }

    #[test]
    fn an_unknown_bonus_type_reaches_both_scalings() {
        let pe = Pe::test_image(BASE, miniature());
        let mut run = Run { pe: &pe, points_fn: POINTS_FN, table_fn: 0, seen: Seen::default() };
        run.explore(ENTRY, State { gpr: [UNKNOWN; 16], xmm: [Xmm::Other; 16], flags: None }).unwrap();
        assert_eq!(run.seen.scales.len(), 2);
    }

    #[test]
    fn other_arithmetic_on_the_points_is_reported_not_read_as_a_factor() {
        let mut code = miniature();
        code[0x1E] = 0x5E; // mulss -> divss
        let seen = walk(code, 5).unwrap();
        assert!(seen.scales.is_empty() && seen.unexpected.contains_key(&(ENTRY + 0x1C)));

        let mut code = miniature();
        code[0x34] = 0x5C; // addss -> subss
        let seen = walk(code, 2).unwrap();
        assert!(seen.scales.is_empty() && seen.unexpected.contains_key(&(ENTRY + 0x32)));
    }

    #[test]
    fn an_instruction_it_does_not_model_stops_the_walk() {
        let mut code = miniature();
        code[0x24..0x26].copy_from_slice(&[0x0F, 0xA2]); // cpuid in the path of type 5
        assert!(walk(code.clone(), 5).unwrap_err().contains("0x180001024"));
        // ... but not a walk that never gets there
        assert!(walk(code, 2).is_ok());
    }

    #[test]
    fn float_constants_as_decimals() {
        assert_eq!(float_value(0x3F4C_CCCD), 0.8);
        assert_eq!(float_value(1.25f32.to_bits()), 1.25);
    }

    /// The real method, with its code changed in memory in the ways a game update might change it: each change
    /// must surface as an error for the constant concerned (or as the new value), never as the old value.
    #[test]
    #[ignore = "needs the installed game: set ZAR_GAME_DIR"]
    fn changed_code_is_noticed() {
        use crate::layout::{dll_path, meta_path};
        use crate::meta::Meta;
        let game = std::path::PathBuf::from(std::env::var_os("ZAR_GAME_DIR").expect("ZAR_GAME_DIR"));
        let meta = Meta::load(&meta_path(&game)).unwrap();
        let mut pe = Pe::load(&dll_path(&game)).unwrap();
        let original = code_constants(&Gen::new(&meta, &pe).unwrap()).unwrap().to_value();
        let site = |name: &str, key: &str| -> usize {
            let text = original["evidence"][name][key].as_str().unwrap();
            let va = u64::from_str_radix(text.trim_start_matches("0x").split(' ').next().unwrap(), 16).unwrap();
            pe.va2off(va).unwrap()
        };
        let (reload, crit, accuracy) = ("enhancement_reload_percent_per_mod_point", "enhancement_crit_percent_per_mod_point", "accuracy_percent_per_mod_point");
        let magazine = "enhancement_magazine_percent_per_mod_point";
        // (what changes, file offset, bytes there, bytes put there, constant affected, expected in the error)
        let double_at = site(reload, "at");
        let flip_at = site(reload, "negated_after");
        let crit_at = site(crit, "at");
        let accuracy_at = site(accuracy, "at");
        let magazine_const = site(magazine, "constant_at");
        type Case<'a> = (&'a str, usize, &'a [u8], &'a [u8], &'a str, &'a str);
        let cases: [Case; 4] = [
            ("the doubling becomes a subtraction", double_at + 2, &[0x58], &[0x5C], reload, "not a plain scaling"),
            ("the sign flip disappears", flip_at, &[0x0F, 0x57, 0xCE], &[0x0F, 0x28, 0xC9], reload, "negated"),
            ("the multiplication becomes a division", crit_at + 2, &[0x59], &[0x5E], crit, "not a plain scaling"),
            ("the multiplication is on doubles", accuracy_at, &[0xF3], &[0xF2], accuracy, "not a plain scaling"),
        ];
        for (what, off, old, new, name, expected) in cases {
            assert_eq!(&pe.d[off..off + old.len()], old, "{what}: the code is not what this test was written for");
            pe.d[off..off + new.len()].copy_from_slice(new);
            let err = code_constants(&Gen::new(&meta, &pe).unwrap()).unwrap_err();
            println!("{what}: {err}");
            assert!(err.contains(name) && err.contains(expected), "{what}: {err}");
            // the other three are still read, so the error must not mention them
            assert_eq!(SPECS.iter().filter(|s| err.contains(s.name)).count(), 1, "{what}: {err}");
            pe.d[off..off + old.len()].copy_from_slice(old);
        }
        // a changed constant is simply the new value
        pe.d[magazine_const..magazine_const + 4].copy_from_slice(&5.5f32.to_le_bytes());
        let changed = code_constants(&Gen::new(&meta, &pe).unwrap()).unwrap().to_value();
        assert_eq!(changed[magazine], 5.5);
        assert_eq!(changed[crit], original[crit]);
    }
}
