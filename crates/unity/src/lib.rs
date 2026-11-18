//! Read-only access to the game's Unity content: icons, definitions and text, straight from the installed files.
//!
//! The game (Unity 6, Addressables) keeps its content in `zombieassault_Data/StreamingAssets/aa/`: a catalog and
//! about ninety asset bundles. Three things are taken from there, and nothing is ever written back:
//!
//! * [`pictures`]: the icon cache shown by the page (weapon renders, ability icons, map cards...);
//! * [`so`]: the ScriptableObject definitions (weapons, abilities, enemies...) as JSON;
//! * [`loc`]: the English localisation text.
//!
//! Underneath, from the outside in: [`bundle`] opens a UnityFS archive and decompresses the parts asked for,
//! [`serialized`] reads the object table and type descriptions of the SerializedFile inside, [`typetree`] turns one
//! object into JSON with those descriptions, [`catalog`] resolves Addressables keys to asset paths, [`sprite`] and
//! [`texture`] turn a Sprite object into pixels, and [`imaging`] crops and scales them.

pub mod bundle;
mod bytes;
pub mod catalog;
pub mod imaging;
pub mod loc;
pub mod pictures;
pub mod serialized;
pub mod so;
pub mod sprite;
pub mod texture;
pub mod typetree;
