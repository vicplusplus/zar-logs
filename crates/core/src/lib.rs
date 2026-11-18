//! zar-logs core: reads the running game from outside and turns what it reads into records.
//!
//! Nothing in this crate writes to the game, its files, its save or its process. The process is opened with
//! `PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION` only (see `proc`), and the only memory API is
//! `ReadProcessMemory`.

pub mod capture;
pub mod consts;
pub mod layout;
pub mod mem;
pub mod names;
pub mod paths;
#[cfg(windows)]
pub mod proc;
pub mod records;
pub mod session;
pub mod store;
pub mod world;

#[cfg(windows)]
use std::sync::Arc;

/// (process, GameAssembly.dll base) of the running game, or None when it is not running (or still starting).
#[cfg(windows)]
pub fn attach() -> Option<(Arc<proc::Process>, u64)> {
    let pid = proc::find_pid(capture::GAME_EXE)?;
    let base = proc::modules(pid).get(capture::GAME_DLL)?.0;
    Some((Arc::new(proc::Process::open(pid).ok()?), base))
}
