//! ScriptableObject data: the game's definitions (weapons, abilities, enemies, levels...) as JSON.
//!
//! A ScriptableObject is stored as a `MonoBehaviour` object that is not attached to a GameObject. Which C# class it
//! is an instance of is given by its `m_Script` reference to a `MonoScript` object; all MonoScripts of this game are
//! in the `*_monoscripts_*.bundle`. The fields themselves are described by the type tree stored with the object's
//! type (see [`crate::typetree`]), so no knowledge of the game's code is needed.
//!
//! The output mirrors the dump the Python tooling made (`dump/so/<Class>.json`): per class, an object keyed by the
//! path id in decimal, each value being the object's fields plus `_name` (its `m_Name`) and `_bundle` (the bundle's
//! file name up to `_assets_`), minus `m_GameObject`, `m_Enabled` and `m_Script`.
//!
//! Cost: the bundles total 2 GB on disk and 4 GB decompressed, almost all of it meshes, textures and audio. A
//! SerializedFile's type table already says which script each MonoBehaviour type belongs to, so only the metadata
//! of each bundle and the blocks holding objects of the requested classes are decompressed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Map, Value};

use crate::bundle::Bundle;
use crate::serialized::{SerializedFile, CLASS_MONO_BEHAVIOUR, CLASS_MONO_SCRIPT};
use crate::typetree;

/// Bundles that hold no definitions (textures, audio, shaders, scenes, Unity's built-ins) and are not opened.
const SKIP: [&str; 6] = ["ui_textures", "wwise", "shadervariants", "_scenes_", "unitybuiltin", "monoscripts"];

/// The class given to an object whose script is not among the MonoScripts.
pub const UNKNOWN_CLASS: &str = "?";

/// Folder of the game's asset bundles.
pub fn bundles_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("zombieassault_Data").join("StreamingAssets").join("aa").join("StandaloneWindows64")
}

/// The `*.bundle` files of the game, sorted by name (case-insensitively, as the Python tooling sorted them).
pub fn bundles(game_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = bundles_dir(game_dir);
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {}", dir.display(), e))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "bundle"))
        .collect();
    out.sort_by_key(|p| file_name(p).to_lowercase());
    Ok(out)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Path id of every MonoScript -> its class name.
pub fn script_classes(game_dir: &Path) -> Result<HashMap<i64, String>, String> {
    let mut scripts = HashMap::new();
    for path in bundles(game_dir)?.iter().filter(|p| file_name(p).contains("monoscripts")) {
        let bundle = Bundle::open(path)?;
        for entry in bundle.entries().iter().filter(|e| e.is_serialized_file()) {
            let file = SerializedFile::open(&bundle, entry)?;
            for object in file.objects.iter().filter(|o| o.class_id == CLASS_MONO_SCRIPT) {
                let Some(tree) = file.tree(object) else { continue };
                let script = typetree::read(tree, &file.bytes(&bundle, object)?, &file.ref_types)?;
                let text = |key: &str| script.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
                if let Some(class) = text("m_ClassName").or(text("m_Name")) {
                    scripts.insert(object.path_id, class.to_string());
                }
            }
        }
    }
    if scripts.is_empty() {
        return Err("no MonoScript found: is the monoscripts bundle missing?".into());
    }
    Ok(scripts)
}

/// The ScriptableObjects of the given script classes: `{class: {path id: fields}}`. An empty `classes` means every
/// class (objects of an unknown script go under [`UNKNOWN_CLASS`]). A requested class without objects is absent
/// from the result.
pub fn dump(game_dir: &Path, classes: &[&str]) -> Result<HashMap<String, Map<String, Value>>, String> {
    let scripts = script_classes(game_dir)?;
    let wanted: HashSet<&str> = classes.iter().copied().collect();
    let paths: Vec<PathBuf> =
        bundles(game_dir)?.into_iter().filter(|p| !SKIP.iter().any(|s| file_name(p).contains(s))).collect();

    // One bundle is independent of the next, so they are read in parallel; the results are merged in bundle order
    // afterwards because an object present in two bundles must come from the later one, as in the Python dump.
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(paths.len().max(1));
    let mut found: Vec<(usize, Vec<Found>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = paths.get(i) else { break };
                        // A bundle that cannot be read is skipped, as the Python did: one odd file must not cost
                        // the whole dump.
                        if let Ok(objects) = read_bundle(path, &scripts, &wanted) {
                            out.push((i, objects));
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    found.sort_by_key(|(i, _)| *i);

    let mut out: HashMap<String, Map<String, Value>> = HashMap::new();
    for (class, path_id, fields) in found.into_iter().flat_map(|(_, objects)| objects) {
        out.entry(class).or_default().insert(path_id.to_string(), Value::Object(fields));
    }
    Ok(out)
}

/// The script class of the game's root scene component.
const GAME_RUNNER: &str = "GameRunner";
/// The Addressables group whose bundle has held the GameRunner in every build seen. Only a hint for where to look
/// first: the group name is stable across builds, the hash after it is not.
const GAME_RUNNER_GROUP: &str = "defaultlocalgroup_";

/// The `GameRunner` component: the game's master lists as asset references (`weapons`, `armourPieces`,
/// `gearModEnhancements`, `playerClassDefinitions`, `defaultCharacterLevelsTable`...).
///
/// Unlike the definitions [`dump`] returns, it is not a ScriptableObject but a MonoBehaviour on a GameObject of a
/// prefab, which is why `dump` leaves it out. The result is the object exactly as its type tree reads, the inner
/// object of the Python tool's `dump/GameRunner.json`: `m_GameObject`, `m_Enabled`, `m_Script` and `m_Name` are
/// kept, and no `_name` / `_bundle` is added.
///
/// It is found through the type tables, like the classes of `dump`: a bundle is opened, and only if one of its
/// MonoBehaviour types is built from the GameRunner script is any object read. Bundles of the
/// `defaultlocalgroup` group are tried first and the search stops at the first bundle that has the component, so
/// normally one bundle's metadata and one object are decompressed. `Ok(None)` when no bundle has it. Should a
/// bundle hold several, the last one in the file is returned, which is the one the Python tool ended up writing.
pub fn game_runner(game_dir: &Path) -> Result<Option<Map<String, Value>>, String> {
    let scripts = script_classes(game_dir)?;
    let mut paths: Vec<PathBuf> =
        bundles(game_dir)?.into_iter().filter(|p| !SKIP.iter().any(|s| file_name(p).contains(s))).collect();
    // A stable sort: the likely bundles first, everything else after them in its usual order.
    paths.sort_by_key(|p| !file_name(p).starts_with(GAME_RUNNER_GROUP));
    for path in &paths {
        let Ok(bundle) = Bundle::open(path) else { continue };
        let mut found = None;
        for entry in bundle.entries().iter().filter(|e| e.is_serialized_file()) {
            let Ok(file) = SerializedFile::open(&bundle, entry) else { continue };
            let is_runner = |path_id: i64| scripts.get(&path_id).is_some_and(|class| class == GAME_RUNNER);
            for object in file.objects.iter().filter(|o| o.class_id == CLASS_MONO_BEHAVIOUR) {
                if !file.script(object).is_some_and(|script| is_runner(script.path_id)) {
                    continue;
                }
                let Some(tree) = file.tree(object) else { continue };
                let Ok(Value::Object(fields)) = typetree::read(tree, &file.bytes(&bundle, object)?, &file.ref_types)
                else {
                    continue;
                };
                let path_id = |key: &str| fields.get(key).and_then(|p| p.get("m_PathID")).and_then(Value::as_i64).unwrap_or(0);
                if path_id("m_GameObject") != 0 && is_runner(path_id("m_Script")) {
                    found = Some(fields);
                }
            }
        }
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

type Found = (String, i64, Map<String, Value>);

fn read_bundle(path: &Path, scripts: &HashMap<i64, String>, wanted: &HashSet<&str>) -> Result<Vec<Found>, String> {
    let bundle = Bundle::open(path)?;
    let label = bundle.name.split("_assets_").next().unwrap_or("").to_string();
    let mut out = Vec::new();
    for entry in bundle.entries().iter().filter(|e| e.is_serialized_file()) {
        let file = SerializedFile::open(&bundle, entry)?;
        let class_of = |path_id: i64| scripts.get(&path_id).map_or(UNKNOWN_CLASS, String::as_str);
        for object in file.objects.iter().filter(|o| o.class_id == CLASS_MONO_BEHAVIOUR) {
            // The type table names the script, so objects of other classes are dropped before their bytes are
            // fetched; this is what keeps a three-class request from decompressing everything.
            if !wanted.is_empty() {
                match file.script(object) {
                    Some(script) if wanted.contains(class_of(script.path_id)) => {}
                    _ => continue,
                }
            }
            let Some(tree) = file.tree(object) else { continue };
            let Ok(Value::Object(mut fields)) = typetree::read(tree, &file.bytes(&bundle, object)?, &file.ref_types)
            else {
                continue; // an object UnityPy cannot read either is not in the reference dump
            };
            let path_id = |key: &str| fields.get(key).and_then(|p| p.get("m_PathID")).and_then(Value::as_i64).unwrap_or(0);
            if path_id("m_GameObject") != 0 {
                continue; // a component of a prefab or scene object, not a ScriptableObject
            }
            let class = class_of(path_id("m_Script"));
            if !wanted.is_empty() && !wanted.contains(class) {
                continue;
            }
            let name = fields.remove("m_Name").unwrap_or_else(|| Value::String(String::new()));
            fields.insert("_name".into(), name);
            fields.insert("_bundle".into(), Value::String(label.clone()));
            for key in ["m_GameObject", "m_Enabled", "m_Script"] {
                fields.remove(key);
            }
            out.push((class.to_string(), object.path_id, fields));
        }
    }
    Ok(out)
}
