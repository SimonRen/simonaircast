//! The simonaircast icon, drawn in code: a rounded square in a state colour
//! with a white speaker and two sound waves. Used for the tray (32 px) and,
//! via `tools/make_icon.py`, for the .exe/installer icon (`assets/simonaircast.ico`).

/// Designed on a 32-unit grid; `rgba` scales it to any size.
const GRID: f32 = 32.0;
/// 4×4 supersampling per pixel for smooth edges.
const SUPERSAMPLE: usize = 4;

pub const IDLE: [u8; 3] = [107, 114, 128];
pub const CONNECTING: [u8; 3] = [37, 99, 235];
pub const STREAMING: [u8; 3] = [22, 163, 74];
pub const RETRYING: [u8; 3] = [217, 119, 6];
/// App/installer icon colour.
pub const BRAND: [u8; 3] = CONNECTING;

/// `size`×`size` RGBA pixels (row-major, top-down).
pub fn rgba(color: [u8; 3], size: usize) -> Vec<u8> {
    let scale = GRID / size as f32;
    let mut out = Vec::with_capacity(size * size * 4);
    for py in 0..size {
        for px in 0..size {
            let (mut bg, mut fg) = (0usize, 0usize);
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let x = (px as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32) * scale;
                    if rounded_square(x, y) {
                        bg += 1;
                        if speaker_glyph(x, y) {
                            fg += 1;
                        }
                    }
                }
            }
            let n = (SUPERSAMPLE * SUPERSAMPLE) as f32;
            let white = fg as f32 / bg.max(1) as f32;
            for c in color {
                out.push((f32::from(c) * (1.0 - white) + 255.0 * white) as u8);
            }
            out.push((255.0 * bg as f32 / n) as u8);
        }
    }
    out
}

fn rounded_square(x: f32, y: f32) -> bool {
    let (lo, hi, r) = (1.0, 31.0, 7.0);
    let cx = x.clamp(lo + r, hi - r);
    let cy = y.clamp(lo + r, hi - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r && (lo..=hi).contains(&x) && (lo..=hi).contains(&y)
}

fn speaker_glyph(x: f32, y: f32) -> bool {
    // Magnet: small box; cone: trapezoid widening to the right.
    let body = (7.0..11.5).contains(&x) && (13.0..19.0).contains(&y);
    let cone = (11.5..17.0).contains(&x) && {
        let t = (x - 11.5) / 5.5;
        let half = 3.0 + t * 5.0;
        (y - 16.0).abs() <= half
    };
    // Two arcs centred on the cone mouth, within ±45°.
    let (dx, dy) = (x - 17.0, y - 16.0);
    let dist = (dx * dx + dy * dy).sqrt();
    let in_angle = dx > 0.0 && dy.abs() <= dx;
    let wave = in_angle && ((3.5..5.5).contains(&dist) || (8.0..10.0).contains(&dist));
    body || cone || wave
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_is_sane() {
        assert!(!rounded_square(0.5, 0.5), "corner is transparent");
        assert!(rounded_square(16.0, 16.0), "centre is filled");
        assert!(speaker_glyph(9.0, 16.0), "speaker body");
        assert!(!speaker_glyph(4.0, 4.0), "background");
    }

    #[test]
    fn scales_to_any_size() {
        for size in [16, 32, 256] {
            let px = rgba(BRAND, size);
            assert_eq!(px.len(), size * size * 4);
            assert_eq!(px[3], 0, "top-left corner transparent at {size}px");
        }
    }

    /// `SIMONAIRCAST_ICON_DUMP=<dir> cargo test -p simonaircast icon::tests::dump`
    /// writes raw RGBA for `tools/make_icon.py` and `tools/rgba_to_png.py`.
    #[test]
    fn dump() {
        let Some(dir) = std::env::var_os("SIMONAIRCAST_ICON_DUMP") else {
            return;
        };
        let dir = std::path::Path::new(&dir);
        for (name, color) in [
            ("idle", IDLE),
            ("connecting", CONNECTING),
            ("streaming", STREAMING),
            ("retrying", RETRYING),
        ] {
            std::fs::write(dir.join(format!("{name}.rgba")), rgba(color, 32)).unwrap();
        }
        std::fs::write(dir.join("brand-256.rgba"), rgba(BRAND, 256)).unwrap();
    }
}
