//! Tray icon composition: status badge rendered onto the base bot face.
//!
//! Build a single `IconCache` at startup so the runtime per-state lookup is
//! a cheap `Image::clone()` (slice + dimensions, no pixel work). The base
//! RGBA buffer is leaked once via `Box::leak` so each cached `Image` owns a
//! `'static` reference - this is fine because the cache lives for the
//! whole process lifetime anyway.

use tauri::image::Image;

use crate::state::IconCache;

/// Composite a colored status disc onto a copy of the base RGBA buffer.
///
/// Renders an anti-aliased disc + an outer ring of transparency that gives
/// the pill a subtle "lift" off the bot face. Non-template (set
/// `icon_as_template(false)` on the tray when using the result).
fn compose_with_dot(base_rgba: &[u8], width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let mut pixels = base_rgba.to_vec();
    let w = width as f32;
    let h = height as f32;
    // ~36 % diameter: a chunky, clearly-readable status badge at the 16 px
    // effective menu-bar size on retina. Inspired by Slack / Linear / Things
    // status badges, sized up for legibility.
    let radius = w.min(h) * 0.18;
    // 1 px ring of transparency around the disc to detach it cleanly.
    let ring = (w.min(h) * 0.025).max(1.0);
    let pad = w.min(h) * 0.05 + ring;
    let cx = w - radius - pad;
    let cy = h - radius - pad;
    let r_outer = radius;
    let r_ring_outer = radius + ring;

    for y in 0..height {
        for x in 0..width {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let idx = ((y * width + x) * 4) as usize;

            if dist >= r_ring_outer {
                continue;
            }

            // Coverage of the disc: 1.0 inside, 0.0 outside, smoothed over a
            // 1 px AA band. Coverage of the ring: 1.0 in [r_outer, r_outer+ring].
            let disc_cov = ((r_outer + 0.5 - dist) / 1.0).clamp(0.0, 1.0);
            let ring_cov = if dist > r_outer {
                ((r_ring_outer - dist) / 1.0).clamp(0.0, 1.0) * (1.0 - disc_cov)
            } else {
                0.0
            };

            // Disc: alpha-composite colour over background.
            if disc_cov > 0.0 {
                let a = (disc_cov * 255.0) as u32;
                let inv = 255 - a;
                let bg_a = pixels[idx + 3] as u32;
                pixels[idx] = ((color[0] as u32 * a + pixels[idx] as u32 * inv) / 255) as u8;
                pixels[idx + 1] =
                    ((color[1] as u32 * a + pixels[idx + 1] as u32 * inv) / 255) as u8;
                pixels[idx + 2] =
                    ((color[2] as u32 * a + pixels[idx + 2] as u32 * inv) / 255) as u8;
                pixels[idx + 3] = (a + (bg_a * inv) / 255).min(255) as u8;
            }

            // Ring: erase background alpha to "punch" a clean separation.
            if ring_cov > 0.0 {
                let keep = ((1.0 - ring_cov) * 255.0) as u32;
                pixels[idx + 3] = ((pixels[idx + 3] as u32 * keep) / 255) as u8;
            }
        }
    }

    pixels
}

fn into_static_image(rgba: Vec<u8>, width: u32, height: u32) -> Image<'static> {
    let leaked: &'static [u8] = Box::leak(rgba.into_boxed_slice());
    Image::new(leaked, width, height)
}

/// Build the four pre-rendered status icons (idle is the bare base) and
/// store them in an [`IconCache`] ready for `set_icon` calls.
pub(crate) fn build_icon_cache(base: &Image<'_>) -> IconCache {
    let base_rgba = base.rgba().to_vec();
    let w = base.width();
    let h = base.height();

    // Apple system semantic colours (dark-mode variants, slightly more
    // vivid; they read fine on light menu bars too).
    let orange = [0xFF, 0x9F, 0x0A];
    let green = [0x30, 0xD1, 0x58];
    let blue = [0x0A, 0x84, 0xFF];
    let red = [0xFF, 0x45, 0x3A];

    IconCache {
        idle: into_static_image(base_rgba.clone(), w, h),
        starting: into_static_image(compose_with_dot(&base_rgba, w, h, orange), w, h),
        running_usb: into_static_image(compose_with_dot(&base_rgba, w, h, green), w, h),
        running_sim: into_static_image(compose_with_dot(&base_rgba, w, h, blue), w, h),
        crashed: into_static_image(compose_with_dot(&base_rgba, w, h, red), w, h),
    }
}
