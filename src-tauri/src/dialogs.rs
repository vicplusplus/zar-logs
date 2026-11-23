//! "Show in folder" (how a log is shared: its file is shown in Explorer, to drag wherever it should go) and the
//! file dialog for importing logs.
//!
//! Both are the system's own (Windows' dialog and Explorer; on Linux the desktop's file chooser and file manager):
//! the user picks, the page never names a path. The dialog blocks the thread that asks
//! until the user has answered.

use std::path::{Path, PathBuf};

/// What the dialog offers: the logs this app writes, or any file.
const FILTER: &str = "zar-logs mission log (*.json.gz)\0*.json.gz\0All files (*.*)\0*.*\0\0";
/// Room for the answer, in UTF-16 units: the longest path Windows hands back, many times over for a selection of
/// several files.
const ANSWER_LEN: usize = 32 * 1024;

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
        OPENFILENAMEW,
    };

    pub fn open_many(owner: isize) -> Vec<PathBuf> {
        let title: Vec<u16> = "Import mission logs".encode_utf16().chain(std::iter::once(0)).collect();
        let filter: Vec<u16> = FILTER.encode_utf16().collect();
        let mut answer = vec![0u16; ANSWER_LEN];
        // SAFETY: the structure is plain data and all-zero is its documented "nothing set" state.
        let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = owner as _;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = answer.as_mut_ptr();
        ofn.nMaxFile = answer.len() as u32;
        ofn.lpstrTitle = title.as_ptr();
        // never change this process's working directory, whatever folder the user browses to
        ofn.Flags = OFN_ALLOWMULTISELECT | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_HIDEREADONLY;
        // SAFETY: every pointer in `ofn` is to a buffer that outlives the call; the dialog writes at most
        // `nMaxFile` units into `answer`.
        if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
            return Vec::new(); // cancelled
        }
        // one file: its full path. Several: the folder, then each file name, each ended by a zero, and one more
        // zero after the last.
        let mut parts: Vec<String> = Vec::new();
        let mut at = 0;
        while at < answer.len() && answer[at] != 0 {
            let end = answer[at..].iter().position(|c| *c == 0).map_or(answer.len(), |n| at + n);
            parts.push(String::from_utf16_lossy(&answer[at..end]));
            at = end + 1;
        }
        match parts.len() {
            0 => Vec::new(),
            1 => vec![PathBuf::from(&parts[0])],
            _ => parts[1..].iter().map(|name| Path::new(&parts[0]).join(name)).collect(),
        }
    }

    pub fn reveal(path: &Path) -> bool {
        use std::os::windows::process::CommandExt;
        // Explorer wants the path quoted inside the switch, which is not how arguments are normally quoted, and
        // with backslashes only: given a forward slash (a data folder set as "D:/logs") it opens some other folder
        let path = path.display().to_string().replace('/', "\\");
        std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn().is_ok()
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::process::{Command, Stdio};

    /// The desktop's own file chooser, through the helper programs that show one: zenity (GNOME, and installed
    /// with Steam almost everywhere, the Steam Deck included), else kdialog (KDE).
    pub fn open_many(_owner: isize) -> Vec<PathBuf> {
        let _ = (FILTER, ANSWER_LEN);
        let tries: [(&str, &[&str]); 2] = [
            ("zenity", &["--file-selection", "--multiple", "--separator=\n", "--title=Import mission logs",
                "--file-filter=zar-logs mission log | *.json.gz", "--file-filter=All files | *"]),
            ("kdialog", &["--getopenfilename", ".", "*.json.gz|zar-logs mission log", "--multiple", "--separate-output",
                "--title", "Import mission logs"]),
        ];
        for (program, args) in tries {
            // a helper that is not installed fails to start: try the next one. One that ran and was cancelled
            // has answered.
            let Ok(out) = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output() else { continue };
            return String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
        }
        Vec::new()
    }

    /// The file manager, with the file selected where it can do that (the freedesktop FileManager1 interface that
    /// Nautilus, Dolphin, Nemo, Thunar and others offer), else its folder.
    pub fn reveal(path: &Path) -> bool {
        let uri = file_uri(path);
        let shown = Command::new("dbus-send")
            .args(["--session", "--print-reply", "--dest=org.freedesktop.FileManager1", "--type=method_call",
                "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems"])
            .arg(format!("array:string:{uri}"))
            .arg("string:")
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status().is_ok_and(|s| s.success());
        shown || path.parent().is_some_and(|dir| run_detached(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }, &[&dir.display().to_string()]))
    }

    /// `file://` and the path, every byte outside the unreserved set percent-encoded.
    pub(super) fn file_uri(path: &Path) -> String {
        use std::os::unix::ffi::OsStrExt;
        let mut out = String::from("file://");
        for &b in path.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }
}

/// Starts a helper program without waiting for it (it is reaped on a thread of its own). False when it could
/// not be started.
#[cfg(not(windows))]
pub fn run_detached(program: &str, args: &[&str]) -> bool {
    use std::process::{Command, Stdio};
    match Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    #[test]
    fn file_uris_are_percent_encoded() {
        let p = std::path::Path::new("/home/u/.local/share/com.vicplusplus.zar-logs/logs/enc 1_Geode-Cavern.json.gz");
        assert_eq!(super::imp::file_uri(p), "file:///home/u/.local/share/com.vicplusplus.zar-logs/logs/enc%201_Geode-Cavern.json.gz");
    }
}

/// Asks for one or more files to open. Empty when the user cancels. `owner` is the window the dialog belongs to
/// (0 = none).
pub fn open_many(owner: isize) -> Vec<PathBuf> { imp::open_many(owner) }
/// Opens the file's folder in Explorer (or the desktop's file manager) with the file selected.
pub fn reveal(path: &Path) -> bool { imp::reveal(path) }
