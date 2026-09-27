//! The app icon, drawn in code: a seven-blade aperture (a lens, and the seven spokes of the
//! Kubernetes helm) around a heptagonal opening with a neon pupil, on a dark rounded tile. Sizes up
//! to 32 px get wider gaps, a larger opening and flat blades so they stay legible. Used by the build
//! script (the exe's .ico) and for the window icon, so this file only uses std.
use std::f32::consts::PI;

type Rgb = [f32; 3];

fn hex(c: u32) -> Rgb {
    [((c >> 16) & 255) as f32 / 255.0, ((c >> 8) & 255) as f32 / 255.0, (c & 255) as f32 / 255.0]
}

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
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

/// `n`×`n` straight RGBA pixels. Distances are in the design's 100-unit square, centered.
pub fn render(n: u32) -> Vec<u8> {
    let px = 100.0 / n as f32;
    let aa = 1.2 * px;
    let small = n <= 32;
    let (tile, accent, white) = (hex(0x0b0d12), hex(0x00e5ff), [1.0; 3]);
    // Blades reach `outer`; the opening is a heptagon of circumradius `hole`, vertex at the top.
    let (outer, hole, gap, pupil) = if small { (37.0, 16.0, 4.2, 6.2) } else { (35.0, 13.0, 1.8, 4.6) };
    let shade: [f32; 7] = if small { [1.0; 7] } else { [1.0, 0.86, 0.74, 0.64, 0.56, 0.49, 0.43] };
    let apothem = hole * (PI / 7.0).cos();
    // Outward normal of each opening edge j (from vertex j to j + 1).
    let normal: [(f32, f32); 7] = std::array::from_fn(|j| {
        let a = (-90.0 + (j as f32 + 0.5) * 360.0 / 7.0_f32).to_radians();
        (a.cos(), a.sin())
    });
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    for j in 0..n {
        for i in 0..n {
            let (x, y) = ((i as f32 + 0.5) * px - 50.0, (j as f32 + 0.5) * px - 50.0);
            let d_tile = sd_round_box(x, y, 46.0, 21.0);
            let alpha = cover(d_tile, aa);
            if alpha <= 0.0 {
                out.extend([0, 0, 0, 0]);
                continue;
            }
            // Tile with a faint edge so it holds on a dark taskbar.
            let mut c = lerp(tile, white, 0.07 * cover((d_tile + 0.5).abs() - 0.5, aa));
            let r = len(x, y);
            if !small {
                c = lerp(c, accent, 0.32 * cover((r - 39.5).abs() - 0.7, aa)); // lens barrel
            }
            // Past edge k + 1 but not past edge k: blade k (each edge extended to the rim splits
            // the ring into seven blades); the gap is cut from every side.
            let side: [f32; 7] = std::array::from_fn(|k| x * normal[k].0 + y * normal[k].1 - apothem);
            for k in 0..7 {
                let d = side[k].max(-side[(k + 1) % 7]).max(r - outer) + gap / 2.0;
                c = lerp(c, accent, shade[k] * cover(d, aa));
            }
            c = lerp(c, accent, cover(r - pupil, aa));
            if !small {
                c = lerp(c, white, 0.9 * cover(len(x + 1.8, y + 1.9) - 1.5, aa)); // glint
            }
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
    fn pupil_sits_in_a_dark_opening() {
        // (size, a pixel between the pupil and the blades), in both the small and the full design
        for (n, y) in [(16u32, 6u32), (64, 26)] {
            let px = render(n);
            let green = |x: u32, y: u32| px[((y * n + x) * 4 + 1) as usize];
            assert!(green(n / 2, n / 2) > 150, "cyan pupil at {n} px"); // at 16 px it spans four pixels
            assert!(green(n / 2, y) < 40, "dark opening at {n} px");
        }
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
