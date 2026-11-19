//! Everything the app knows about the game is read from the installed game's own files and stored per game
//! version:
//!
//! ```text
//! <cache>/game/<version id>/manifest.json   which game version this is, and what made the files
//!                          /layout.json     memory layout: offsets, sizes, enum values (from the game's metadata)
//!                          /tables.json     definitions, localisation text, enum values, the class skills table
//!                          /names.json      the names pack
//!                          /icons/          pictures
//! <cache>/current.json                      the version used last (so logs can be viewed without the game)
//! ```
//!
//! The version id is a hash of the game's own markers (Steam build number, content catalog hash, size and time of
//! the code and metadata files): see `zar_logs_core::paths::game_version`. When the installed game's id has no
//! complete folder, the files are made again from the game; older folders are kept (a few), so nothing has to be
//! derived twice and a layout is always tied to the build it was read from.
//!
//! Nothing here is shipped with the app, nothing is written anywhere but the cache folder, and the game's files
//! are only read.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use zar_logs_buildview::{BuildData, Tables};
use zar_logs_core::layout::Layout;
use zar_logs_core::paths::{game_version, GameVersion};

/// Bump when this app changes what it writes into a version folder: folders made by an older app are made again.
const GENERATOR: u64 = 5;
/// How many game versions' folders are kept.
const KEEP_VERSIONS: usize = 3;

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| format!("{}: {e}", path.display()))
}
fn read_json(path: &Path) -> Option<Value> { serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok() }

/// The folder of one game version.
pub struct Store {
    pub version: GameVersion,
    pub dir: PathBuf,
}

impl Store {
    /// The folder for the game installed at `game` (created when new).
    pub fn open(cache: &Path, game: &Path) -> Result<Store, String> {
        let version = game_version(game)?;
        let dir = cache.join("game").join(&version.id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Store { version, dir })
    }
    pub fn layout(&self) -> PathBuf { self.dir.join("layout.json") }
    fn tables(&self) -> PathBuf { self.dir.join("tables.json") }
    fn names(&self) -> PathBuf { self.dir.join("names.json") }
    fn icons(&self) -> PathBuf { self.dir.join("icons") }
    fn manifest(&self) -> PathBuf { self.dir.join("manifest.json") }

    /// True when the tables of this folder were made by this generator and are all there.
    fn complete(&self) -> bool {
        read_json(&self.manifest()).is_some_and(|m| m["generator"].as_u64() == Some(GENERATOR) && m["tables"].as_bool() == Some(true))
            && self.tables().is_file() && self.names().is_file() && self.icons().join("index.json").is_file()
    }

    fn write_manifest(&self, tables: bool) -> Result<(), String> {
        let v = &self.version;
        let files: Vec<Value> = v.files.iter().map(|(name, size, mtime)| json!({ "file": name, "size": size, "modified": mtime })).collect();
        write_atomic(&self.manifest(), &serde_json::to_string_pretty(&json!({
            "game_version": v.id, "steam_build": v.build, "catalog_hash": v.catalog_hash, "files": files,
            "generator": GENERATOR, "layout": zar_logs_layoutgen::VERSION, "names": zar_logs_namespack::VERSION,
            "tables": tables, "app": env!("CARGO_PKG_VERSION"),
        })).unwrap_or_default())
    }
}

/// The folder used last, for viewing logs when the game is not installed or not found.
pub fn last_dir(cache: &Path) -> Option<PathBuf> {
    let id = read_json(&cache.join("current.json"))?["game_version"].as_str()?.to_string();
    Some(cache.join("game").join(id)).filter(|d| d.is_dir())
}

fn remember(cache: &Path, store: &Store) {
    let _ = write_atomic(&cache.join("current.json"), &json!({ "game_version": store.version.id, "steam_build": store.version.build }).to_string());
    // drop the folders of game versions that are long gone
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(cache.join("game")).into_iter().flatten().flatten()
        .filter(|e| e.path().is_dir() && e.path() != store.dir)
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
    dirs.sort();
    while dirs.len() + 1 > KEEP_VERSIONS {
        let _ = std::fs::remove_dir_all(dirs.remove(0).1);
    }
}

/// The memory layout for the game installed at `game`: from that version's folder, or derived from the game's
/// metadata (under a second) when it has none yet, i.e. on first start and after every game update.
pub fn ensure_layout(cache: &Path, game: &Path, note: &dyn Fn(&str)) -> Result<Layout, String> {
    let store = Store::open(cache, game)?;
    if let Ok(lay) = Layout::load(&store.layout()) {
        return Ok(lay);
    }
    note("reading the game's memory layout");
    write_atomic(&store.layout(), &zar_logs_layoutgen::generate_text(game)?)?;
    if !store.manifest().is_file() {
        store.write_manifest(false)?;
    }
    Layout::load(&store.layout())
}

/// What `ensure_tables` hands back.
pub struct Ready {
    /// the version folder in use
    pub dir: PathBuf,
    /// something the user should know about these tables (a value that could not be read from this version)
    pub warning: Option<String>,
    /// true when the tables were made anew from the game (first start, game update)
    pub made: bool,
    pub builds: BuildData,
}

/// The tables for the game installed at `game`: definitions, texts, names, skills and pictures. Read from that
/// version's folder; made from the game's asset bundles when the folder is new or was written by an older app.
pub fn ensure_tables(cache: &Path, game: &Path, note: &dyn Fn(&str)) -> Result<Ready, String> {
    let store = Store::open(cache, game)?;
    let made = !store.complete();
    let tables = if made {
        note("reading the game's values and names");
        // the few factors read from the game's code fall back to the previous version's when they cannot be read
        let previous = last_dir(cache).and_then(|d| read_json(&d.join("tables.json"))).map(|mut t| t["code_constants"].take());
        let tables = Tables::read_game(game, previous.as_ref())?;
        write_atomic(&store.tables(), &tables.to_json().to_string())?;
        write_atomic(&store.names(), &tables.names.to_string())?;
        note("reading the game's pictures");
        zar_logs_unity::pictures::ensure(game, &store.icons(), &tables.definitions, false)?;
        store.write_manifest(true)?;
        tables
    } else {
        let names = read_json(&store.names()).ok_or("the stored names could not be read")?;
        Tables::from_json(read_json(&store.tables()).ok_or("the stored tables could not be read")?, names)?
    };
    let builds = BuildData::from_tables(&tables, &store.icons().join("index.json"))?;
    let warning = (tables.code_constants["verified"].as_bool() == Some(false)).then(|| format!(
        "some factors could not be read from this game version's code and were carried over from the previous version ({})",
        tables.code_constants["problem"].as_str().unwrap_or("unknown reason")));
    remember(cache, &store);
    Ok(Ready { dir: store.dir, warning, made, builds })
}

/// The build viewer's data from a version folder alone (no game needed): for viewing logs.
pub fn stored_builds(dir: &Path) -> Option<BuildData> {
    let tables = Tables::from_json(read_json(&dir.join("tables.json"))?, read_json(&dir.join("names.json"))?).ok()?;
    BuildData::from_tables(&tables, &dir.join("icons").join("index.json")).ok()
}
