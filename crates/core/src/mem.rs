//! The memory source of the decoder: the running game, read from outside.
//!
//! The decoder only ever calls `read(addr, size)`: `ReadProcessMemory` on a handle that can do nothing else on
//! Windows, `process_vm_readv` on Linux (see proc/).

#[cfg(any(windows, target_os = "linux"))]
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(any(windows, target_os = "linux"))]
use crate::proc::Process;

pub fn le_u16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) }
pub fn le_u32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
pub fn le_i32(b: &[u8], o: usize) -> i32 { i32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
pub fn le_u64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }
pub fn le_i64(b: &[u8], o: usize) -> i64 { i64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }
/// A C# / native `float`, widened: every number the meter handles is an f64.
pub fn le_f32(b: &[u8], o: usize) -> f64 { f32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as f64 }
pub fn le_f64(b: &[u8], o: usize) -> f64 { f64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

pub trait Mem {
    /// Starts a poll; returns its wall time (unix seconds).
    fn begin_frame(&mut self) -> f64;
    fn end_frame(&mut self, keep: bool);
    fn read(&mut self, addr: u64, size: usize) -> Option<Vec<u8>>;
    /// A second reader for slow table walks on another thread. None = do the walk inline.
    fn fork(&self) -> Option<Box<dyn Mem + Send>> { None }

    fn u64(&mut self, addr: u64) -> u64 {
        self.read(addr, 8).map(|b| le_u64(&b, 0)).unwrap_or(0)
    }
    fn u32(&mut self, addr: u64) -> u32 {
        self.read(addr, 4).map(|b| le_u32(&b, 0)).unwrap_or(0)
    }
    /// UTF-8 NUL-terminated string (native `char*`).
    fn cstr(&mut self, addr: u64, maxlen: usize) -> String {
        if addr == 0 {
            return String::new();
        }
        let b = match self.read(addr, maxlen) {
            Some(b) => b,
            None => {
                // the string may sit at the end of a mapped region: fall back to short reads
                let mut b = Vec::new();
                let mut i = 0;
                while i < maxlen {
                    match self.read(addr + i as u64, 8) {
                        Some(c) => {
                            let done = c.contains(&0);
                            b.extend_from_slice(&c);
                            if done {
                                break;
                            }
                        }
                        None => break,
                    }
                    i += 8;
                }
                b
            }
        };
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..n]).into_owned()
    }
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(any(windows, target_os = "linux"))]
pub struct Live {
    p: Arc<Process>,
}

#[cfg(any(windows, target_os = "linux"))]
impl Live {
    pub fn new(p: Arc<Process>) -> Live { Live { p } }
    pub fn process(&self) -> &Arc<Process> { &self.p }
}

#[cfg(any(windows, target_os = "linux"))]
impl Mem for Live {
    fn begin_frame(&mut self) -> f64 { now() }
    fn end_frame(&mut self, _keep: bool) {}
    fn read(&mut self, addr: u64, size: usize) -> Option<Vec<u8>> {
        if size == 0 { None } else { self.p.read(addr, size) }
    }
    fn fork(&self) -> Option<Box<dyn Mem + Send>> { Some(Box::new(Live { p: self.p.clone() })) }
}
