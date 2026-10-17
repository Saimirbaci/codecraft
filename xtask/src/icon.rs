//! CodeCraft's app icon, drawn from signed distance fields (our own design: an orange open ring
//! around a white diamond on a night-blue rounded square). Rendered at any size with analytic
//! anti-aliasing, so every icon-set size is drawn natively rather than scaled.

use std::path::Path;

/// One RGBA colour, straight alpha, components 0..=1.
#[derive(Clone, Copy)]
struct Rgba(f32, f32, f32, f32);

fn hex(rgb: u32) -> Rgba {
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
    Rgba(c(16), c(8), c(0), 1.0)
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    Rgba(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t, a.3 + (b.3 - a.3) * t)
}

/// Source-over compositing of `src` with coverage `cov` onto `dst`.
fn over(dst: Rgba, src: Rgba, cov: f32) -> Rgba {
    let sa = (src.3 * cov).clamp(0.0, 1.0);
    let out_a = sa + dst.3 * (1.0 - sa);
    if out_a <= 0.0 {
        return Rgba(0.0, 0.0, 0.0, 0.0);
    }
    let ch = |s: f32, d: f32| (s * sa + d * dst.3 * (1.0 - sa)) / out_a;
    Rgba(ch(src.0, dst.0), ch(src.1, dst.1), ch(src.2, dst.2), out_a)
}

/// Signed distance to a rounded box centred at the origin (half extent `b`, corner radius `r`).
fn rounded_box(x: f32, y: f32, b: f32, r: f32) -> f32 {
    let qx = x.abs() - b + r;
    let qy = y.abs() - b + r;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Signed distance to a ring of radius `r` and width `w` with an opening of half-angle `gap`
/// facing +x, with round ends.
fn open_ring(x: f32, y: f32, r: f32, w: f32, gap: f32) -> f32 {
    let angle = y.atan2(x).abs();
    if angle > gap {
        ((x * x + y * y).sqrt() - r).abs() - w / 2.0
    } else {
        let (ex, ey) = (r * gap.cos(), r * gap.sin());
        let d1 = ((x - ex).powi(2) + (y - ey).powi(2)).sqrt();
        let d2 = ((x - ex).powi(2) + (y + ey).powi(2)).sqrt();
        d1.min(d2) - w / 2.0
    }
}

/// Signed distance to a square rotated 45° (a diamond) with half-diagonal `h`.
fn diamond(x: f32, y: f32, h: f32) -> f32 {
    (x.abs() + y.abs() - h) / std::f32::consts::SQRT_2
}

/// Coverage of a shape from its signed distance, with a one-pixel anti-aliased edge.
fn coverage(d: f32, px: f32) -> f32 {
    (0.5 - d / px).clamp(0.0, 1.0)
}

/// The icon as straight-alpha RGBA bytes, `size`×`size`.
pub fn render(size: u32) -> Vec<u8> {
    let size = size.max(1);
    let px = 1.0 / size as f32;
    let top = hex(0x3a4f9a);
    let bottom = hex(0x161b36);
    let ring_a = hex(0xff9a3c);
    let ring_b = hex(0xe5502b);
    let white = hex(0xf4f6ff);
    let mut out = Vec::with_capacity((size as usize).saturating_mul(size as usize).saturating_mul(4));
    for j in 0..size {
        for i in 0..size {
            // Centred coordinates, y down, the canvas spanning -0.5..0.5.
            let x = (i as f32 + 0.5) * px - 0.5;
            let y = (j as f32 + 0.5) * px - 0.5;
            let mut c = Rgba(0.0, 0.0, 0.0, 0.0);
            // macOS icon grid: the body is 824/1024 of the canvas with a soft shadow below.
            let (half, radius) = (0.4023, 0.1807);
            let shadow = rounded_box(x, y - 0.012, half, radius);
            let shadow_alpha = 0.32 * (1.0 - (shadow / 0.03).clamp(0.0, 1.0));
            c = over(c, Rgba(0.0, 0.0, 0.0, 1.0), shadow_alpha.max(0.0));
            let body = rounded_box(x, y, half, radius);
            let fill = mix(top, bottom, (y + half) / (2.0 * half));
            c = over(c, fill, coverage(body, px));
            // A faint light rim inside the edge.
            let rim = (body + 0.006).abs() - 0.003;
            c = over(c, Rgba(1.0, 1.0, 1.0, 0.10), coverage(rim, px) * coverage(body, px));
            // The open ring, its gradient running along the diagonal.
            let ring = open_ring(x, y, 0.205, 0.085, 0.62);
            let ring_fill = mix(ring_a, ring_b, (x + y + 0.4) / 0.8);
            c = over(c, ring_fill, coverage(ring, px));
            // The diamond, slightly right of centre, inside the ring's opening line of sight.
            let gem = diamond(x - 0.02, y, 0.085);
            c = over(c, white, coverage(gem, px));
            let to_byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            out.extend_from_slice(&[to_byte(c.0), to_byte(c.1), to_byte(c.2), to_byte(c.3)]);
        }
    }
    out
}

/// Encode RGBA bytes as a PNG file.
pub fn encode_png(size: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::High);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

/// Write the icon at `size` as a PNG file.
pub fn write_png(path: &Path, size: u32) -> Result<(), String> {
    let bytes = encode_png(size, &render(size))?;
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// A Windows `.ico` holding PNG images (Vista and later read PNG entries) at `sizes` (each 1..=256).
pub fn encode_ico(sizes: &[u32]) -> Result<Vec<u8>, String> {
    let images: Vec<(u32, Vec<u8>)> =
        sizes.iter().map(|&s| encode_png(s, &render(s)).map(|png| (s, png))).collect::<Result<_, _>>()?;
    let count = u16::try_from(images.len()).map_err(|_| "too many icon sizes".to_string())?;
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&count.to_le_bytes());
    let mut offset = 6 + 16 * u32::from(count);
    for (size, png) in &images {
        if !(1..=256).contains(size) {
            return Err(format!("icon size {size} is outside 1..=256"));
        }
        // 0 means 256 in the directory entry.
        let dim = u8::try_from(*size).unwrap_or(0);
        let len = u32::try_from(png.len()).map_err(|_| "icon image too large".to_string())?;
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset = offset.checked_add(len).ok_or_else(|| "icon too large".to_string())?;
    }
    for (_, png) in images {
        out.extend_from_slice(&png);
    }
    Ok(out)
}

/// Write the Windows icon (16 to 256 pixels).
pub fn write_ico(path: &Path) -> Result<(), String> {
    let bytes = encode_ico(&[16, 32, 48, 64, 128, 256])?;
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// The files of a macOS `.iconset` directory: (file name, pixel size).
pub const ICONSET: &[(&str, u32)] = &[
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

/// Write every `.iconset` file into `dir` (created if needed).
pub fn write_iconset(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, size) in ICONSET {
        write_png(&dir.join(name), *size)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn corners_are_transparent_and_the_body_is_opaque() {
        let size = 128;
        let img = render(size);
        assert_eq!(img.len(), (size * size * 4) as usize);
        assert_eq!(pixel(&img, size, 0, 0)[3], 0);
        assert_eq!(pixel(&img, size, size - 1, 0)[3], 0);
        // Inside the body, off the glyphs: opaque blue.
        let p = pixel(&img, size, size / 2, (size as f32 * 0.17) as u32);
        assert_eq!(p[3], 255);
        assert!(p[2] > p[0], "{p:?}");
        // The diamond near the centre is white.
        let c = pixel(&img, size, size / 2 + 2, size / 2);
        assert!(c[0] > 230 && c[1] > 230 && c[2] > 230, "{c:?}");
        // The ring on the left is orange.
        let r = pixel(&img, size, (size as f32 * 0.295) as u32, size / 2);
        assert!(r[0] > 200 && r[2] < 120, "{r:?}");
    }

    #[test]
    fn png_round_trips() {
        let size = 32;
        let bytes = encode_png(size, &render(size)).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (size, size));
        assert_eq!(&buf[..info.buffer_size()], render(size).as_slice());
    }

    #[test]
    fn iconset_has_every_size() {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("CodeCraft.iconset");
        write_iconset(&set).unwrap();
        for (name, size) in ICONSET {
            let bytes = std::fs::read(set.join(name)).unwrap();
            let reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().unwrap();
            assert_eq!(reader.info().width, *size, "{name}");
        }
    }
}
