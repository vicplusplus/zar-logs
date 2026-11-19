//! The two file dialogs of the app (where to save an exported log, which logs to import) and "show in folder".
//!
//! These are Windows' own dialogs: the user picks the place, the page never names a path. They block the thread
//! that asks until the user has answered.

use std::path::{Path, PathBuf};

/// What the dialogs offer: the logs this app writes, or any file.
const FILTER: &str = "zar-logs mission log (*.json.gz)\0*.json.gz\0All files (*.*)\0*.*\0\0";
/// Room for the answer, in UTF-16 units: the longest path Windows hands back, many times over for a selection of
/// several files.
const ANSWER_LEN: usize = 32 * 1024;

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, GetSaveFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR,
        OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

    /// The dialog's settings: `answer` holds the suggested name going in and the choice coming out.
    fn request(owner: isize, title: &[u16], filter: &[u16], answer: &mut [u16], flags: u32) -> OPENFILENAMEW {
        // SAFETY: the structure is plain data and all-zero is its documented "nothing set" state.
        let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = owner as _;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = answer.as_mut_ptr();
        ofn.nMaxFile = answer.len() as u32;
        ofn.lpstrTitle = title.as_ptr();
        // never change this process's working directory, whatever folder the user browses to
        ofn.Flags = flags | OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_HIDEREADONLY;
        ofn
    }

    pub fn save_as(owner: isize, suggested: &str) -> Option<PathBuf> {
        let (title, filter) = (wide("Export mission log"), FILTER.encode_utf16().collect::<Vec<u16>>());
        let mut answer = vec![0u16; ANSWER_LEN];
        let name: Vec<u16> = suggested.encode_utf16().take(ANSWER_LEN - 1).collect();
        answer[..name.len()].copy_from_slice(&name);
        let mut ofn = request(owner, &title, &filter, &mut answer, OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST);
        // SAFETY: every pointer in `ofn` is to a buffer that outlives the call; the dialog writes at most
        // `nMaxFile` units into `answer`.
        if unsafe { GetSaveFileNameW(&mut ofn) } == 0 {
            return None; // cancelled
        }
        let end = answer.iter().position(|c| *c == 0).unwrap_or(0);
        (end > 0).then(|| PathBuf::from(String::from_utf16_lossy(&answer[..end])))
    }

    pub fn open_many(owner: isize) -> Vec<PathBuf> {
        let (title, filter) = (wide("Import mission logs"), FILTER.encode_utf16().collect::<Vec<u16>>());
        let mut answer = vec![0u16; ANSWER_LEN];
        let mut ofn = request(owner, &title, &filter, &mut answer, OFN_ALLOWMULTISELECT | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST);
        // SAFETY: as in `save_as`.
        if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
            return Vec::new();
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
        // Explorer wants the path quoted inside the switch, which is not how arguments are normally quoted
        std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", path.display())).spawn().is_ok()
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn save_as(_owner: isize, _suggested: &str) -> Option<PathBuf> { let _ = (FILTER, ANSWER_LEN); None }
    pub fn open_many(_owner: isize) -> Vec<PathBuf> { Vec::new() }
    pub fn reveal(_path: &Path) -> bool { false }
}

/// Asks where to save a file, with `suggested` as its name. None when the user cancels. `owner` is the window the
/// dialog belongs to (0 = none).
pub fn save_as(owner: isize, suggested: &str) -> Option<PathBuf> { imp::save_as(owner, suggested) }
/// Asks for one or more files to open. Empty when the user cancels.
pub fn open_many(owner: isize) -> Vec<PathBuf> { imp::open_many(owner) }
/// Opens the file's folder in Explorer with the file selected.
pub fn reveal(path: &Path) -> bool { imp::reveal(path) }
