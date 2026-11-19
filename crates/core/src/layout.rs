//! Memory layout of the structures the meter reads.
//!
//! The layout is derived from the game's own metadata (GameAssembly.dll + global-metadata.dat) and cached per game
//! build as `layout_<dll size>_<dll mtime>.json`. The `zar-logs-layoutgen` crate produces that file; this module
//! loads it. Offsets of value types are "in place" (the 0x10 managed
//! object header already subtracted); offsets of classes are from the object pointer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

/// The layout version this code was written against (`VERSION` in zarmeter/layout.py).
pub const VERSION: i64 = 12;

/// Field offsets of one struct.
#[derive(Clone, Debug)]
pub struct St {
    pub name: String,
    pub size: Option<usize>,
    fields: HashMap<String, i64>,
}

impl St {
    /// Offset of a field. Panics with the struct and field name when this game build has no such field: a layout
    /// that does not match the code must stop the meter, not make it read the wrong bytes.
    pub fn o(&self, field: &str) -> usize {
        match self.fields.get(field) {
            Some(&v) if v >= 0 => v as usize,
            _ => panic!("layout: struct {} has no field {field:?} in this game build", self.name),
        }
    }
    pub fn has(&self, field: &str) -> bool { self.fields.contains_key(field) }
    pub fn size(&self) -> usize {
        self.size.unwrap_or_else(|| panic!("layout: struct {} has no known size", self.name))
    }
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub typeinfo_rva: u64,
    pub static_fields: u64,
    pub instance_off: u64,
    pub event_extra: HashMap<u8, usize>,
    structs: HashMap<String, St>,
    /// what the code needs to know about managed objects and native containers (see `rt`)
    runtime: Value,
    /// enum name -> (member, value) in declaration order
    enums: HashMap<String, Vec<(String, i64)>>,
}

impl Layout {
    pub fn load(path: &Path) -> Result<Layout, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let ver = v["version"].as_i64().unwrap_or(0);
        if ver != VERSION {
            return Err(format!("{}: layout version {ver}, this build needs {VERSION}", path.display()));
        }
        let mut structs = HashMap::new();
        for (name, d) in v["structs"].as_object().ok_or("layout: no structs")? {
            let mut fields = HashMap::new();
            for (k, f) in d["fields"].as_object().into_iter().flatten() {
                // same normalisation as the Python `S` helper: "<Instance>k__BackingField" -> "Instance"
                let key = k.trim_matches(|c| c == '<' || c == '>').replace(">k__BackingField", "");
                fields.insert(key, f[0].as_i64().unwrap_or(-1));
            }
            structs.insert(name.clone(), St { name: name.clone(), size: d["size"].as_u64().map(|x| x as usize), fields });
        }
        let mut enums = HashMap::new();
        for (name, d) in v["enums"].as_object().into_iter().flatten() {
            enums.insert(name.clone(), d.as_object().into_iter().flatten().filter_map(|(k, x)| Some((k.clone(), x.as_i64()?))).collect());
        }
        let event_extra = v["event_extra"].as_object().into_iter().flatten()
            .filter_map(|(k, x)| Some((k.parse::<u8>().ok()?, x.as_u64()? as usize))).collect();
        Ok(Layout {
            typeinfo_rva: v["gamerunner_typeinfo_rva"].as_u64().ok_or("layout: no TypeInfo slot")?,
            static_fields: v["class_static_fields"].as_u64().ok_or("layout: no static_fields offset")?,
            instance_off: v["gamerunner_statics"]["<Instance>k__BackingField"].as_u64().ok_or("layout: no GameRunner.Instance")?,
            event_extra, structs, enums, runtime: v["runtime"].clone(),
        })
    }

    /// The cached layout for the game build installed at `game_dir`; when that exact build has none (or the game
    /// folder is unknown), the newest cached layout.
    pub fn find(cache_dir: &Path, game_dir: Option<&Path>) -> Result<(Layout, PathBuf), String> {
        let mut pick: Option<PathBuf> = None;
        if let Some(g) = game_dir {
            if let Ok(md) = std::fs::metadata(g.join("GameAssembly.dll")) {
                let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
                let exact = cache_dir.join(format!("layout_{}_{}.json", md.len(), mtime));
                if exact.is_file() {
                    pick = Some(exact);
                }
            }
        }
        if pick.is_none() {
            let mut all: Vec<_> = std::fs::read_dir(cache_dir).map_err(|e| format!("{}: {e}", cache_dir.display()))?
                .flatten().map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("layout_") && n.ends_with(".json")))
                .collect();
            all.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
            pick = all.pop();
        }
        let path = pick.ok_or_else(|| format!("no layout in {}", cache_dir.display()))?;
        Ok((Layout::load(&path)?, path))
    }

    /// A fact about the runtime's own object layout, e.g. `rt("string", "chars")`: where a managed string's
    /// characters start. Generated with the rest of the layout, so it is tied to the game build like any offset.
    pub fn rt(&self, group: &str, name: &str) -> usize {
        self.runtime[group][name].as_u64().unwrap_or_else(|| panic!("layout: no runtime.{group}.{name} in this game build")) as usize
    }

    pub fn st(&self, name: &str) -> &St {
        self.structs.get(name).unwrap_or_else(|| panic!("layout: no struct {name} in this game build"))
    }
    pub fn try_st(&self, name: &str) -> Option<&St> { self.structs.get(name) }
    pub fn en(&self, name: &str, member: &str) -> i64 {
        self.members(name).iter().find(|(m, _)| m == member).map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("layout: no enum member {name}.{member} in this game build"))
    }
    /// (member, value) of an enum in declaration order; empty when this game build has no such enum.
    pub fn members(&self, name: &str) -> &[(String, i64)] {
        self.enums.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}
