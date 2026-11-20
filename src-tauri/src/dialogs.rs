//! "Show in folder" (how a log is shared: its file is shown in Explorer, to drag wherever it should go) and the
//! file dialog for importing logs.
//!
//! Both are Windows' own: the user picks, the page never names a path. The dialog blocks the thread that asks
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
    pub fn open_many(_owner: isize) -> Vec<PathBuf> { let _ = (FILTER, ANSWER_LEN); Vec::new() }
    pub fn reveal(_path: &Path) -> bool { false }
}

/// Asks for one or more files to open. Empty when the user cancels. `owner` is the window the dialog belongs to
/// (0 = none).
pub fn open_many(owner: isize) -> Vec<PathBuf> { imp::open_many(owner) }
/// Opens the file's folder in Explorer with the file selected.
pub fn reveal(path: &Path) -> bool { imp::reveal(path) }
