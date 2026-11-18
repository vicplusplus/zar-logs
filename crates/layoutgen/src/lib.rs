//! Generates the meter's memory "layout" from the game's own files, without running the game.
//!
//! zar-logs reads the memory of Zombie Assault: Resurgence (a Unity IL2CPP game) and so needs the offsets of the
//! fields it reads, the values of the enums it interprets and two addresses to start from. Those are different
//! in every game build, and all of them can be derived statically from two files in the game folder:
//!
//! * `zombieassault_Data/il2cpp_data/Metadata/global-metadata.dat`: names of types, fields, methods, enum
//!   constants (`meta`);
//! * `GameAssembly.dll`: the compiled code plus the tables IL2CPP registers at startup, i.e. field offsets,
//!   type descriptions and method addresses (`pe`, `il2cpp`), and two facts that only exist as machine code
//!   (`x86`).
//!
//! `layout` puts them together into one JSON document. It started as a port of the Python meter's generator
//! (`python -m zarmeter layout`, with the parts of the repo tools `zar_meta.py` / `zdis.py` that one uses) and
//! is a superset of its output since format version 11, so that the app needs neither Python nor any offset,
//! size or enum value of the game in its own source. Besides the layout, the crate reads enum tables on request
//! (`enums`) and a few factors of the gear maths that only exist as machine code (`code_constants`).
//!
//! Strictly read-only: the two files are read once each and nothing is written anywhere by the library.

use std::path::Path;

mod bytes;
mod constants;
mod il2cpp;
mod json;
mod layout;
mod meta;
mod pe;
mod x86;

pub use layout::VERSION;

/// The layout of the game build installed in `game_dir` (the folder that contains `GameAssembly.dll`).
///
/// Takes about a second: it reads ~80 MB and searches the DLL a few times. Callers should cache the result under
/// [`cache_file_name`].
pub fn generate(game_dir: &Path) -> Result<serde_json::Value, String> {
    Ok(layout::build(game_dir)?.to_value())
}

/// The same document as text, with keys in the order the generator emits them (fields in declaration = memory
/// order, enum members in declaration order) and one space of indentation per level, the format of the Python
/// generator's files, which makes two layout files diffable. A `serde_json::Value` cannot carry that order
/// (it sorts keys unless serde_json's `preserve_order` feature is on for the whole build), hence the separate
/// function; a reader that needs declaration order must parse this text with an order-preserving parser.
pub fn generate_text(game_dir: &Path) -> Result<String, String> {
    Ok(layout::build(game_dir)?.to_text())
}

/// The values of game enums that are not part of the layout: `{EnumName: {Member: value}}`, one entry per
/// requested name, keyed by the name exactly as given.
///
/// A name is a short type name (`DamageType`) or a full one (`NativeBuildInterop.DamageType`). Short names are
/// not unique in the game (nested types have no namespace); every type with that name contributes its members,
/// which is the rule of the Python tools (`zar_meta.Types.enum`) this mirrors. Members are all fields with a
/// constant, so a class of `const`s can be read the same way; values are integers for enums, and may be floats
/// or strings for such constants. Unlike the Python function, a name that matches no type is an error (naming
/// every such name) instead of an empty result, so a typo or a renamed enum is noticed.
///
/// Each call reads both game files again (well under a second), so ask for all the enums needed in one call.
pub fn enums(game_dir: &Path, names: &[&str]) -> Result<serde_json::Value, String> {
    Ok(layout::enums(game_dir, names)?.to_value())
}

/// The factors of the gear maths that exist only in the game's machine code, read from `GameAssembly.dll`:
///
/// ```json
/// { "accuracy_percent_per_mod_point": 1.25, "enhancement_reload_percent_per_mod_point": 2.0,
///   "enhancement_crit_percent_per_mod_point": 0.8, "enhancement_magazine_percent_per_mod_point": 4.0,
///   "evidence": { "<name>": { "value", "method", "bonus_type", "at", "how", "constant_at", "float_bits",
///                             "negated_after", "crafting_table_path" } } }
/// ```
///
/// The four numbers are top-level keys; `evidence` says for each where in the method
/// `PassiveBonusDefinition.TranslateModPointToRealBonus` it was found and how it appears there, so that a value
/// can be checked by hand with a disassembler (addresses are for the DLL at its preferred image base). A float
/// constant is given as the shortest decimal that reads back as the same 32-bit float (0.8, not
/// 0.800000011920929); `float_bits` has the stored bits.
///
/// Nothing is looked up by address: the method is found by name, the branch of each bonus type by following the
/// method's own dispatch for the value of that `BonusType` member, and the constant as the operand that scales
/// the mod point count there (see `constants.rs`). If the code of a game build does not have that shape, the
/// result is an error naming each constant concerned and what was found, never a guess; the caller should then
/// keep the value of the previous build and flag it as unverified.
pub fn code_constants(game_dir: &Path) -> Result<serde_json::Value, String> {
    Ok(layout::with_game(game_dir, constants::code_constants)?.to_value())
}

/// `layout_<dll size>_<dll mtime seconds>.json`: the name under which the layout of this game build is cached.
/// Size and modification time of the DLL identify a build without hashing 66 MB on every start.
pub fn cache_file_name(game_dir: &Path) -> Result<String, String> {
    let (size, mtime) = layout::stamp(&layout::dll_path(game_dir))?;
    Ok(format!("layout_{size}_{mtime}.json"))
}
