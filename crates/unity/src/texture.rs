//! Decoding the pixels of a Texture2D.
//!
//! A Texture2D object holds the size, a format number and the pixels, either inline (`image data`) or, as in every
//! texture of this game, in the bundle's `.resS` entry at `m_StreamData.{offset, size}`. The pixels are the largest
//! mip level first, followed by the smaller ones; only the first is read. Rows are stored bottom to top, and
//! [`decode`] keeps that order: a sprite's rectangle is given in the same bottom-up coordinates.
//!
//! Formats (Unity's `TextureFormat` numbers) found in the UI bundle and decoded here: RGBA32 (4) and RGB24 (3),
//! which are plain bytes, and the block formats DXT1 (10) and DXT5 (12). ARGB32 (5) and BGRA32 (14) cost three lines
//! each and are included. BC7 (25) is used by a single sprite atlas that no icon comes from, and is refused rather
//! than paying for a decoder.
//!
//! DXT1 / BC1 stores 4x4 pixel blocks in 8 bytes: two RGB565 colours and sixteen 2-bit indices into a palette of
//! those two colours plus two blends (or, when the first colour is not greater than the second, one blend and
//! transparent black). DXT5 / BC3 puts 8 bytes of alpha in front: two alpha values and sixteen 3-bit indices into
//! their interpolations. The integer arithmetic below is Pillow's, so the result is identical to the Python tool's.

use crate::imaging::Image;

pub const RGB24: i64 = 3;
pub const RGBA32: i64 = 4;
pub const ARGB32: i64 = 5;
pub const DXT1: i64 = 10;
pub const DXT5: i64 = 12;
pub const BGRA32: i64 = 14;

/// Whether the format has no alpha channel. Pillow holds such a texture as an "RGB" image, and a sprite mask then
/// hides pixels without erasing their colour.
pub fn is_opaque(format: i64) -> bool {
    format == RGB24
}

/// Bytes of the first mip level, or `None` for a format that is not decoded.
pub fn data_len(format: i64, width: usize, height: usize) -> Option<usize> {
    let blocks = width.div_ceil(4) * height.div_ceil(4);
    Some(match format {
        RGB24 => width * height * 3,
        RGBA32 | ARGB32 | BGRA32 => width * height * 4,
        DXT1 => blocks * 8,
        DXT5 => blocks * 16,
        _ => return None,
    })
}

/// The first mip level as RGBA, rows in stored (bottom-up) order.
pub fn decode(format: i64, width: usize, height: usize, data: &[u8]) -> Result<Image, String> {
    let len = data_len(format, width, height).ok_or_else(|| format!("texture format {} is not supported", format))?;
    let data = data.get(..len).ok_or_else(|| format!("texture has {} bytes, {} needed", data.len(), len))?;
    let mut out = Image::new(width, height);
    match format {
        RGBA32 => out.data.copy_from_slice(data),
        RGB24 => {
            for (pixel, rgb) in out.data.chunks_exact_mut(4).zip(data.chunks_exact(3)) {
                pixel[..3].copy_from_slice(rgb);
                pixel[3] = 255;
            }
        }
        ARGB32 => {
            for (pixel, argb) in out.data.chunks_exact_mut(4).zip(data.chunks_exact(4)) {
                pixel.copy_from_slice(&[argb[1], argb[2], argb[3], argb[0]]);
            }
        }
        BGRA32 => {
            for (pixel, bgra) in out.data.chunks_exact_mut(4).zip(data.chunks_exact(4)) {
                pixel.copy_from_slice(&[bgra[2], bgra[1], bgra[0], bgra[3]]);
            }
        }
        _ => {
            let block_len = if format == DXT1 { 8 } else { 16 };
            let across = width.div_ceil(4);
            for (index, block) in data.chunks_exact(block_len).enumerate() {
                let mut pixels = [[0u8; 4]; 16];
                if format == DXT1 {
                    bc1_colors(block, false, &mut pixels);
                } else {
                    bc1_colors(&block[8..], true, &mut pixels);
                    bc3_alpha(&block[..8], &mut pixels);
                }
                // The last column and row of blocks can stick out of a texture whose size is not a multiple of 4.
                let (bx, by) = (index % across * 4, index / across * 4);
                for y in 0..4.min(height - by) {
                    for x in 0..4.min(width - bx) {
                        out.data[((by + y) * width + bx + x) * 4..][..4].copy_from_slice(&pixels[y * 4 + x]);
                    }
                }
            }
        }
    }
    Ok(out)
}

fn rgb565(c: u16) -> [u16; 3] {
    let r = (c & 0xf800) >> 8;
    let g = (c & 0x07e0) >> 3;
    let b = (c & 0x001f) << 3;
    [r | r >> 5, g | g >> 6, b | b >> 5]
}

/// `opaque`: a DXT5 block always uses the four-colour palette, whatever the order of its two colours.
fn bc1_colors(block: &[u8], opaque: bool, out: &mut [[u8; 4]; 16]) {
    let c0 = u16::from_le_bytes([block[0], block[1]]);
    let c1 = u16::from_le_bytes([block[2], block[3]]);
    let (a, b) = (rgb565(c0), rgb565(c1));
    let mix = |wa: u16, wb: u16, div: u16| -> [u8; 4] {
        let channel = |i: usize| ((wa * a[i] + wb * b[i]) / div) as u8;
        [channel(0), channel(1), channel(2), 255]
    };
    let palette = if c0 > c1 || opaque {
        [mix(1, 0, 1), mix(0, 1, 1), mix(2, 1, 3), mix(1, 2, 3)]
    } else {
        [mix(1, 0, 1), mix(0, 1, 1), mix(1, 1, 2), [0, 0, 0, 0]]
    };
    let indices = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    for (n, pixel) in out.iter_mut().enumerate() {
        *pixel = palette[(indices >> (2 * n) & 3) as usize];
    }
}

fn bc3_alpha(block: &[u8], out: &mut [[u8; 4]; 16]) {
    let (a0, a1) = (block[0] as u16, block[1] as u16);
    let mut alpha = [a0, a1, 0, 0, 0, 0, 0, 255];
    if a0 > a1 {
        for i in 1..7u16 {
            alpha[i as usize + 1] = ((7 - i) * a0 + i * a1) / 7;
        }
    } else {
        for i in 1..5u16 {
            alpha[i as usize + 1] = ((5 - i) * a0 + i * a1) / 5;
        }
    }
    let mut bits = [0u8; 8];
    bits[..6].copy_from_slice(&block[2..8]);
    let indices = u64::from_le_bytes(bits);
    for (n, pixel) in out.iter_mut().enumerate() {
        pixel[3] = alpha[(indices >> (3 * n) & 7) as usize] as u8;
    }
}
