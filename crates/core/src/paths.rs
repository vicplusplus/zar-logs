//! Where the game is installed. Read-only: looks things up, never writes.
//!
//! The first folder that holds a GameAssembly.dll wins:
//!   1. the ZAR_GAME_DIR environment variable (explicit override)
//!   2. the folder of a running zombieassault.exe
//!   3. Steam's library list: every library in steamapps/libraryfolders.vdf that has the game's app manifest
//!   4. the default Steam location on C:

use std::path::{Path, PathBuf};

pub const APP_ID: &str = "2747660";

/// This app's identifier (the same as in `src-tauri/tauri.conf.json`; a test there keeps them equal). Windows
/// keeps the viewer's window data in `%LOCALAPPDATA%\<identifier>`, and the app keeps its own data next to it in
/// the same folder, so that one folder is everything the app has stored, and the installer's "delete the
/// application data" removes all of it.
pub const APP_IDENTIFIER: &str = "com.vicplusplus.zar-logs";

/// The data folder: `ZARLOGS_DATA`, else `%LOCALAPPDATA%\<identifier>`. Holds `config.json`, `logs`, `cache`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ZARLOGS_DATA").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(APP_IDENTIFIER)
}

/// Where the data folder was before the app had an installer (`%LOCALAPPDATA%\zar-logs`, which is now where the
/// program itself is installed). `None` when the data folder was set by hand.
pub fn old_data_dir() -> Option<PathBuf> {
    if std::env::var_os("ZARLOGS_DATA").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("zar-logs"))
}
const DEFAULT: &str = r"C:\Program Files (x86)\Steam\steamapps\common\Zombie Assault";
const MARKER: &str = "GameAssembly.dll";

fn ok(p: &Path) -> bool { p.join(MARKER).is_file() }

#[cfg(windows)]
fn running() -> Option<PathBuf> {
    let pid = crate::proc::find_pid(crate::capture::GAME_EXE)?;
    crate::proc::Process::open(pid).ok()?.image_path()?.parent().map(Path::to_path_buf)
}
#[cfg(not(windows))]
fn running() -> Option<PathBuf> { None }

#[cfg(windows)]
fn reg_string(local_machine: bool, key: &str, name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (k, n) = (wide(key), wide(name));
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let hive = if local_machine { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER };
    // SAFETY: query-only registry read into a buffer whose byte length is passed alongside.
    let rc = unsafe { RegGetValueW(hive, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut len) };
    if rc != 0 {
        return None;
    }
    let chars = (len as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..chars]))
}

/// Steam installation folders that exist: ZAR_STEAM_DIR, the registry, then the usual place.
pub fn steam_roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        // Windows paths: the registry and the default spell the same folder with different case
        let same = |q: &PathBuf| q.to_string_lossy().eq_ignore_ascii_case(&p.to_string_lossy());
        if p.is_dir() && !out.iter().any(same) {
            out.push(p);
        }
    };
    if let Ok(p) = std::env::var("ZAR_STEAM_DIR") {
        add(PathBuf::from(p));
    }
    #[cfg(windows)]
    {
        for (lm, key, name) in [(false, r"Software\Valve\Steam", "SteamPath"), (true, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"),
            (true, r"SOFTWARE\Valve\Steam", "InstallPath")] {
            if let Some(p) = reg_string(lm, key, name) {
                add(PathBuf::from(p.replace('/', "\\")));
            }
        }
        add(PathBuf::from(r"C:\Program Files (x86)\Steam"));
    }
    out
}

/// The quoted value after `"key"` on each line of a Valve .vdf / .acf text file.
fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    text.lines().filter_map(|line| {
        let rest = line.trim().strip_prefix(&needle)?.trim();
        let inner = rest.strip_prefix('"')?.strip_suffix('"')?;
        Some(inner.replace("\\\\", "\\"))
    }).collect()
}

fn from_steam() -> Option<PathBuf> {
    let mut libs: Vec<PathBuf> = Vec::new();
    for root in steam_roots() {
        if !libs.contains(&root) {
            libs.push(root.clone());
        }
        if let Ok(text) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
            for p in vdf_values(&text, "path") {
                let p = PathBuf::from(p);
                if !libs.contains(&p) {
                    libs.push(p);
                }
            }
        }
    }
    for lib in libs {
        let apps = lib.join("steamapps");
        let Ok(text) = std::fs::read_to_string(apps.join(format!("appmanifest_{APP_ID}.acf"))) else { continue };
        let dir = vdf_values(&text, "installdir").into_iter().next().unwrap_or_else(|| "Zombie Assault".into());
        let p = apps.join("common").join(dir);
        if ok(&p) {
            return Some(p);
        }
    }
    None
}

/// (folder, how it was found). None when the game is nowhere to be found.
pub fn find_game() -> Option<(PathBuf, &'static str)> {
    if let Ok(p) = std::env::var("ZAR_GAME_DIR") {
        let p = PathBuf::from(p);
        return ok(&p).then_some((p, "ZAR_GAME_DIR"));
    }
    if let Some(p) = running().filter(|p| ok(p)) {
        return Some((p, "running game"));
    }
    if let Some(p) = from_steam() {
        return Some((p, "Steam library"));
    }
    let p = PathBuf::from(DEFAULT);
    ok(&p).then_some((p, "default location"))
}

#[cfg(test)]
mod tests {
    use super::vdf_values;

    #[test]
    fn reads_library_paths() {
        let text = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t\t\"label\"\t\t\"\"\n\t}\n}\n";
        assert_eq!(vdf_values(text, "path"), vec!["D:\\SteamLibrary".to_string()]);
    }
}

/// Which version of the game is installed. Everything the app derives from the game's files (memory layout, names,
/// values, pictures) is stored under this id and made again when it changes.
#[derive(Clone, Debug, PartialEq)]
pub struct GameVersion {
    /// Short id of this exact set of game files (hex): a hash of everything below.
    pub id: String,
    /// Steam's build number, when the game sits in a Steam library.
    pub build: Option<String>,
    /// The content catalog's own hash (changes with every content update).
    pub catalog_hash: Option<String>,
    /// (file, size, modification time) of the files the layout is derived from.
    pub files: Vec<(String, u64, u64)>,
}

/// Reads the version markers of the game installed at `game`. Fails when the folder does not hold the game.
pub fn game_version(game: &Path) -> Result<GameVersion, String> {
    let data = game.join("zombieassault_Data");
    let stamp = |rel: &str, p: PathBuf| -> Result<(String, u64, u64), String> {
        let md = std::fs::metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
        Ok((rel.to_string(), md.len(), mtime))
    };
    let files = vec![
        stamp(MARKER, game.join(MARKER))?,
        stamp("global-metadata.dat", data.join("il2cpp_data").join("Metadata").join("global-metadata.dat"))?,
        stamp("catalog.bin", data.join("StreamingAssets").join("aa").join("catalog.bin"))?,
    ];
    let catalog_hash = std::fs::read_to_string(data.join("StreamingAssets").join("aa").join("catalog.hash")).ok()
        .map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    // <library>/steamapps/common/<game>  ->  <library>/steamapps/appmanifest_<app id>.acf
    let build = game.parent().and_then(Path::parent)
        .and_then(|apps| std::fs::read_to_string(apps.join(format!("appmanifest_{APP_ID}.acf"))).ok())
        .and_then(|text| vdf_values(&text, "buildid").into_iter().next());
    // FNV-1a over the markers: small, stable, and only ever compared for equality
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |s: &str| {
        for b in s.bytes().chain(std::iter::once(0)) {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(build.as_deref().unwrap_or(""));
    eat(catalog_hash.as_deref().unwrap_or(""));
    for (name, size, mtime) in &files {
        eat(&format!("{name}:{size}:{mtime}"));
    }
    Ok(GameVersion { id: format!("{h:016x}"), build, catalog_hash, files })
}
