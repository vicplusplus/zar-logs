//! zar-logs core: reads the running game from outside and turns what it reads into records.
//!
//! Nothing in this crate writes to the game, its files, its save or its process. On Windows the process is opened
//! with `PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION` only and the only memory API is `ReadProcessMemory`;
//! on Linux (the game under Proton) the only memory API is `process_vm_readv` (see `proc`).

pub mod capture;
pub mod consts;
pub mod layout;
pub mod mem;
pub mod names;
pub mod paths;
#[cfg(any(windows, target_os = "linux"))]
pub mod proc;
pub mod records;
pub mod session;
pub mod store;
pub mod world;

#[cfg(any(windows, target_os = "linux"))]
use std::sync::Arc;

/// (process, GameAssembly.dll base) of the running game. `Ok(None)` when it is not running (or still starting),
/// `Err` when it runs but this program may not read it (on Linux: what to do about it).
#[cfg(any(windows, target_os = "linux"))]
pub fn attach() -> Result<Option<(Arc<proc::Process>, u64)>, String> {
    let Some(pid) = proc::find_pid(capture::GAME_EXE) else { return Ok(None) };
    let p = match proc::Process::open(pid) {
        Ok(p) => p,
        // Windows: a refusal there is the game still starting (or exiting); try again later, as always
        Err(_) if cfg!(windows) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(proc::module_base(&p, capture::GAME_DLL).map(|base| (Arc::new(p), base)))
}
