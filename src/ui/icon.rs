//! The HushKey icon, rasterised at runtime (no image assets, no decoder dependency).
//!
//! A rounded square with a speaker glyph: violet with sound waves when audio is live,
//! coral with an × when muted. 4×4 supersampling gives clean anti-aliased edges at
//! tray sizes (16–32 px) and window-icon sizes.
//!
//! Std-only on purpose: `build.rs` includes this file to embed the same icon into the
//! Windows executable, so it must not depend on any crate.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Normal,
    Muted,
}

pub fn render(size: u32, state: IconState) -> Vec<u8> {
    let (top, bottom) = match state {
        IconState::Normal => ([0x8a, 0x7b, 0xff], [0x5b, 0x49, 0xf2]),
        IconState::Muted => ([0xff, 0x6b, 0x78], [0xe0, 0x40, 0x5a]),
    };
    const SS: u32 = 4;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let mut bg_cov = 0.0f32;
            let mut fg_cov = 0.0f32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if in_rounded_rect(x, y, 0.02, 0.22) {
                        bg_cov += 1.0;
                        if in_glyph(x, y, state) {
                            fg_cov += 1.0;
                        }
                    }
                }
            }
            let n = (SS * SS) as f32;
            let (bg_a, fg_a) = (bg_cov / n, fg_cov / n);
            let t = py as f32 / size as f32;
            let base = [0, 1, 2].map(|i| top[i] as f32 * (1.0 - t) + bottom[i] as f32 * t);
            let i = ((py * size + px) * 4) as usize;
            if bg_a > 0.0 {
                // White glyph over gradient (fg coverage is a fraction of bg coverage).
                let mix = if bg_a > 0.0 { fg_a / bg_a } else { 0.0 };
                for c in 0..3 {
                    out[i + c] = (base[c] * (1.0 - mix) + 255.0 * mix).round() as u8;
                }
                out[i + 3] = (bg_a * 255.0).round() as u8;
            }
        }
    }
    out
}

fn in_rounded_rect(x: f32, y: f32, inset: f32, r: f32) -> bool {
    let (lo, hi) = (inset, 1.0 - inset);
    if x < lo || x > hi || y < lo || y > hi {
        return false;
    }
    let cx = x.clamp(lo + r, hi - r);
    let cy = y.clamp(lo + r, hi - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn in_glyph(x: f32, y: f32, state: IconState) -> bool {
    // Speaker body + cone.
    if (0.20..=0.34).contains(&x) && (0.40..=0.60).contains(&y) {
        return true;
    }
    if in_convex(x, y, &[(0.32, 0.40), (0.50, 0.24), (0.50, 0.76), (0.32, 0.60)]) {
        return true;
    }
    match state {
        IconState::Normal => {
            let (dx, dy) = (x - 0.50, y - 0.50);
            let d = (dx * dx + dy * dy).sqrt();
            let angle_ok = dx > 0.0 && dy.abs() <= dx * 1.2;
            angle_ok && ((0.115..=0.175).contains(&d) || (0.235..=0.295).contains(&d))
        }
        IconState::Muted => {
            seg_dist(x, y, (0.60, 0.39), (0.80, 0.61)) <= 0.035 || seg_dist(x, y, (0.60, 0.61), (0.80, 0.39)) <= 0.035
        }
    }
}

fn in_convex(x: f32, y: f32, pts: &[(f32, f32)]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..pts.len() {
        let (ax, ay) = pts[i];
        let (bx, by) = pts[(i + 1) % pts.len()];
        let cross = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        if cross != 0.0 {
            if sign != 0.0 && cross.signum() != sign {
                return false;
            }
            sign = cross.signum();
        }
    }
    true
}

fn seg_dist(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * vx + (y - a.1) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
    let (px, py) = (a.0 + t * vx, a.1 + t * vy);
    ((x - px).powi(2) + (y - py).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_opaque_center_and_transparent_corner() {
        let px = render(32, IconState::Normal);
        assert_eq!(px.len(), 32 * 32 * 4);
        assert_eq!(px[3], 0, "top-left corner is outside the rounded rect");
        let center = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(px[center + 3], 255);
    }

    #[test]
    fn states_differ() {
        assert_ne!(render(24, IconState::Normal), render(24, IconState::Muted));
    }
}
