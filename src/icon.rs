//! The app icon, drawn in code: a neon heptagon ring (cyan → purple → pink) around a watching eye,
//! on a dark rounded tile. Used by the build script (the exe's .ico) and for the window icon, so
//! this file only uses std.
use std::f32::consts::{PI, TAU};

type Rgb = [f32; 3];

fn hex(c: u32) -> Rgb {
    [((c >> 16) & 255) as f32 / 255.0, ((c >> 8) & 255) as f32 / 255.0, (c & 255) as f32 / 255.0]
}

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn add(a: Rgb, b: Rgb, k: f32) -> Rgb {
    [(a[0] + b[0] * k).min(1.0), (a[1] + b[1] * k).min(1.0), (a[2] + b[2] * k).min(1.0)]
}

/// Coverage of a signed distance (negative inside), anti-aliased over `aa`.
fn cover(d: f32, aa: f32) -> f32 {
    (0.5 - d / aa).clamp(0.0, 1.0)
}

fn len(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

fn sd_round_box(x: f32, y: f32, half: f32, r: f32) -> f32 {
    let (qx, qy) = (x.abs() - half + r, y.abs() - half + r);
    len(qx.max(0.0), qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

/// Regular `n`-gon of circumradius `r`, a vertex at the top (Inigo Quilez's formula).
fn sd_polygon(x: f32, y: f32, r: f32, n: u32) -> f32 {
    let an = PI / n as f32;
    let bn = x.atan2(y).rem_euclid(2.0 * an) - an;
    let l = len(x, y);
    let (mut px, mut py) = (l * bn.cos(), l * bn.sin().abs());
    px -= r * an.cos();
    py -= r * an.sin();
    py += (-py).clamp(0.0, r * an.sin());
    len(px, py) * px.signum()
}

/// `n`×`n` straight RGBA pixels.
pub fn render(n: u32) -> Vec<u8> {
    let s = n as f32;
    let aa = 1.5 / s;
    let (cyan, purple, pink) = (hex(0x00e5ff), hex(0xb026ff), hex(0xff2bd6));
    let ring_color = |t: f32| {
        let t = t.rem_euclid(1.0) * 3.0;
        match t as u32 {
            0 => lerp(cyan, purple, t),
            1 => lerp(purple, pink, t - 1.0),
            _ => lerp(pink, cyan, t - 2.0),
        }
    };
    // Lines stay at least about a pixel wide in the small sizes.
    let (ring_w, outline_w) = (0.03_f32.max(0.9 / s), 0.013_f32.max(0.55 / s));
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    for j in 0..n {
        for i in 0..n {
            let (x, y) = ((i as f32 + 0.5) / s - 0.5, (j as f32 + 0.5) / s - 0.5);
            let alpha = cover(sd_round_box(x, y, 0.47, 0.11), aa);
            if alpha <= 0.0 {
                out.extend([0, 0, 0, 0]);
                continue;
            }
            let r = len(x, y);
            // Tile: deep navy to black, darker at the rim.
            let mut c = lerp(hex(0x1b2142), hex(0x06070c), y + 0.5);
            c = lerp(c, hex(0x040509), (r / 0.72) * 0.5);
            // Neon heptagon ring with its glow.
            let rc = ring_color(x.atan2(-y) / TAU);
            let d_ring = sd_polygon(x, y, 0.36, 7).abs() - ring_w;
            c = add(c, rc, (-d_ring.max(0.0) / 0.05).exp() * 0.5);
            c = lerp(c, rc, cover(d_ring, aa));
            // The eye: a lens (two circles), outlined, with a glowing iris and a dark pupil.
            let (lr, lc) = (0.26, 0.155);
            let d_lens = (len(x, y - lc) - lr).max(len(x, y + lc) - lr);
            let inside = cover(d_lens, aa);
            c = lerp(c, hex(0x080a14), inside);
            c = lerp(c, lerp(hex(0xc8f9ff), rc, 0.3), cover(d_lens.abs() - outline_w, aa));
            let iris = lerp(cyan, purple, (x + y) * 4.0 + 0.5);
            let d_iris = r - 0.085;
            c = add(c, iris, (-d_iris.max(0.0) / 0.03).exp() * 0.3 * inside);
            c = lerp(c, iris, cover(d_iris, aa) * inside);
            c = lerp(c, hex(0x04050a), cover(r - 0.037, aa));
            c = lerp(c, [1.0, 1.0, 1.0], cover(len(x + 0.029, y + 0.03) - 0.015_f32.max(0.5 / s), aa) * 0.95);
            let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            out.extend([byte(c[0]), byte(c[1]), byte(c[2]), byte(alpha)]);
        }
    }
    out
}

/// A Windows .ico holding one 32-bit image per size (build.rs embeds it in the exe).
#[allow(dead_code)]
pub fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&n| dib(n, &render(n))).collect();
    let mut out = vec![];
    for v in [0u16, 1, sizes.len() as u16] {
        out.extend(v.to_le_bytes());
    }
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&n, img) in sizes.iter().zip(&images) {
        let b = if n >= 256 { 0 } else { n as u8 }; // 0 means 256
        out.extend([b, b, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((img.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += img.len() as u32;
    }
    images.into_iter().for_each(|img| out.extend(img));
    out
}

/// One .ico image: BITMAPINFOHEADER, bottom-up BGRA rows, an empty AND mask (alpha is used).
#[allow(dead_code)]
fn dib(n: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    for v in [40u32, n, 2 * n] {
        out.extend(v.to_le_bytes()); // header size, width, height (color + mask)
    }
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend([0u8; 24]);
    for y in (0..n).rev() {
        for x in 0..n {
            let i = ((y * n + x) * 4) as usize;
            out.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.extend(vec![0u8; ((n + 31) / 32 * 4 * n) as usize]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_a_tile_with_transparent_corners() {
        let px = render(64);
        assert_eq!(px.len(), 64 * 64 * 4);
        assert_eq!(px[3], 0); // top-left corner outside the rounded tile
        assert_eq!(px[(32 * 64 + 32) * 4 + 3], 255); // the center is opaque
    }

    #[test]
    fn ico_has_one_entry_per_size() {
        let f = ico(&[16, 32, 256]);
        assert_eq!(u16::from_le_bytes([f[4], f[5]]), 3);
        assert_eq!(f[6 + 32], 0); // the 256 px entry stores 0 as its width
        let first = u32::from_le_bytes([f[18], f[19], f[20], f[21]]) as usize;
        assert_eq!(u32::from_le_bytes([f[first], f[first + 1], f[first + 2], f[first + 3]]), 40);
    }
}
