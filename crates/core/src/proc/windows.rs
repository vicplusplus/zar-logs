//! Read-only access to another process's memory: Windows.
//!
//! The handle is opened with `PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION` and nothing else: no VM_WRITE,
//! no VM_OPERATION, no CREATE_THREAD, no debug rights. The only memory API used is `ReadProcessMemory`. Nothing is
//! injected and the target is never suspended, so the game runs exactly as it would without the meter.

use std::collections::HashMap;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW, MODULEENTRY32W,
    PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// The complete set of rights the meter ever asks for.
pub const ACCESS: u32 = PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION;
const STILL_ACTIVE: u32 = 259;
const ERROR_BAD_LENGTH: u32 = 24;

fn wide_to_string(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

/// Process id of the first process whose executable is called `exe_name` (case-insensitive).
pub fn find_pid(exe_name: &str) -> Option<u32> {
    // SAFETY: plain toolhelp enumeration; the entry struct is sized as the API requires.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            if wide_to_string(&e.szExeFile).eq_ignore_ascii_case(exe_name) {
                found = Some(e.th32ProcessID);
                break;
            }
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
        found
    }
}

/// `{lower-case module name: (base, size)}` of a process (toolhelp snapshot; no process handle involved).
pub fn modules(pid: u32) -> HashMap<String, (u64, u32)> {
    let mut out = HashMap::new();
    // SAFETY: as above.
    unsafe {
        let mut snap: HANDLE = std::ptr::null_mut();
        for _ in 0..20 {
            // ERROR_BAD_LENGTH while the loader is busy: retry
            snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid);
            if !snap.is_null() && snap != INVALID_HANDLE_VALUE {
                break;
            }
            if GetLastError() != ERROR_BAD_LENGTH {
                return out;
            }
            snap = std::ptr::null_mut();
        }
        if snap.is_null() || snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: MODULEENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut ok = Module32FirstW(snap, &mut e);
        while ok != 0 {
            out.insert(wide_to_string(&e.szModule).to_lowercase(), (e.modBaseAddr as u64, e.modBaseSize));
            ok = Module32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    out
}

/// Load address of the module called `name` (lower case) in the process. None while the game is still loading.
pub fn module_base(p: &Process, name: &str) -> Option<u64> { modules(p.pid).get(name).map(|m| m.0) }

/// A process opened for reading only.
pub struct Process {
    pub pid: u32,
    h: HANDLE,
}

// SAFETY: the handle is only ever used for ReadProcessMemory / queries, which are thread-safe.
unsafe impl Send for Process {}
unsafe impl Sync for Process {}

impl Process {
    pub fn open(pid: u32) -> Result<Process, String> {
        // SAFETY: OpenProcess with the read-only access mask above.
        let h = unsafe { OpenProcess(ACCESS, 0, pid) };
        if h.is_null() {
            return Err(format!("OpenProcess({pid}) failed (read-only access), error {}", unsafe { GetLastError() }));
        }
        Ok(Process { pid, h })
    }

    pub fn alive(&self) -> bool {
        let mut code = 0u32;
        // SAFETY: valid handle, valid out pointer.
        unsafe { GetExitCodeProcess(self.h, &mut code) != 0 && code == STILL_ACTIVE }
    }

    /// Fills `buf` from the target's memory. False when the range is not (fully) readable.
    pub fn read_into(&self, addr: u64, buf: &mut [u8]) -> bool {
        if buf.is_empty() || addr == 0 {
            return false;
        }
        let mut n = 0usize;
        // SAFETY: `buf` is a valid writable buffer of the given length in this process.
        let ok = unsafe { ReadProcessMemory(self.h, addr as *const _, buf.as_mut_ptr() as *mut _, buf.len(), &mut n) };
        ok != 0 && n == buf.len()
    }

    /// `size` bytes at `addr`, or None when the range is not readable.
    pub fn read(&self, addr: u64, size: usize) -> Option<Vec<u8>> {
        let mut v = vec![0u8; size];
        self.read_into(addr, &mut v).then_some(v)
    }

    /// Full path of the process's executable.
    pub fn image_path(&self) -> Option<PathBuf> {
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        // SAFETY: buffer and length match.
        let ok = unsafe { QueryFullProcessImageNameW(self.h, 0, buf.as_mut_ptr(), &mut n) };
        (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by us and is closed once.
        unsafe { CloseHandle(self.h) };
    }
}
