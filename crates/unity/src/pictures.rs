//! The icon cache: in-game pictures extracted from the user's own install.
//!
//! Nothing here ships with the app. Weapon and armour renders, mod, ability and skill icons, enemy portraits and the
//! card pictures of the maps are the property of Ninja Kiwi; they are read on this machine from the installed game
//! and written to a cache folder that only the local page is served from.
//!
//! How a definition leads to a picture: it carries an Addressables GUID for its icon (see [`crate::so`] for the
//! definitions), the catalog turns the GUID into an asset path ([`crate::catalog`]), and the
//! `ui_textures_assets_res_medium_*.bundle` lists a Sprite under that path ([`crate::sprite`]).
//!
//! The cache is a flat folder of images plus `index.json`:
//!
//! ```text
//! {"stamp": {"bundle": file name, "size": bytes, "mtime": seconds, "px": 192, "v": 4},
//!  "icons": {key: file name}}
//! ```
//!
//! with keys: an icon GUID; `ui:<name>` for the fixed interface sprites; `level:<descriptor name, lower case>` and
//! `scene:<scene path, lower case>` for a map's card picture; `enemy:<name>` for the portraits. The stamp describes
//! the bundle the cache was made from; while it matches, [`ensure`] does nothing.
//!
//! Icons are stored as PNG, cropped to their non-transparent box and scaled down to at most 192 px; card pictures
//! are photo-like and stored as JPEG, at most 560 px. File names, keys and pixels follow the Python tool this
//! replaces (`zarmeter/assets.py`), whose caches are therefore still valid.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Map, Value};

use crate::catalog::{self, Catalog, Location};
use crate::imaging::{thumbnail_size, Image};
use crate::so::bundles;
use crate::sprite::Assets;

/// Longest side of a stored icon, in pixels.
pub const SIZE: usize = 192;
/// Longest side of a stored map picture.
pub const CARD: usize = 560;
/// Bump when this module changes what it extracts. Kept equal to the Python tool's `STAMP_V`.
const STAMP_VERSION: u32 = 4;
const JPEG_QUALITY: u8 = 84;
const PREFIX: &str = "Assets/UI/Textures/medium/";
const ENEMY_PREFIX: &str = "Assets/UI/Textures/medium/Hud/enemy_icon_";
const BUNDLE_PREFIX: &str = "ui_textures_assets_res_medium_";

/// Sprites no definition refers to: the mod slot glyphs, the mod-power symbol and the boss markers.
const UI_SPRITES: [(&str, &str); 9] = [
    ("ui:Optic", "mod_icons/mod_slot_icon_optic.png"),
    ("ui:Barrel", "mod_icons/mod_slot_icon_barrel.png"),
    ("ui:Grip", "mod_icons/mod_slot_icon_grip.png"),
    ("ui:Core", "mod_icons/mod_slot_icon_core.png"),
    ("ui:Clip", "mod_icons/mod_slot_icon_clip.png"),
    ("ui:Armor", "mod_icons/mod_icon_armor_physical_common.png"),
    ("ui:power", "mod_icons/mod_power_icon.png"),
    ("ui:bosspod", "markers/boss_pod_icon.png"),
    ("ui:boss", "markers/boss_marker_icon.png"),
];

/// The ScriptableObject classes [`ensure`] reads icon references from.
pub const CLASSES: [&str; 6] = [
    "WeaponDefinitionSO",
    "ArmourPieceSO",
    "GearModEnhancementDefinitionSO",
    "PassiveSkillDefinitionSO",
    "AbilityDefinitionSO",
    "LevelDescriptorSO",
];

type Definitions = HashMap<String, Map<String, Value>>;

/// The GUID inside an asset reference field (`{"m_AssetGUID": ...}`), or "" when the field is absent or empty.
fn guid(reference: Option<&Value>) -> &str {
    reference.and_then(|r| r.get("m_AssetGUID")).and_then(Value::as_str).unwrap_or("")
}

fn rows<'a>(so: &'a Definitions, class: &str) -> impl Iterator<Item = &'a Value> {
    so.get(class).into_iter().flat_map(|objects| objects.values())
}

/// Every icon GUID referenced by a definition the build viewer can show, without repeats.
pub fn wanted(so: &Definitions) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |g: &str| {
        if !g.is_empty() && seen.insert(g.to_string()) {
            out.push(g.to_string());
        }
    };
    for d in rows(so, "WeaponDefinitionSO") {
        let menu = guid(d.get("menuIcon"));
        add(if menu.is_empty() { guid(d.get("ingameIcon")) } else { menu });
    }
    for d in rows(so, "ArmourPieceSO") {
        add(guid(d.get("menuIcon")));
    }
    for d in rows(so, "GearModEnhancementDefinitionSO") {
        add(guid(d.get("icon")));
    }
    for d in rows(so, "PassiveSkillDefinitionSO") {
        add(guid(d.get("menuIconRef")));
    }
    for d in rows(so, "AbilityDefinitionSO") {
        add(guid(d.get("ingameIcon")));
        for list in ["abilityUpgrades", "abilityEnhancements"] {
            for x in d.get(list).and_then(Value::as_array).into_iter().flatten() {
                add(guid(x.get("icon")));
            }
        }
    }
    out
}

/// `("level:<LevelDescriptor name, lower case>", GUID of the picture on the map's mission card)` and
/// `("scene:<scene GUID>", the same GUID)`: the scene is how a mission names its map, and its file name need not
/// resemble the descriptor's (LevelDescriptor_Void is Op_ForgottenOutpost.unity).
///
/// Should two descriptors share a name or a scene, the one later in the map's order wins. The Python tool took the
/// later one in bundle order instead; the game has no such pair.
pub fn levels(so: &Definitions) -> Vec<(String, String)> {
    let mut out = Ordered::default();
    for d in rows(so, "LevelDescriptorSO") {
        let mut card = guid(d.get("cardImage"));
        if card.is_empty() {
            let modes = d.get("supportedModes").and_then(Value::as_array).into_iter().flatten();
            card = modes.map(|m| guid(m.get("thumbnail"))).find(|g| !g.is_empty()).unwrap_or("");
        }
        let name = d.get("_name").and_then(Value::as_str).unwrap_or("");
        if card.is_empty() || name.is_empty() {
            continue;
        }
        out.set(format!("level:{}", name.to_lowercase()), card.to_string());
        let scene = guid(d.get("scene"));
        if !scene.is_empty() {
            out.set(format!("scene:{}", scene), card.to_string());
        }
    }
    out.items
}

/// Key/value pairs with a Python dictionary's behaviour: setting a key again replaces its value in place.
#[derive(Default)]
struct Ordered {
    items: Vec<(String, String)>,
    index: HashMap<String, usize>,
}

impl Ordered {
    fn set(&mut self, key: String, value: String) {
        match self.index.get(&key) {
            Some(&i) => self.items[i].1 = value,
            None => {
                self.index.insert(key.clone(), self.items.len());
                self.items.push((key, value));
            }
        }
    }
}

/// Extracts the pictures into `icons_dir` unless they are there and the game's texture bundle has not changed
/// since (`force` extracts regardless). `so` holds the definitions of [`CLASSES`], as [`crate::so::dump`] returns
/// them. Returns the number of entries in the index.
pub fn ensure(game_dir: &Path, icons_dir: &Path, so: &Definitions, force: bool) -> Result<usize, String> {
    let bundle_path = bundles(game_dir)?
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(BUNDLE_PREFIX)))
        .ok_or("texture bundle not found")?;
    let meta = std::fs::metadata(&bundle_path).map_err(|e| format!("{}: {}", bundle_path.display(), e))?;
    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    let stamp = json!({
        "bundle": bundle_path.file_name().map(|n| n.to_string_lossy()),
        "size": meta.len(),
        "mtime": modified.map_or(0, |d| d.as_secs()),
        "px": SIZE,
        "v": STAMP_VERSION,
    });
    let index_path = icons_dir.join("index.json");
    if !force {
        let old = std::fs::read(&index_path).ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
        if let Some(old) = old.filter(|old| old.get("stamp") == Some(&stamp)) {
            return Ok(old.get("icons").and_then(Value::as_object).map_or(0, Map::len));
        }
    }

    let table = Catalog::open(&catalog::path(game_dir))?.table();
    let asset_path = |key: &str| -> Option<&str> {
        table.get(key).and_then(|l| l.first()).and_then(|l: &Location| l.internal_id.as_deref()).filter(|p| !p.is_empty())
    };
    let mut paths = Ordered::default();
    for g in wanted(so) {
        if let Some(path) = asset_path(&g) {
            paths.set(g, path.to_string());
        }
    }
    for (key, rel) in UI_SPRITES {
        paths.set(key.to_string(), format!("{}{}", PREFIX, rel));
    }
    let mut cards = HashSet::new();
    for (mut key, g) in levels(so) {
        let Some(path) = asset_path(&g) else { continue };
        if let Some(scene) = key.strip_prefix("scene:") {
            // Indexed by the scene's path, which is what a mission reports.
            let Some(scene_path) = asset_path(scene) else { continue };
            key = format!("scene:{}", scene_path.to_lowercase());
        }
        paths.set(key, path.to_string());
        cards.insert(path.to_string());
    }

    let assets = Assets::open(&bundle_path)?;
    let listed = assets.container()?;
    for (path, _) in &listed {
        // Enemy portraits of the codex and health bars: one per enemy kind.
        if let Some(rest) = path.strip_prefix(ENEMY_PREFIX) {
            paths.set(format!("enemy:{}", strip_extension(rest).to_lowercase()), path.clone());
        }
    }
    let container: HashMap<&str, i64> = listed.iter().map(|(path, id)| (path.as_str(), *id)).collect();

    // Each distinct asset is rendered once, in parallel; files are written afterwards in order, so that two assets
    // that map to the same file name end the same way on every run.
    let mut jobs: Vec<(&str, i64, String, bool)> = Vec::new();
    let mut job_of: HashMap<&str, usize> = HashMap::new();
    for (_, path) in &paths.items {
        let Some(&path_id) = container.get(path.as_str()) else { continue };
        if job_of.contains_key(path.as_str()) {
            continue;
        }
        let card = cards.contains(path);
        let stem = match path.strip_prefix(PREFIX) {
            Some(rest) => strip_extension(rest).replace('/', "__"),
            None => strip_extension(path.rsplit('/').next().unwrap_or(path)).to_string(),
        };
        job_of.insert(path, jobs.len());
        jobs.push((path, path_id, format!("{}.{}", stem, if card { "jpg" } else { "png" }), card));
    }
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(jobs.len().max(1));
    let mut rendered: Vec<Option<Vec<u8>>> = vec![None; jobs.len()];
    let done: Vec<(usize, Vec<u8>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break };
                        // A picture that cannot be read is left out of the index; the page shows a glyph instead.
                        if let Ok(file) = render(&assets, job.1, job.3) {
                            out.push((i, file));
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    for (i, file) in done {
        rendered[i] = Some(file);
    }

    std::fs::create_dir_all(icons_dir).map_err(|e| format!("{}: {}", icons_dir.display(), e))?;
    for (job, file) in jobs.iter().zip(&rendered) {
        if let Some(file) = file {
            let target = icons_dir.join(&job.2);
            std::fs::write(&target, file).map_err(|e| format!("{}: {}", target.display(), e))?;
        }
    }
    let mut icons = Map::new();
    for (key, path) in &paths.items {
        if let Some(&i) = job_of.get(path.as_str()) {
            if rendered[i].is_some() {
                icons.insert(key.clone(), Value::String(jobs[i].2.clone()));
            }
        }
    }
    let count = icons.len();
    // Written by hand to keep "stamp" first, as in the caches made by the Python tool.
    let index = format!("{{\"stamp\": {}, \"icons\": {}}}", stamp, Value::Object(icons));
    std::fs::write(&index_path, index).map_err(|e| format!("{}: {}", index_path.display(), e))?;
    Ok(count)
}

fn strip_extension(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// One asset as the bytes of its cache file.
fn render(assets: &Assets, path_id: i64, card: bool) -> Result<Vec<u8>, String> {
    let mut image = assets.image(path_id)?;
    if image.width == 0 || image.height == 0 {
        return Err("empty picture".into());
    }
    if card {
        // A photo-like picture: kept whole, stored small.
        image.set_opaque();
        if let Some((width, height)) = thumbnail_size(image.width, image.height, CARD) {
            image = image.resize(width, height);
        }
        jpeg(&image)
    } else {
        // Transparent margins are dropped so that the icon fills its tile.
        if let Some((x0, y0, x1, y1)) = image.bbox() {
            image = image.crop(x0 as i64, y0 as i64, x1 as i64, y1 as i64);
        }
        if let Some((width, height)) = thumbnail_size(image.width, image.height, SIZE) {
            image = image.resize_with_alpha(width, height);
        }
        png(&image)
    }
}

pub fn png(image: &Image) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&image.data).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Baseline JPEG, 4:2:0. The crate does the DCT and entropy coding as libjpeg does, but it converts colours with a
/// slightly different rounding and subsamples chroma by taking one pixel of every four, which aliases. Both steps are
/// therefore done here the way libjpeg does them (the same fixed-point matrix, each 2x2 block averaged with its
/// alternating rounding bias, edges replicated), and the encoder is handed YCbCr whose chroma it can pick blindly.
fn jpeg(image: &Image) -> Result<Vec<u8>, String> {
    let (width, height) = (image.width, image.height);
    let (Ok(w), Ok(h)) = (u16::try_from(width), u16::try_from(height)) else {
        return Err("picture too large for JPEG".into());
    };
    let mut ycc = vec![0u8; width * height * 3];
    for (out, rgb) in ycc.chunks_exact_mut(3).zip(image.data.chunks_exact(4)) {
        let (r, g, b) = (rgb[0] as i32, rgb[1] as i32, rgb[2] as i32);
        out[0] = ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u8;
        out[1] = ((-11059 * r - 21709 * g + 32768 * b + (128 << 16) + 32767) >> 16) as u8;
        out[2] = ((32768 * r - 27439 * g - 5329 * b + (128 << 16) + 32767) >> 16) as u8;
    }
    for y in (0..height).step_by(2) {
        let below = (y + 1).min(height - 1);
        for x in (0..width).step_by(2) {
            let right = (x + 1).min(width - 1);
            let bias = 1 + ((x as u32 / 2) & 1);
            for c in 1..3 {
                let at = |x: usize, y: usize| ycc[(y * width + x) * 3 + c] as u32;
                let mean = ((at(x, y) + at(right, y) + at(x, below) + at(right, below) + bias) >> 2) as u8;
                for (x, y) in [(x, y), (right, y), (x, below), (right, below)] {
                    ycc[(y * width + x) * 3 + c] = mean;
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
    encoder.encode(&ycc, w, h, jpeg_encoder::ColorType::Ycbcr).map_err(|e| e.to_string())?;
    Ok(out)
}
