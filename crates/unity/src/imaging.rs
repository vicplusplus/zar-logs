//! The few image operations the icon cache needs, done the way Pillow does them.
//!
//! The icons were first produced with Python's Pillow (`crop(getbbox())`, `thumbnail()`), and the page that shows
//! them was tuned on that output, so these are ports of Pillow's algorithms rather than "a" bicubic resize:
//!
//! * [`Image::resize`] is `ImagingResample` with the bicubic filter: a separable convolution whose support widens
//!   with the reduction factor (so it averages when shrinking instead of skipping pixels), run horizontally then
//!   vertically, in fixed point with 22 fractional bits and 8-bit rounding between the passes.
//! * An image with transparency is resized premultiplied (`RGBA -> RGBa -> resize -> RGBA`), otherwise the colour
//!   of fully transparent pixels would bleed into the edges.
//! * [`thumbnail_size`] is the size rule of `Image.thumbnail`.
//!
//! One thing is left out: for an image without alpha that is more than 4x larger than the target, `thumbnail` first
//! shrinks by an integer factor (`reducing_gap`). No picture of the game is that large; [`Image::resize`] samples
//! the full image instead, which differs from Pillow only in the last bit of quality there.

/// 8-bit RGBA, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

const PRECISION_BITS: u32 = 32 - 8 - 2;

impl Image {
    pub fn new(width: usize, height: usize) -> Image {
        Image { width, height, data: vec![0; width * height * 4] }
    }

    pub fn row(&self, y: usize) -> &[u8] {
        &self.data[y * self.width * 4..(y + 1) * self.width * 4]
    }

    pub fn flip_vertical(&mut self) {
        let stride = self.width * 4;
        let (mut top, mut bottom) = (0, self.height.saturating_sub(1));
        while top < bottom {
            let (head, tail) = self.data.split_at_mut(bottom * stride);
            head[top * stride..(top + 1) * stride].swap_with_slice(&mut tail[..stride]);
            top += 1;
            bottom -= 1;
        }
    }

    pub fn flip_horizontal(&mut self) {
        let width = self.width;
        for row in self.data.chunks_exact_mut((width * 4).max(1)) {
            for x in 0..width / 2 {
                let (left, right) = row.split_at_mut((width - 1 - x) * 4);
                left[x * 4..x * 4 + 4].swap_with_slice(&mut right[..4]);
            }
        }
    }

    /// Pillow's `ROTATE_270` (a quarter turn clockwise).
    pub fn rotate_clockwise(&self) -> Image {
        let mut out = Image::new(self.height, self.width);
        for y in 0..out.height {
            for x in 0..out.width {
                let from = ((self.height - 1 - x) * self.width + y) * 4;
                out.data[(y * out.width + x) * 4..][..4].copy_from_slice(&self.data[from..from + 4]);
            }
        }
        out
    }

    /// The rectangle `x0..x1, y0..y1`; whatever lies outside the image is transparent black, as in Pillow.
    pub fn crop(&self, x0: i64, y0: i64, x1: i64, y1: i64) -> Image {
        let mut out = Image::new((x1 - x0).max(0) as usize, (y1 - y0).max(0) as usize);
        let from_x = x0.clamp(0, self.width as i64);
        let to_x = x1.clamp(from_x, self.width as i64);
        let n = (to_x - from_x) as usize * 4;
        for y in y0.max(0)..y1.min(self.height as i64) {
            let src = (y as usize * self.width + from_x as usize) * 4;
            let dst = ((y - y0) as usize * out.width + (from_x - x0) as usize) * 4;
            out.data[dst..dst + n].copy_from_slice(&self.data[src..src + n]);
        }
        out
    }

    /// Bounding box `(x0, y0, x1, y1)` of the pixels that are not fully transparent; `None` when there are none.
    pub fn bbox(&self) -> Option<(usize, usize, usize, usize)> {
        let (mut x0, mut y0, mut x1, mut y1) = (self.width, usize::MAX, 0, 0);
        for y in 0..self.height {
            let row = self.row(y);
            let Some(first) = (0..self.width).find(|&x| row[x * 4 + 3] != 0) else { continue };
            let last = (0..self.width).rev().find(|&x| row[x * 4 + 3] != 0).unwrap_or(first);
            x0 = x0.min(first);
            x1 = x1.max(last + 1);
            y0 = y0.min(y);
            y1 = y + 1;
        }
        (y0 != usize::MAX).then_some((x0, y0, x1, y1))
    }

    pub fn set_opaque(&mut self) {
        for pixel in self.data.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }

    /// Bicubic resize of all four channels as plain numbers. Right for an opaque image; one with transparency goes
    /// through [`Image::resize_with_alpha`].
    pub fn resize(&self, width: usize, height: usize) -> Image {
        if (width, height) == (self.width, self.height) || self.width == 0 || self.height == 0 {
            return self.clone();
        }
        let horizontal = Coefficients::new(self.width, width);
        let vertical = Coefficients::new(self.height, height);
        // Only the source rows the vertical pass will read are filtered horizontally.
        let first_row = vertical.bounds.first().map_or(0, |b| b.0);
        let last_row = vertical.bounds.last().map_or(self.height, |b| b.0 + b.1);

        let mut temp;
        let (source, row_shift) = if width != self.width {
            temp = Image::new(width, last_row - first_row);
            for y in 0..temp.height {
                let src = self.row(y + first_row);
                let dst = &mut temp.data[y * width * 4..(y + 1) * width * 4];
                for (x, out) in dst.chunks_exact_mut(4).enumerate() {
                    let (min, count) = horizontal.bounds[x];
                    let k = &horizontal.weights[x * horizontal.size..][..count];
                    let mut sum = [1i32 << (PRECISION_BITS - 1); 4];
                    for (i, weight) in k.iter().enumerate() {
                        let pixel = &src[(min + i) * 4..][..4];
                        for c in 0..4 {
                            sum[c] += pixel[c] as i32 * weight;
                        }
                    }
                    for c in 0..4 {
                        out[c] = clip8(sum[c]);
                    }
                }
            }
            (&temp, first_row)
        } else {
            (self, 0)
        };
        if height == self.height {
            return source.clone();
        }

        let mut out = Image::new(width, height);
        let stride = width * 4;
        let mut sums = vec![0i32; stride];
        for y in 0..height {
            let (min, count) = vertical.bounds[y];
            let k = &vertical.weights[y * vertical.size..][..count];
            sums.fill(1 << (PRECISION_BITS - 1));
            for (i, weight) in k.iter().enumerate() {
                let row = source.row(min - row_shift + i);
                for (sum, &value) in sums.iter_mut().zip(row) {
                    *sum += value as i32 * weight;
                }
            }
            for (out, &sum) in out.data[y * stride..(y + 1) * stride].iter_mut().zip(&sums) {
                *out = clip8(sum);
            }
        }
        out
    }

    /// Bicubic resize of an image with transparency: colours are weighted by their alpha.
    pub fn resize_with_alpha(&self, width: usize, height: usize) -> Image {
        if (width, height) == (self.width, self.height) {
            return self.clone();
        }
        let mut work = self.clone();
        for pixel in work.data.chunks_exact_mut(4) {
            let alpha = pixel[3] as u32;
            for c in &mut pixel[..3] {
                // Pillow's MULDIV255: a rounded division by 255 without dividing.
                let t = *c as u32 * alpha + 128;
                *c = (((t >> 8) + t) >> 8) as u8;
            }
        }
        let mut out = work.resize(width, height);
        for pixel in out.data.chunks_exact_mut(4) {
            let alpha = pixel[3] as u32;
            if alpha != 0 && alpha != 255 {
                for c in &mut pixel[..3] {
                    *c = (255 * *c as u32 / alpha).min(255) as u8;
                }
            }
        }
        out
    }
}

fn clip8(value: i32) -> u8 {
    (value >> PRECISION_BITS).clamp(0, 255) as u8
}

fn bicubic(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = x.abs();
    if x < 1.0 {
        ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        (((x - 5.0) * x + 8.0) * x - 4.0) * A
    } else {
        0.0
    }
}

/// For each output position: the first source pixel it reads, how many, and their weights (which sum to 1).
struct Coefficients {
    size: usize,
    bounds: Vec<(usize, usize)>,
    weights: Vec<i32>,
}

impl Coefficients {
    fn new(in_size: usize, out_size: usize) -> Coefficients {
        let scale = in_size as f64 / out_size as f64;
        let filter_scale = scale.max(1.0);
        let support = 2.0 * filter_scale;
        let size = support.ceil() as usize * 2 + 1;
        let mut bounds = Vec::with_capacity(out_size);
        let mut weights = vec![0i32; out_size * size];
        let mut k = vec![0f64; size];
        // Multiplying by the reciprocal, not dividing: the weights must come out bit for bit as Pillow's.
        let inverse_scale = 1.0 / filter_scale;
        for xx in 0..out_size {
            let center = (xx as f64 + 0.5) * scale;
            let min = ((center - support + 0.5) as i64).max(0);
            let max = ((center + support + 0.5) as i64).min(in_size as i64);
            let count = (max - min).max(0) as usize;
            let mut total = 0.0;
            for (x, weight) in k[..count].iter_mut().enumerate() {
                *weight = bicubic(((x as i64 + min) as f64 - center + 0.5) * inverse_scale);
                total += *weight;
            }
            for (x, weight) in k[..count].iter().enumerate() {
                let w = if total != 0.0 { weight / total } else { *weight };
                let fixed = w * (1u32 << PRECISION_BITS) as f64;
                weights[xx * size + x] = if w < 0.0 { (fixed - 0.5) as i32 } else { (fixed + 0.5) as i32 };
            }
            bounds.push((min as usize, count));
        }
        Coefficients { size, bounds, weights }
    }
}

/// The size `Image.thumbnail((max, max))` gives an image: the aspect ratio is kept, the longest side becomes
/// `max`. `None` when the image already fits (a thumbnail is never an enlargement).
pub fn thumbnail_size(width: usize, height: usize, max: usize) -> Option<(usize, usize)> {
    if (max >= width && max >= height) || width == 0 || height == 0 {
        return None;
    }
    let aspect = width as f64 / height as f64;
    // Of the two integers around the exact size, the one that keeps the aspect ratio best; the lower one on a tie.
    let pick = |exact: f64, error: &dyn Fn(f64) -> f64| -> usize {
        let (low, high) = (exact.floor(), exact.ceil());
        (if error(high) < error(low) { high } else { low }).max(1.0) as usize
    };
    let max_f = max as f64;
    Some(if 1.0 >= aspect {
        (pick(max_f * aspect, &|n| (aspect - n / max_f).abs()), max)
    } else {
        (max, pick(max_f / aspect, &|n| if n == 0.0 { 0.0 } else { (aspect - max_f / n).abs() }))
    })
}

#[cfg(test)]
mod tests {
    use super::{thumbnail_size, Image};

    /// Sizes Pillow's `thumbnail` gives, taken from Pillow itself.
    #[test]
    fn thumbnail_sizes_match_pillow() {
        let cases = [
            ((183, 183, 192), None),
            ((512, 336, 560), None),
            ((300, 300, 192), Some((192, 192))),
            ((400, 157, 192), Some((192, 75))),
            ((157, 400, 192), Some((75, 192))),
            ((1297, 850, 560), Some((560, 367))),
            ((1000, 3, 192), Some((192, 1))),
            ((3, 1000, 192), Some((1, 192))),
            ((193, 192, 192), Some((192, 191))),
            ((577, 333, 192), Some((192, 111))),
        ];
        for ((width, height, max), expected) in cases {
            assert_eq!(thumbnail_size(width, height, max), expected, "{}x{} into {}", width, height, max);
        }
    }

    #[test]
    fn bbox_and_crop() {
        let mut image = Image::new(5, 4);
        image.data[(5 + 2) * 4 + 3] = 1;
        image.data[(2 * 5 + 3) * 4 + 3] = 255;
        assert_eq!(image.bbox(), Some((2, 1, 4, 3)));
        assert_eq!(Image::new(3, 3).bbox(), None);
        let cropped = image.crop(2, 1, 7, 3);
        assert_eq!((cropped.width, cropped.height), (5, 2));
        assert_eq!(cropped.data[3], 1);
        assert_eq!(cropped.data[(5 + 1) * 4 + 3], 255);
    }

    #[test]
    fn a_flat_colour_stays_flat_when_resized() {
        let mut image = Image::new(40, 30);
        for pixel in image.data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[10, 200, 30, 128]);
        }
        let small = image.resize_with_alpha(13, 10);
        assert_eq!((small.width, small.height), (13, 10));
        assert!(small.data.chunks_exact(4).all(|p| p == [9, 199, 29, 128] || p == [10, 200, 30, 128]));
    }
}
