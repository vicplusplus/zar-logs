//! Read-only access to the game's process, for each system the app runs on.
//!
//! Both versions offer the same few things: `find_pid`, `Process::open` / `alive` / `read` / `read_into` /
//! `image_path`, and `module_base`. Nothing else is ever done to the game: no writes, no injection, no
//! suspension.
//!
//! * Windows (`windows.rs`): `OpenProcess` with `PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION` and
//!   `ReadProcessMemory`.
//! * Linux (`linux.rs`), where the game runs under Proton / Wine: `/proc` to find it and `process_vm_readv` to
//!   read it. The game is the same Windows program there, so its memory looks exactly as it does on Windows.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
