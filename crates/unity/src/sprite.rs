//! From an asset path to a picture: the bundle's container, Texture2D and Sprite objects.
//!
//! * The `AssetBundle` object (one per bundle) has `m_Container`, a list of `[asset path, {asset: PPtr}]`. An image
//!   imported as a sprite is listed twice under its path, once for the Texture2D and once for the Sprite; like the
//!   Python tool's dictionary, [`Assets::container`] keeps the last entry, the Sprite.
//! * A `Sprite` is a rectangle of a texture. Its `m_RD` (render data) names the `texture`, the `textureRect` in
//!   pixels with the origin at the bottom left, and `settingsRaw`: bit 0 = packed into an atlas, bit 1 = packing
//!   mode (0 tight, 1 rectangle), bits 2..5 = rotation applied when packing, bit 6 = mesh type (0 full rectangle,
//!   1 tight). A sprite packed at build time has no texture of its own: `m_SpriteAtlas` points to a `SpriteAtlas`
//!   whose `m_RenderDataMap` holds, under the sprite's `m_RenderDataKey`, the same fields for the atlas texture.
//! * A sprite also carries a small mesh (`m_RD.m_VertexData`, `m_IndexBuffer`, `m_SubMeshes`), in units of
//!   1 / `m_PixelsToUnits` pixels: a quad for the full rectangle, or an outline hugging the opaque pixels. Unity
//!   draws only that mesh, so what lies outside it is made transparent. Which pixels a triangle covers follows
//!   Pillow's polygon filler, as that is what the Python tool used.
//!
//! The picture returned is what UnityPy's `.image` gives: top row first, sprite cut out of its texture or atlas.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::bundle::Bundle;
use crate::imaging::Image;
use crate::serialized::{
    ObjectInfo, SerializedFile, CLASS_ASSET_BUNDLE, CLASS_SPRITE, CLASS_SPRITE_ATLAS, CLASS_TEXTURE2D,
};
use crate::{texture, typetree};

/// A bundle and its (first) SerializedFile, with the object lookups pictures need.
pub struct Assets {
    pub bundle: Bundle,
    pub file: SerializedFile,
    /// Atlas textures are shared by many sprites and decoded once; a sprite's own texture is used once and is not
    /// kept, or the cache would hold hundreds of megabytes.
    atlas_textures: Mutex<HashMap<i64, Arc<(Image, bool)>>>,
}

impl Assets {
    pub fn open(path: &Path) -> Result<Assets, String> {
        let bundle = Bundle::open(path)?;
        let entry = bundle
            .entries()
            .iter()
            .find(|e| e.is_serialized_file())
            .ok_or_else(|| format!("{}: no SerializedFile inside", bundle.name))?;
        let file = SerializedFile::open(&bundle, entry)?;
        Ok(Assets { bundle, file, atlas_textures: Mutex::new(HashMap::new()) })
    }

    fn info(&self, path_id: i64) -> Result<&ObjectInfo, String> {
        self.file.object(path_id).ok_or_else(|| format!("{}: no object {}", self.bundle.name, path_id))
    }

    /// An object as JSON (see [`crate::typetree`]).
    pub fn value(&self, path_id: i64) -> Result<Value, String> {
        let object = self.info(path_id)?;
        let tree = self.file.tree(object).ok_or("object type without a type tree")?;
        typetree::read(tree, &self.file.bytes(&self.bundle, object)?, &self.file.ref_types)
    }

    /// `(asset path, path id)` in the bundle's order. A path listed more than once keeps its place and takes the
    /// object listed last, like a Python dictionary built from the list.
    pub fn container(&self) -> Result<Vec<(String, i64)>, String> {
        let bundle = self.file.objects.iter().find(|o| o.class_id == CLASS_ASSET_BUNDLE);
        let bundle = self.value(bundle.ok_or("no AssetBundle object")?.path_id)?;
        let mut out: Vec<(String, i64)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for entry in bundle["m_Container"].as_array().into_iter().flatten() {
            let asset = &entry[1]["asset"];
            let (Some(path), Some(0), Some(path_id)) =
                (entry[0].as_str(), asset["m_FileID"].as_i64(), asset["m_PathID"].as_i64())
            else {
                continue;
            };
            match index.get(path) {
                Some(&i) => out[i].1 = path_id,
                None => {
                    index.insert(path.to_string(), out.len());
                    out.push((path.to_string(), path_id));
                }
            }
        }
        Ok(out)
    }

    /// The picture of a Sprite or Texture2D object, top row first.
    pub fn image(&self, path_id: i64) -> Result<Image, String> {
        match self.info(path_id)?.class_id {
            CLASS_SPRITE => self.sprite(path_id),
            CLASS_TEXTURE2D => {
                let (mut image, _) = self.texture(path_id)?;
                image.flip_vertical();
                Ok(image)
            }
            other => Err(format!("object {} is of class {}, not a picture", path_id, other)),
        }
    }

    /// A Texture2D's first mip level, rows in stored (bottom-up) order, and whether its format lacks alpha.
    pub fn texture(&self, path_id: i64) -> Result<(Image, bool), String> {
        let object = self.info(path_id)?;
        if object.class_id != CLASS_TEXTURE2D {
            return Err(format!("object {} is not a Texture2D", path_id));
        }
        let tree = self.file.tree(object).ok_or("Texture2D without a type tree")?;
        let bytes = self.file.bytes(&self.bundle, object)?;
        let (fields, blobs) = typetree::read_with_blobs(tree, &bytes, &self.file.ref_types)?;
        let number = |key: &str| fields[key].as_i64().unwrap_or(0);
        let (width, height, format) = (number("m_Width") as usize, number("m_Height") as usize, number("m_TextureFormat"));
        let needed = texture::data_len(format, width, height)
            .ok_or_else(|| format!("texture format {} is not supported", format))?;

        let stream = &fields["m_StreamData"];
        let resource = stream["path"].as_str().unwrap_or("");
        if resource.is_empty() {
            let inline = blobs.iter().find(|(name, _)| name == "image data").ok_or("texture without pixels")?;
            let image = texture::decode(format, width, height, &bytes[inline.1.clone()])?;
            return Ok((image, texture::is_opaque(format)));
        }
        let entry = self.bundle.entry(resource).ok_or_else(|| format!("resource file {} not in the bundle", resource))?;
        let (offset, size) = (stream["offset"].as_u64().unwrap_or(0), stream["size"].as_u64().unwrap_or(0));
        // Only the first mip level is fetched; the smaller ones that follow would be decompressed for nothing.
        let len = (needed as u64).min(size);
        if offset + len > entry.size {
            return Err("texture data lies outside its resource file".into());
        }
        let pixels = self.bundle.read(entry.offset + offset, len as usize)?;
        Ok((texture::decode(format, width, height, &pixels)?, texture::is_opaque(format)))
    }

    fn atlas_texture(&self, path_id: i64) -> Result<Arc<(Image, bool)>, String> {
        if let Some(image) = self.atlas_textures.lock().unwrap().get(&path_id) {
            return Ok(image.clone());
        }
        let image = Arc::new(self.texture(path_id)?);
        self.atlas_textures.lock().unwrap().insert(path_id, image.clone());
        Ok(image)
    }

    fn sprite(&self, path_id: i64) -> Result<Image, String> {
        let object = self.info(path_id)?;
        let tree = self.file.tree(object).ok_or("Sprite without a type tree")?;
        let bytes = self.file.bytes(&self.bundle, object)?;
        let (sprite, blobs) = typetree::read_with_blobs(tree, &bytes, &self.file.ref_types)?;

        let own = &sprite["m_RD"];
        let atlas = self.atlas_data(&sprite)?;
        let data = atlas.as_ref().unwrap_or(own);
        let texture_id = data["texture"]["m_PathID"].as_i64().unwrap_or(0);
        if data["alphaTexture"]["m_PathID"].as_i64().unwrap_or(0) != 0 {
            return Err("sprite with a separate alpha texture is not supported".into());
        }
        let texture: Arc<(Image, bool)> = if atlas.is_some() {
            self.atlas_texture(texture_id)?
        } else {
            Arc::new(self.texture(texture_id)?)
        };

        // Pillow rounds a fractional crop box half to even; so does this.
        let edge = |key: &str| data["textureRect"][key].as_f64().unwrap_or(0.0);
        let (x, y, w, h) = (edge("x"), edge("y"), edge("width"), edge("height"));
        let round = |v: f64| v.round_ties_even() as i64;
        let (texture, opaque) = (&texture.0, texture.1);
        let mut image = texture.crop(round(x), round(y), round(x + w), round(y + h));

        let settings = data["settingsRaw"].as_u64().unwrap_or(0);
        if settings & 1 != 0 {
            match settings >> 2 & 0xf {
                1 => image.flip_horizontal(),
                2 => image.flip_vertical(),
                3 => {
                    image.flip_horizontal();
                    image.flip_vertical();
                }
                4 => image = image.rotate_clockwise(),
                _ => {}
            }
        }
        if settings >> 1 & 1 == 0 {
            let vertex_data = blobs.iter().find(|(name, _)| name == "m_DataSize").map(|(_, range)| &bytes[range.clone()]);
            let mesh = Mesh::read(own, vertex_data.unwrap_or(&[]))?;
            if mesh.has_uvs {
                return Err("sprite drawn through a UV-mapped mesh is not supported".into());
            }
            let scale = sprite["m_PixelsToUnits"].as_f64().unwrap_or(1.0);
            mesh.mask(&mut image, scale, opaque);
        }
        image.flip_vertical();
        Ok(image)
    }

    /// The render data of a sprite that was packed into an atlas, if it was.
    fn atlas_data(&self, sprite: &Value) -> Result<Option<Value>, String> {
        let mut atlas_id = sprite["m_SpriteAtlas"]["m_PathID"].as_i64().unwrap_or(0);
        if atlas_id == 0 {
            // Some builds leave the pointer empty and only name the atlas.
            let Some(tag) = sprite["m_AtlasTags"].get(0).and_then(Value::as_str) else { return Ok(None) };
            for object in self.file.objects.iter().filter(|o| o.class_id == CLASS_SPRITE_ATLAS) {
                if self.value(object.path_id)?["m_Name"].as_str() == Some(tag) {
                    atlas_id = object.path_id;
                }
            }
            if atlas_id == 0 {
                return Ok(None);
            }
        }
        let atlas = self.value(atlas_id)?;
        let key = &sprite["m_RenderDataKey"];
        let found = atlas["m_RenderDataMap"].as_array().into_iter().flatten().find(|entry| &entry[0] == key);
        Ok(Some(found.ok_or("sprite is missing from its atlas")?[1].clone()))
    }
}

/// The sprite's mesh: positions of stream 0 and the triangles of every submesh.
struct Mesh {
    vertices: Vec<[f32; 2]>,
    triangles: Vec<[usize; 3]>,
    has_uvs: bool,
}

impl Mesh {
    fn read(render_data: &Value, vertex_bytes: &[u8]) -> Result<Mesh, String> {
        let vertex_data = &render_data["m_VertexData"];
        let count = vertex_data["m_VertexCount"].as_u64().unwrap_or(0) as usize;
        let channels: Vec<[usize; 4]> = vertex_data["m_Channels"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| ["stream", "offset", "format", "dimension"].map(|k| c[k].as_u64().unwrap_or(0) as usize))
            .collect();
        // Bytes per component of Unity's VertexFormat: float, half, 8-bit x4, 16-bit x4, 32-bit x2.
        const SIZES: [usize; 12] = [4, 2, 1, 1, 2, 2, 1, 1, 2, 2, 4, 4];
        let size = |c: &[usize; 4]| (c[3] & 0xf) * SIZES.get(c[2]).copied().unwrap_or(4);
        // The streams are not stored: each is the channels assigned to it, interleaved per vertex, and starts at a
        // multiple of 16 after the previous one.
        let stream_count = channels.iter().map(|c| c[0] + 1).max().unwrap_or(1);
        let mut starts = Vec::with_capacity(stream_count);
        let mut strides = Vec::with_capacity(stream_count);
        let mut offset = 0usize;
        for s in 0..stream_count {
            let stride: usize = channels.iter().filter(|c| c[0] == s).map(size).sum();
            starts.push(offset);
            strides.push(stride);
            offset = (offset + count * stride + 15) & !15;
        }
        let floats = |channel: usize, n: usize| -> Option<Vec<Vec<f32>>> {
            let c = channels.get(channel)?;
            if c[3] & 0xf < n || c[2] != 0 {
                return None;
            }
            (0..count)
                .map(|v| {
                    let at = starts[c[0]] + v * strides[c[0]] + c[1];
                    let raw = vertex_bytes.get(at..at + n * 4)?;
                    Some(raw.chunks_exact(4).map(|f| f32::from_le_bytes(f.try_into().unwrap())).collect())
                })
                .collect()
        };
        let vertices: Vec<[f32; 2]> = floats(0, 2).unwrap_or_default().iter().map(|v| [v[0], v[1]]).collect();
        let has_uvs = floats(4, 2).is_some_and(|uvs| uvs.iter().any(|uv| uv[0] != 0.0 || uv[1] != 0.0));

        let indices: Vec<usize> = render_data["m_IndexBuffer"]
            .as_array()
            .map(|bytes| bytes.iter().map(|b| b.as_u64().unwrap_or(0) as u8).collect::<Vec<u8>>())
            .unwrap_or_default()
            .chunks_exact(2)
            .map(|i| u16::from_le_bytes([i[0], i[1]]) as usize)
            .collect();
        let mut triangles = Vec::new();
        for submesh in render_data["m_SubMeshes"].as_array().into_iter().flatten() {
            if submesh["topology"].as_u64().unwrap_or(0) != 0 {
                return Err("sprite mesh that is not a triangle list".into());
            }
            let first = submesh["firstByte"].as_u64().unwrap_or(0) as usize / 2;
            let n = submesh["indexCount"].as_u64().unwrap_or(0) as usize;
            for triangle in indices.get(first..first + n).unwrap_or(&[]).chunks_exact(3) {
                if triangle.iter().all(|&i| i < vertices.len()) {
                    triangles.push([triangle[0], triangle[1], triangle[2]]);
                }
            }
        }
        Ok(Mesh { vertices, triangles, has_uvs })
    }

    /// Clears the pixels of `image` (still bottom-up, as cut from the texture) that no triangle covers. With
    /// `keep_color` only their alpha is cleared, which is what Pillow's `putalpha` does to an image without alpha.
    fn mask(&self, image: &mut Image, pixels_per_unit: f64, keep_color: bool) {
        if self.vertices.is_empty() {
            return;
        }
        let min_x = self.vertices.iter().map(|v| v[0]).fold(f32::INFINITY, f32::min) as f64;
        let min_y = self.vertices.iter().map(|v| v[1]).fold(f32::INFINITY, f32::min) as f64;
        // Pillow's polygon takes integer coordinates and truncates what it is given.
        let points: Vec<[i32; 2]> = self
            .vertices
            .iter()
            .map(|v| [((v[0] as f64 - min_x) * pixels_per_unit) as i32, ((v[1] as f64 - min_y) * pixels_per_unit) as i32])
            .collect();
        let (width, height) = (image.width, image.height);
        let mut covered = vec![false; width * height];
        for triangle in &self.triangles {
            fill_polygon(&mut covered, width, height, &triangle.map(|i| points[i]));
        }
        for (pixel, covered) in image.data.chunks_exact_mut(4).zip(covered) {
            if !covered && keep_color {
                pixel[3] = 0;
            } else if !covered {
                pixel.copy_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
}

struct Edge {
    x0: i32,
    y0: i32,
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
    dx: f32,
}

/// Pillow's scanline polygon fill (`ImagingDrawPolygon` with a fill), which decides the pixels on the outline: a
/// row is filled between each pair of edge crossings, rounded outwards, and horizontal edges are drawn in full.
fn fill_polygon(covered: &mut [bool], width: usize, height: usize, points: &[[i32; 2]]) {
    let mut hline = |x0: i32, y: i32, x1: i32| {
        if y < 0 || y as usize >= height || x0 >= width as i32 || x1 < 0 {
            return;
        }
        let (x0, x1) = (x0.max(0) as usize, (x1.min(width as i32 - 1)) as usize);
        if x0 <= x1 {
            covered[y as usize * width + x0..=y as usize * width + x1].fill(true);
        }
    };

    let mut edges = Vec::with_capacity(points.len());
    let mut y_min = height as i32 - 1;
    let mut y_max = 0;
    for (i, a) in points.iter().enumerate() {
        let b = points[(i + 1) % points.len()];
        let edge = Edge {
            x0: a[0],
            y0: a[1],
            x_min: a[0].min(b[0]),
            x_max: a[0].max(b[0]),
            y_min: a[1].min(b[1]),
            y_max: a[1].max(b[1]),
            dx: if a[1] == b[1] { 0.0 } else { (b[0] - a[0]) as f32 / (b[1] - a[1]) as f32 },
        };
        y_min = y_min.min(edge.y_min);
        y_max = y_max.max(edge.y_max);
        if edge.y_min == edge.y_max {
            hline(edge.x_min, edge.y_min, edge.x_max);
        } else {
            edges.push(edge);
        }
    }
    let y_min = y_min.max(0);
    let y_max = y_max.min(height as i32);

    let at = |edge: &Edge, y: i32| (y - edge.y0) as f32 * edge.dx + edge.x0 as f32;
    let mut xx: Vec<f32> = Vec::with_capacity(edges.len() * 2);
    for y in y_min..=y_max {
        xx.clear();
        for (i, current) in edges.iter().enumerate() {
            if y < current.y_min || y > current.y_max {
                continue;
            }
            let x = at(current, y);
            xx.push(x);
            if y == current.y_max && y < y_max {
                // The end of an edge counts twice, so that the edge starting there pairs up with it.
                xx.push(x);
            } else if current.dx != 0.0 && xx.len() % 2 == 1 && x.round() == x {
                // Two edges meeting in a corner would leave the corner row one pixel short; widen it to where the
                // edges are on the neighbouring row.
                for (k, other) in edges[..i].iter().enumerate() {
                    if (current.dx > 0.0 && other.dx <= 0.0) || (current.dx < 0.0 && other.dx >= 0.0) {
                        continue;
                    }
                    if x == at(other, y) {
                        let neighbour = if y == y_max { y - 1 } else { y + 1 };
                        let (a, b) = (at(current, neighbour), at(other, neighbour));
                        let widened = match (y == current.y_max, current.dx > 0.0) {
                            (true, true) => a.max(b) + 1.0,
                            (true, false) => a.min(b) - 1.0,
                            (false, true) => a.min(b),
                            (false, false) => a.max(b) + 1.0,
                        };
                        if let Some(slot) = xx.get_mut(k) {
                            *slot = widened;
                        }
                        break;
                    }
                }
            }
        }
        xx.sort_by(f32::total_cmp);
        let round_up = |f: f32| if f >= 0.0 { (f + 0.5).floor() as i32 } else { -((f.abs() + 0.5).floor() as i32) };
        let round_down = |f: f32| if f >= 0.0 { (f - 0.5).ceil() as i32 } else { -((f.abs() - 0.5).ceil() as i32) };
        let mut x_pos = if xx.is_empty() { -1 } else { 0 };
        for pair in xx.chunks_exact(2) {
            let x_end = round_down(pair[1]);
            if x_end < x_pos {
                continue;
            }
            let x_start = round_up(pair[0]).max(x_pos);
            if x_end < x_start {
                continue;
            }
            hline(x_start, y, x_end);
            x_pos = x_end + 1;
        }
    }
}
