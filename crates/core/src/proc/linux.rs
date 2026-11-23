//! Read-only access to another process's memory: Linux, where the game runs under Proton / Wine.
//!
//! Under Proton the game is still `zombieassault.exe` with `GameAssembly.dll` loaded into it, just hosted by
//! Wine, so its memory holds the same structures at the same offsets as on Windows. What differs is how it is
//! found and read:
//!
//! * the process: `/proc/<pid>/cmdline` (Wine shows the Windows program's path there, `Z:\...\zombieassault.exe`)
//!   or, failing that, its 15-character name in `/proc/<pid>/comm`;
//! * `GameAssembly.dll`'s load address: a mapping of that file in `/proc/<pid>/maps`, or, when Wine copied the
//!   image instead of mapping it, the region whose PE header exports that name;
//! * its memory: `process_vm_readv`, the read-only call. Nothing is attached to the game (no `ptrace` attach, no
//!   stop), nothing is written, and `/proc/<pid>/mem` is never opened.
//!
//! Linux only lets a program read another one's memory when it is allowed to trace it. With the kernel's usual
//! setting (Yama `ptrace_scope` 1, the default on Ubuntu, Fedora, Arch and SteamOS) that means the reader is the
//! game's parent, or has the `CAP_SYS_PTRACE` capability. `Process::open` checks this and says what to do.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[repr(C)]
struct IoVec {
    base: *mut std::ffi::c_void,
    len: usize,
}

extern "C" {
    /// glibc / musl wrapper of the system call (Linux 3.2+).
    fn process_vm_readv(pid: i32, local: *const IoVec, local_n: usize, remote: *const IoVec, remote_n: usize, flags: usize) -> isize;
}

const EPERM: i32 = 1;

/// The last component of a path written either way: `Z:\a\b.exe` and `/a/b.exe` both give `b.exe`.
fn basename(p: &str) -> &str { p.rsplit(['/', '\\']).next().unwrap_or(p) }

fn proc_file(pid: u32, name: &str) -> Option<Vec<u8>> { std::fs::read(format!("/proc/{pid}/{name}")).ok() }

/// The first argument of the process's command line (its program, as the process presents it).
fn argv0(pid: u32) -> Option<String> {
    let raw = proc_file(pid, "cmdline")?;
    let first = raw.split(|&b| b == 0).next()?;
    // Wine may also write the whole command line into the first argument
    let s = String::from_utf8_lossy(first).into_owned();
    Some(match s.find(".exe ").or_else(|| s.find(".EXE ")) {
        Some(i) => s[..i + 4].to_string(),
        None => s,
    })
}

/// Process id of the first process whose program is called `exe_name` (case-insensitive).
pub fn find_pid(exe_name: &str) -> Option<u32> {
    // the kernel keeps at most 15 bytes of a program's name in `comm`
    let short: String = exe_name.chars().take(15).collect();
    let mut by_comm = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if argv0(pid).is_some_and(|a| basename(&a).eq_ignore_ascii_case(exe_name)) {
            return Some(pid);
        }
        if by_comm.is_none() {
            let comm = proc_file(pid, "comm").map(|c| String::from_utf8_lossy(&c).trim_end().to_string());
            if comm.is_some_and(|c| c.eq_ignore_ascii_case(&short)) {
                by_comm = Some(pid);
            }
        }
    }
    by_comm
}

/// One line of `/proc/<pid>/maps`.
#[derive(Debug, Clone)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub readable: bool,
    pub offset: u64,
    /// The mapped file, empty for anonymous memory.
    pub path: String,
}

pub fn regions(pid: u32) -> Vec<Region> {
    let Some(raw) = proc_file(pid, "maps") else { return Vec::new() };
    String::from_utf8_lossy(&raw).lines().filter_map(parse_region).collect()
}

/// `start-end perms offset dev inode   path`, single spaces between the first five fields; the path may hold spaces.
fn parse_region(line: &str) -> Option<Region> {
    let mut f = line.splitn(6, ' ');
    let (range, perms, offset) = (f.next()?, f.next()?, f.next()?);
    let (_dev, _inode) = (f.next()?, f.next()?);
    let path = f.next().unwrap_or("").trim_start();
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    let (a, b) = range.split_once('-')?;
    Some(Region {
        start: u64::from_str_radix(a, 16).ok()?,
        end: u64::from_str_radix(b, 16).ok()?,
        readable: perms.starts_with('r'),
        offset: u64::from_str_radix(offset, 16).ok()?,
        path: path.to_string(),
    })
}

/// `{lower-case file name: (base, size)}` of the files mapped into a process (base = lowest address of the file).
pub fn modules(pid: u32) -> HashMap<String, (u64, u32)> {
    let mut span: HashMap<String, (u64, u64)> = HashMap::new();
    for r in regions(pid) {
        if !r.path.starts_with('/') {
            continue; // anonymous memory, [heap], [stack] and the like
        }
        let e = span.entry(basename(&r.path).to_lowercase()).or_insert((r.start, r.end));
        e.0 = e.0.min(r.start);
        e.1 = e.1.max(r.end);
    }
    span.into_iter().map(|(k, (a, b))| (k, (a, (b - a).min(u32::MAX as u64) as u32))).collect()
}

/// The name a PE image at `base` gives itself in its export directory, read through `read`.
fn pe_export_name(read: &dyn Fn(u64, usize) -> Option<Vec<u8>>, base: u64) -> Option<String> {
    let le32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let head = read(base, 0x400)?;
    if &head[..2] != b"MZ" {
        return None;
    }
    let pe = le32(&head, 0x3c) as usize;
    if pe + 24 + 120 > head.len() || &head[pe..pe + 4] != b"PE\0\0" {
        return None;
    }
    let opt = pe + 24;
    // the export directory is data directory 0: at 112 in a PE32+ optional header, at 96 in a PE32 one
    let dirs = match u16::from_le_bytes([head[opt], head[opt + 1]]) {
        0x20b => opt + 112,
        0x10b => opt + 96,
        _ => return None,
    };
    let rva = le32(&head, dirs) as u64;
    if rva == 0 {
        return None;
    }
    let exports = read(base + rva, 40)?;
    let name_rva = le32(&exports, 12) as u64;
    let raw = read(base + name_rva, 64)?;
    let n = raw.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&raw[..n]).into_owned())
}

/// Load address of the module called `name` (lower case) in the process. None while the game is still loading.
pub fn module_base(p: &Process, name: &str) -> Option<u64> {
    let all = regions(p.pid);
    // Wine maps the DLL from its file where it can: the file's first mapping is the image's start
    if let Some(r) = all.iter().filter(|r| r.offset == 0 && basename(&r.path).eq_ignore_ascii_case(name)).min_by_key(|r| r.start) {
        return Some(r.start);
    }
    // otherwise the image was copied into anonymous memory: find its header by the name it exports. Images
    // start on a 64 KiB boundary.
    let read = |a: u64, n: usize| p.read(a, n);
    all.iter()
        .filter(|r| r.readable && r.start % 0x10000 == 0 && r.end - r.start >= 0x1000)
        .find(|r| pe_export_name(&read, r.start).is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .map(|r| r.start)
}

/// Field 22 of `/proc/<pid>/stat` (when the process started), and whether it has already exited.
fn stat(pid: u32) -> Option<(u64, bool)> {
    let raw = String::from_utf8_lossy(&proc_file(pid, "stat")?).into_owned();
    // the name in parentheses may hold spaces and parentheses: the fields that matter come after the last ')'
    let rest: Vec<&str> = raw.rsplit_once(')')?.1.split_whitespace().collect();
    let state = rest.first()?;
    Some((rest.get(19)?.parse().ok()?, *state == "Z" || *state == "X"))
}

/// A process the meter may read (and does nothing else to).
pub struct Process {
    pub pid: u32,
    started: u64,
}

impl Process {
    /// Fails, with what to do about it, when Linux does not let this program read the game.
    pub fn open(pid: u32) -> Result<Process, String> {
        let (started, dead) = stat(pid).ok_or_else(|| format!("process {pid} is gone"))?;
        if dead {
            return Err(format!("process {pid} has exited"));
        }
        let p = Process { pid, started };
        // one small read tells whether reading is allowed at all
        let probe = regions(pid).into_iter().find(|r| r.readable).ok_or_else(|| format!("cannot see the memory map of process {pid}"))?;
        let mut b = [0u8; 8];
        match p.read_raw(probe.start, &mut b) {
            Ok(_) => Ok(p),
            Err(e) if e == EPERM => Err(permission_help()),
            Err(e) => Err(format!("reading process {pid} failed, error {e}")),
        }
    }

    pub fn alive(&self) -> bool { stat(self.pid).is_some_and(|(started, dead)| started == self.started && !dead) }

    fn read_raw(&self, addr: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let local = IoVec { base: buf.as_mut_ptr().cast(), len: buf.len() };
        let remote = IoVec { base: addr as *mut _, len: buf.len() };
        // SAFETY: `local` describes `buf`, which is valid and writable for its length; the remote range is only
        // read, by the kernel, and a bad address there is an error, not undefined behaviour.
        let n = unsafe { process_vm_readv(self.pid as i32, &local, 1, &remote, 1, 0) };
        if n < 0 {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        } else {
            Ok(n as usize)
        }
    }

    /// Fills `buf` from the target's memory. False when the range is not (fully) readable.
    pub fn read_into(&self, addr: u64, buf: &mut [u8]) -> bool {
        if buf.is_empty() || addr == 0 {
            return false;
        }
        self.read_raw(addr, buf) == Ok(buf.len())
    }

    /// `size` bytes at `addr`, or None when the range is not readable.
    pub fn read(&self, addr: u64, size: usize) -> Option<Vec<u8>> {
        let mut v = vec![0u8; size];
        self.read_into(addr, &mut v).then_some(v)
    }

    /// Where the game's program is, as a Linux path (its folder is what matters). Found from the folder of the
    /// mapped `GameAssembly.dll`, else the game's working folder (Steam starts it in its own folder), else the
    /// Windows path Wine shows (`Z:\` is `/`).
    pub fn image_path(&self) -> Option<PathBuf> {
        let exe = argv0(self.pid).map(|a| basename(&a).to_string()).unwrap_or_else(|| crate::capture::GAME_EXE.to_string());
        let marker = |d: &Path| d.join("GameAssembly.dll").is_file();
        if let Some(r) = regions(self.pid).into_iter().find(|r| r.path.starts_with('/') && basename(&r.path).eq_ignore_ascii_case(crate::capture::GAME_DLL)) {
            if let Some(dir) = Path::new(&r.path).parent() {
                return Some(dir.join(exe));
            }
        }
        if let Ok(cwd) = std::fs::read_link(format!("/proc/{}/cwd", self.pid)) {
            if marker(&cwd) {
                return Some(cwd.join(exe));
            }
        }
        let a = argv0(self.pid)?;
        let unix = a.strip_prefix("Z:").or_else(|| a.strip_prefix("z:"))?.replace('\\', "/");
        Some(PathBuf::from(unix))
    }
}

/// What to tell the user when Linux refuses the read.
fn permission_help() -> String {
    let me = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "zar-logs".into());
    let scope = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").map(|s| s.trim().to_string()).unwrap_or_default();
    format!(
        "Linux does not let zar-logs read the game (kernel.yama.ptrace_scope = {scope}). Allow it once with: \
         sudo setcap cap_sys_ptrace=ep \"{me}\"  (read-only use; see the README, \"Linux\")"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A child that sleeps, presenting itself as a Windows program the way Wine does.
    fn fake_game(name: &str) -> Kill {
        let child = Command::new("sleep").arg("30").arg0(format!(r"Z:\games\Zombie Assault\{name}")).spawn().expect("spawn sleep");
        // let it exec
        for _ in 0..100 {
            if argv0(child.id()).is_some_and(|a| a.ends_with(name)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Kill(child)
    }

    #[test]
    fn names_are_taken_from_either_kind_of_path() {
        assert_eq!(basename(r"Z:\a b\zombieassault.exe"), "zombieassault.exe");
        assert_eq!(basename("/a b/GameAssembly.dll"), "GameAssembly.dll");
        assert_eq!(basename("plain"), "plain");
    }

    #[test]
    fn maps_lines_are_parsed_with_spaces_in_paths() {
        let r = parse_region("7f0000000000-7f0000010000 r--p 00000000 08:01 1234       /home/u/Steam/steamapps/common/Zombie Assault/GameAssembly.dll").unwrap();
        assert_eq!((r.start, r.end, r.readable, r.offset), (0x7f00_0000_0000, 0x7f00_0001_0000, true, 0));
        assert_eq!(r.path, "/home/u/Steam/steamapps/common/Zombie Assault/GameAssembly.dll");
        let anon = parse_region("7f0000010000-7f0000020000 ---p 00000000 00:00 0 ").unwrap();
        assert_eq!((anon.readable, anon.path.as_str()), (false, ""));
        let gone = parse_region("1000-2000 r-xp 00001000 08:01 9 /tmp/x.dll (deleted)").unwrap();
        assert_eq!((gone.offset, gone.path.as_str()), (0x1000, "/tmp/x.dll"));
    }

    #[test]
    fn the_game_is_found_by_the_windows_path_wine_shows() {
        let child = fake_game("zartest-find.exe");
        assert_eq!(find_pid("ZARTEST-FIND.EXE"), Some(child.0.id()));
        assert_eq!(find_pid("no-such-program.exe"), None);
    }

    #[test]
    fn a_child_process_can_be_read() {
        // the reader is the parent here, which Linux allows even with ptrace_scope 1, as with a launcher
        let child = fake_game("zartest-read.exe");
        let p = Process::open(child.0.id()).expect("open own child");
        assert!(p.alive());
        let mods = modules(p.pid);
        let (base, size) = *mods.get("sleep").expect("sleep is mapped");
        assert!(size > 0);
        assert_eq!(p.read(base, 4).as_deref(), Some(&b"\x7fELF"[..]));
        assert!(p.read(0, 4).is_none());
        assert!(p.read(8, 4).is_none(), "an unmapped address is refused, not a crash");
        drop(child);
        assert!(!p.alive());
    }

    #[test]
    fn a_copied_pe_image_is_found_by_its_export_name() {
        // a minimal PE32+ header with an export directory naming itself, in this process's own memory
        let mut img = vec![0u8; 0x1000];
        img[..2].copy_from_slice(b"MZ");
        img[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        img[0x80..0x84].copy_from_slice(b"PE\0\0");
        let opt = 0x80 + 24;
        img[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        img[opt + 112..opt + 116].copy_from_slice(&0x400u32.to_le_bytes());
        img[0x400 + 12..0x400 + 16].copy_from_slice(&0x500u32.to_le_bytes());
        img[0x500..0x510].copy_from_slice(b"GameAssembly.dll");
        let me = Process { pid: std::process::id(), started: 0 };
        let read = |a: u64, n: usize| me.read(a, n);
        assert_eq!(pe_export_name(&read, img.as_ptr() as u64).as_deref(), Some("GameAssembly.dll"));
        img[0] = b'X';
        assert_eq!(pe_export_name(&read, img.as_ptr() as u64), None);
    }

    #[test]
    fn the_game_folder_comes_from_the_windows_path_when_nothing_else_tells() {
        let child = fake_game("zartest-path.exe");
        let p = Process::open(child.0.id()).unwrap();
        assert_eq!(p.image_path(), Some(PathBuf::from("/games/Zombie Assault/zartest-path.exe")));
    }
}
