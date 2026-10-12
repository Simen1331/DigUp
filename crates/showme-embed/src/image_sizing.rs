//! How the reference sizes an image for the vision encoder, so llama.cpp sees the same pixels.
//!
//! mtmd fixes the token budget per context, and each context loads the encoder weights; it also rescales only images
//! outside its range, while the reference always fills the budget. So images are resized here, and mtmd takes them as
//! they are. Checked against Pillow bit for bit.

/// transformers' Gemma 4 image processor: scale to fill `budget` soft tokens (each 3 × 3 patches of 16 px) keeping the
/// aspect ratio, each side rounded down to a multiple of 48 px. Returns (height, width).
pub fn target_size(height: usize, width: usize, budget: usize) -> Option<(usize, usize)> {
    let (patch, pooling) = (16, 3);
    let max_patches = budget * pooling * pooling;
    let factor = ((max_patches * patch * patch) as f64 / (height * width) as f64).sqrt();
    let side = pooling * patch;
    let mut target_height = (factor * height as f64 / side as f64).floor() as usize * side;
    let mut target_width = (factor * width as f64 / side as f64).floor() as usize * side;
    if target_height == 0 && target_width == 0 {
        return None;
    }
    let max_side = (max_patches / (pooling * pooling)) * side;
    if target_height == 0 {
        target_height = side;
        target_width = (width / height * side).min(max_side);
    } else if target_width == 0 {
        target_width = side;
        target_height = (height / width * side).min(max_side);
    }
    Some((target_height, target_width))
}

/// Pillow's bicubic resampling of packed RGB (a = −0.5, the kernel widened when shrinking, 22-bit fixed point): what
/// the reference gets from PIL's Image.resize. Ported from mtmd's copy of it.
pub fn resize_bicubic(source: &[u8], width: usize, height: usize, to_width: usize, to_height: usize) -> Vec<u8> {
    match (to_width != width, to_height != height) {
        (true, true) => {
            let pass = horizontal(source, width, height, to_width);
            vertical(&pass, height, to_width, to_height)
        }
        (true, false) => horizontal(source, width, height, to_width),
        (false, true) => vertical(source, height, width, to_height),
        (false, false) => source[..width * height * 3].to_vec(),
    }
}

const PRECISION: i32 = 22;

fn filter(value: f64) -> f64 {
    let (x, a) = (value.abs(), -0.5);
    if x < 1.0 {
        return ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0;
    }
    if x < 2.0 {
        return (((x - 5.0) * x + 8.0) * x - 4.0) * a;
    }
    0.0
}

/// For each output pixel: the first contributing input pixel, how many contribute, and their weights.
fn coefficients(in_size: usize, out_size: usize) -> (Vec<usize>, Vec<i32>, usize) {
    let scale = in_size as f64 / out_size as f64;
    let filter_scale = scale.max(1.0);
    let support = 2.0 * filter_scale;
    let size = support.ceil() as usize * 2 + 1;
    let mut bounds = vec![0usize; out_size * 2];
    let mut weights = vec![0i32; out_size * size];
    let mut row = vec![0.0f64; size];
    for out in 0..out_size {
        let center = (out as f64 + 0.5) * scale;
        let first = ((center - support + 0.5) as i64).max(0) as usize;
        let count = (((center + support + 0.5) as i64) as usize).min(in_size) - first;
        let mut total = 0.0;
        for (x, slot) in row.iter_mut().enumerate() {
            *slot = if x < count { filter(((x + first) as f64 - center + 0.5) / filter_scale) } else { 0.0 };
            total += *slot;
        }
        for (x, value) in row.iter().enumerate() {
            let weight = if x < count && total != 0.0 { value / total } else { *value };
            weights[out * size + x] =
                (weight * (1i64 << PRECISION) as f64 + if weight < 0.0 { -0.5 } else { 0.5 }) as i32;
        }
        bounds[out * 2] = first;
        bounds[out * 2 + 1] = count;
    }
    (bounds, weights, size)
}

fn clip(value: i32) -> u8 {
    (value >> PRECISION).clamp(0, 255) as u8
}

fn horizontal(src: &[u8], in_width: usize, rows: usize, out_width: usize) -> Vec<u8> {
    let (bounds, weights, size) = coefficients(in_width, out_width);
    let mut out = vec![0u8; out_width * rows * 3];
    for y in 0..rows {
        for x in 0..out_width {
            let start = (y * in_width + bounds[x * 2]) * 3;
            let (mut r, mut g, mut b) = (1i32 << (PRECISION - 1), 1i32 << (PRECISION - 1), 1i32 << (PRECISION - 1));
            for k in 0..bounds[x * 2 + 1] {
                let w = weights[x * size + k];
                let p = &src[start + k * 3..start + k * 3 + 3];
                r = r.wrapping_add((p[0] as i32).wrapping_mul(w));
                g = g.wrapping_add((p[1] as i32).wrapping_mul(w));
                b = b.wrapping_add((p[2] as i32).wrapping_mul(w));
            }
            let o = (y * out_width + x) * 3;
            out[o] = clip(r);
            out[o + 1] = clip(g);
            out[o + 2] = clip(b);
        }
    }
    out
}

fn vertical(src: &[u8], in_height: usize, row_width: usize, out_height: usize) -> Vec<u8> {
    let (bounds, weights, size) = coefficients(in_height, out_height);
    let stride = row_width * 3;
    let mut out = vec![0u8; stride * out_height];
    let mut sums = vec![0i32; stride];
    for y in 0..out_height {
        sums.fill(1 << (PRECISION - 1));
        for k in 0..bounds[y * 2 + 1] {
            let row = &src[(bounds[y * 2] + k) * stride..(bounds[y * 2] + k + 1) * stride];
            let w = weights[y * size + k];
            for (sum, pixel) in sums.iter_mut().zip(row) {
                *sum = sum.wrapping_add((*pixel as i32).wrapping_mul(w));
            }
        }
        for (i, sum) in sums.iter().enumerate() {
            out[y * stride + i] = clip(*sum);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_size_fills_the_budget() {
        assert_eq!(target_size(900, 1440, 280), Some((624, 1008)));
        assert_eq!(target_size(900, 1440, 140), Some((432, 672)));
        assert_eq!(target_size(900, 1440, 70), Some((288, 480)));
        assert_eq!(target_size(768, 768, 280), Some((768, 768)));
        assert_eq!(target_size(768, 768, 140), Some((528, 528)));
        assert_eq!(target_size(300, 300, 140), Some((528, 528))); // small images grow to fill it
        assert_eq!(target_size(10, 4000, 140), Some((48, 6720))); // a thin one: one 48 px row
    }

    /// A 7×5 image resized by PIL (Image.resize, BICUBIC).
    const SOURCE: [u8; 105] = [
        0, 115, 230, 3, 118, 233, 6, 121, 236, 9, 124, 239, 12, 127, 242, 15, 130, 245, 18, 133, 248, 77, 192, 51, 80,
        195, 54, 83, 198, 57, 86, 201, 60, 89, 204, 63, 92, 207, 66, 95, 210, 69, 154, 13, 128, 157, 16, 131, 160, 19,
        134, 163, 22, 137, 166, 25, 140, 169, 28, 143, 172, 31, 146, 231, 90, 205, 234, 93, 208, 237, 96, 211, 240, 99,
        214, 243, 102, 217, 246, 105, 220, 249, 108, 223, 52, 167, 26, 55, 170, 29, 58, 173, 32, 61, 176, 35, 64, 179,
        38, 67, 182, 41, 70, 185, 44,
    ];

    #[test]
    fn shrink_matches_pillow() {
        let pil = [
            28, 151, 152, 33, 156, 157, 39, 162, 163, 44, 167, 168, 165, 68, 129, 170, 73, 134, 176, 79, 140, 181, 84,
            145, 132, 133, 106, 137, 138, 111, 143, 144, 117, 148, 149, 122,
        ];
        assert_eq!(resize_bicubic(&SOURCE, 7, 5, 4, 3), pil);
    }

    #[test]
    fn grow_matches_pillow() {
        let pil = [
            0, 112, 237, 0, 113, 238, 1, 116, 241, 3, 118, 243, 5, 120, 245, 7, 122, 247, 9, 124, 249, 11, 126, 251,
            14, 129, 254, 15, 130, 255, 55, 187, 85, 56, 188, 86, 59, 191, 89, 61, 193, 91, 63, 195, 93, 65, 197, 95,
            67, 199, 97, 69, 201, 99, 72, 204, 102, 73, 205, 103, 122, 82, 83, 123, 83, 84, 126, 86, 87, 128, 88, 89,
            130, 90, 91, 132, 92, 93, 134, 94, 95, 136, 96, 97, 139, 99, 100, 140, 100, 101, 199, 27, 173, 200, 28,
            174, 203, 31, 177, 205, 33, 179, 207, 35, 181, 209, 37, 183, 211, 39, 185, 213, 41, 187, 216, 44, 190, 217,
            45, 191, 197, 112, 171, 198, 113, 172, 201, 116, 175, 203, 118, 177, 205, 120, 179, 207, 122, 181, 209,
            124, 183, 211, 126, 185, 214, 129, 188, 215, 130, 189, 45, 170, 19, 46, 171, 20, 49, 174, 23, 51, 176, 25,
            53, 178, 27, 55, 180, 29, 57, 182, 31, 59, 184, 33, 62, 187, 36, 63, 188, 37,
        ];
        assert_eq!(resize_bicubic(&SOURCE, 7, 5, 10, 6), pil);
    }

    #[test]
    fn one_axis_matches_pillow() {
        let pil = [
            74, 120, 137, 77, 123, 140, 80, 126, 143, 83, 129, 146, 86, 132, 149, 89, 135, 152, 92, 138, 155, 153, 100,
            119, 156, 103, 122, 159, 106, 125, 162, 109, 128, 165, 112, 131, 168, 115, 134, 171, 118, 137,
        ];
        assert_eq!(resize_bicubic(&SOURCE, 7, 5, 7, 2), pil);
        assert_eq!(resize_bicubic(&SOURCE, 7, 5, 7, 5), SOURCE);
    }
}
