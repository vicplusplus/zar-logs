//! The game's English text.
//!
//! Localisation lives in `localization_assets_*.bundle` as one `TextAsset` per language, listed in the bundle's
//! container as `Assets/Languages/<Language>/<language>.xml`. A TextAsset is just `m_Name` and `m_Script`, the
//! file's content as a string. The English one is an XML document of `<T id="KEY">text</T>` elements, which is how
//! a definition's name key becomes the name the player sees.

use std::path::Path;

use serde_json::Value;

use crate::serialized::CLASS_TEXT_ASSET;
use crate::so::bundles;
use crate::sprite::Assets;

const BUNDLE_PREFIX: &str = "localization_assets_";
const ENGLISH: &str = "assets/languages/english/english.xml";

/// The content of the English localisation file, exactly as stored in the game.
pub fn english(game_dir: &Path) -> Result<String, String> {
    let path = bundles(game_dir)?
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(BUNDLE_PREFIX)))
        .ok_or("localization bundle not found")?;
    let assets = Assets::open(&path)?;
    let listed = assets.container()?.into_iter().find(|(path, _)| path.to_lowercase() == ENGLISH).map(|(_, id)| id);
    // Should the file ever move, the asset's own name still identifies it.
    let by_name = || {
        assets.file.objects.iter().filter(|o| o.class_id == CLASS_TEXT_ASSET).map(|o| o.path_id).find(|&id| {
            assets.value(id).is_ok_and(|v| v["m_Name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case("english")))
        })
    };
    let path_id = listed.or_else(by_name).ok_or("English text asset not found in the localization bundle")?;
    match assets.value(path_id)?.get_mut("m_Script").map(Value::take) {
        Some(Value::String(text)) => Ok(text),
        _ => Err("English text asset has no text".into()),
    }
}
