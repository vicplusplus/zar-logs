//! The Addressables content catalog (`StreamingAssets/aa/catalog.bin`).
//!
//! Game data does not point at an icon by file name: it stores an Addressables *key*, usually the 32-hex-digit GUID
//! of the asset in the Unity project. The catalog maps every key (GUID, address or label) to *locations*; a
//! location's internal id is the asset's project path (`Assets/UI/Textures/medium/...png`), which is the name the
//! asset is listed under in its bundle's container.
//!
//! The file is Unity's `BinaryStorageBuffer`: little endian, and everything is found by following u32 offsets.
//!
//! ```text
//! header   u32 magic, u32 version, u32 keys, u32 ids, u32 instance provider, u32 scene provider,
//!          u32 init objects, u32 build result hash          (the last six are offsets; only `keys` is used here)
//! array    at an offset: the u32 before it is the array's size in bytes
//! keys     array of (u32 key object, u32 location list)
//! key      u32 type id, u32 object; for a string key the object is one u32 string id
//! list     array of u32 location offsets
//! location u32 primary key (string id), u32 internal id (string id), u32 provider, u32 dependencies,
//!          i32 dependency hash, u32 data, u32 resource type
//! string   id 0xFFFFFFFF = none. Bit 31 = UTF-16LE instead of ASCII. Bits 0..29 = offset of the bytes (again
//!          with their size in the u32 before). Bit 30 = "dynamic": the offset is instead a node (u32 string id of
//!          the last part, u32 id of the node holding what comes before it, or none); the parts are joined with '/'.
//!          Paths are stored that way so that their directories are shared.
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const UNICODE: u32 = 0x8000_0000;
const DYNAMIC: u32 = 0x4000_0000;
const MASK: u32 = 0x3FFF_FFFF;
const NONE: u32 = 0xFFFF_FFFF;

/// Where the catalog is, given the game's install folder.
pub fn path(game_dir: &Path) -> PathBuf {
    game_dir.join("zombieassault_Data").join("StreamingAssets").join("aa").join("catalog.bin")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub primary_key: Option<String>,
    /// For an asset: its project path.
    pub internal_id: Option<String>,
    /// Offset of the location record, which identifies it within the catalog.
    pub offset: u32,
}

pub struct Catalog {
    data: Vec<u8>,
    keys_offset: u32,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Catalog, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        if data.len() < 32 {
            return Err(format!("{}: too short for a catalog", path.display()));
        }
        let keys_offset = u32::from_le_bytes(data[8..12].try_into().unwrap());
        Ok(Catalog { data, keys_offset })
    }

    fn u32(&self, offset: u32) -> Option<u32> {
        let at = offset as usize;
        Some(u32::from_le_bytes(self.data.get(at..at + 4)?.try_into().ok()?))
    }

    /// The bytes of the array at `offset`; its length is stored just before it.
    fn raw(&self, offset: u32) -> Option<&[u8]> {
        let size = self.u32(offset.checked_sub(4)?)? as usize;
        let start = (offset as usize).min(self.data.len());
        Some(&self.data[start..start.saturating_add(size).min(self.data.len())])
    }

    fn u32_array(&self, offset: u32) -> Vec<u32> {
        if offset == NONE || offset as usize > self.data.len() {
            return Vec::new();
        }
        let raw = self.raw(offset).unwrap_or(&[]);
        raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
    }

    fn plain(&self, id: u32) -> Option<String> {
        if id == NONE {
            return None;
        }
        let raw = self.raw(id & MASK)?;
        Some(if id & UNICODE != 0 {
            let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        } else {
            raw.iter().map(|&b| if b < 0x80 { b as char } else { char::REPLACEMENT_CHARACTER }).collect()
        })
    }

    fn string(&self, id: u32) -> Option<String> {
        if id == NONE {
            return None;
        }
        if id & DYNAMIC == 0 {
            return self.plain(id);
        }
        let mut parts = Vec::new();
        let mut offset = id & MASK;
        // The chain is as long as the path is deep; the bound only stops a corrupt file from looping.
        for _ in 0..4096 {
            parts.push(self.plain(self.u32(offset)?)?);
            let next = self.u32(offset + 4)?;
            if next == NONE {
                break;
            }
            offset = next & MASK;
        }
        parts.reverse();
        Some(parts.join("/"))
    }

    fn key(&self, key_offset: u32) -> Option<String> {
        let object = self.u32(key_offset.checked_add(4)?)?;
        self.string(self.u32(object)?)
    }

    fn locations(&self, list_offset: u32) -> Vec<Location> {
        self.u32_array(list_offset)
            .into_iter()
            .filter_map(|offset| {
                self.data.get(offset as usize..offset as usize + 28)?;
                Some(Location {
                    primary_key: self.string(self.u32(offset)?),
                    internal_id: self.string(self.u32(offset + 4)?),
                    offset,
                })
            })
            .collect()
    }

    /// Every key with its locations in catalog order. A key that appears twice keeps its last list.
    pub fn table(&self) -> HashMap<String, Vec<Location>> {
        let keys = self.u32_array(self.keys_offset);
        let mut table = HashMap::with_capacity(keys.len() / 2);
        for pair in keys.chunks_exact(2) {
            if let Some(key) = self.key(pair[0]) {
                table.insert(key, self.locations(pair[1]));
            }
        }
        table
    }

    /// The locations of one key, without building the whole table.
    pub fn lookup(&self, key: &str) -> Vec<Location> {
        let keys = self.u32_array(self.keys_offset);
        let found = keys.chunks_exact(2).rev().find(|pair| self.key(pair[0]).as_deref() == Some(key));
        found.map(|pair| self.locations(pair[1])).unwrap_or_default()
    }
}
