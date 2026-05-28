//! Procedurally-rendered glyphs for tray menu items.
//!
//! We bake small RGBA buffers for the play / stop icons once at first
//! access and cache them in a `OnceLock` for the rest of the process
//! lifetime. Same `Box::leak` trick as `tray_icon.rs` so the resulting
//! [`Image`] owns a `'static` slice and can be cheaply cloned into a
//! fresh `IconMenuItem` on every menu rebuild.
//!
//! Why not [`NativeIcon`]: the macOS-provided `NativeIcon::RightFacing
//! Triangle` is too thin/chevron-shaped at the menu-item size we render
//! at; users read it as a `>` rather than a `▶`. Bundling our own
//! filled-triangle glyph gives us a proper Play-button look and lets us
//! pair it with a matching filled-square Stop glyph for symmetry.
//!
//! Rendering tips:
//!   * The glyphs are pure-black + alpha. macOS treats menu-item icons
//!     as templates by default, so the OS tints them appropriately for
//!     the current appearance (dark/light/highlight).
//!   * 4x supersampling for anti-aliasing - at 22 px the edges of the
//!     triangle would otherwise read as jaggies.

use std::sync::OnceLock;

use tauri::image::Image;

/// Final glyph size in pixels. Picked to match macOS' recommended menu
/// item icon size (16-22 pt; we render at 22 for a touch more
/// presence). Width and height are equal.
const SIZE: u32 = 22;

/// Per-axis supersampling factor used during rasterization. 4x means we
/// evaluate 16 sub-pixels per output pixel - enough to hide jaggies on
/// the diagonal edges of the play triangle without burning real CPU
/// (the glyph is 22x22, the cost is paid exactly once per process).
const SAMPLES: u32 = 4;

struct GlyphCache {
    play: Image<'static>,
    stop: Image<'static>,
}

static CACHE: OnceLock<GlyphCache> = OnceLock::new();

fn cache() -> &'static GlyphCache {
    CACHE.get_or_init(|| GlyphCache {
        play: render_play(),
        stop: render_stop(),
    })
}

/// Filled right-pointing triangle glyph - the "Start daemon" icon.
pub(crate) fn play_icon() -> Image<'static> {
    cache().play.clone()
}

/// Filled rounded-square glyph - the "Stop daemon" icon.
pub(crate) fn stop_icon() -> Image<'static> {
    cache().stop.clone()
}

fn into_static_image(rgba: Vec<u8>, width: u32, height: u32) -> Image<'static> {
    // Leak intentionally: glyphs are reused for the whole process
    // lifetime, freeing them on exit is the OS's job.
    let leaked: &'static [u8] = Box::leak(rgba.into_boxed_slice());
    Image::new(leaked, width, height)
}

fn alloc_buffer() -> Vec<u8> {
    vec![0u8; (SIZE * SIZE * 4) as usize]
}

/// Write a single pixel as (0, 0, 0, alpha) at `(x, y)`. The glyph is
/// monochrome - macOS does the tinting through its template-image
/// pipeline - so all we ever set is the alpha channel.
fn put_alpha(pixels: &mut [u8], x: u32, y: u32, alpha: u8) {
    let idx = ((y * SIZE + x) * 4) as usize;
    pixels[idx] = 0;
    pixels[idx + 1] = 0;
    pixels[idx + 2] = 0;
    pixels[idx + 3] = alpha;
}

/// Render a filled isosceles triangle pointing right.
///
/// Geometry: two vertices on the left edge (top and bottom), one on the
/// right edge (vertically centered). The triangle is inset by `pad_h`
/// horizontally and `pad_v` vertically to leave breathing room around
/// the glyph.
///
/// Anti-aliasing is done with `SAMPLES x SAMPLES` supersampling: for
/// each output pixel we count how many sub-pixel samples fall inside
/// the triangle and write that as the alpha value.
fn render_play() -> Image<'static> {
    let mut pixels = alloc_buffer();
    let w = SIZE as f32;
    let h = SIZE as f32;
    // The visual weight of a play triangle reads heavier than its
    // bounding box, so the horizontal inset is slightly bigger than
    // the vertical one - keeps the glyph centred optically rather
    // than mathematically.
    let pad_h = w * 0.22;
    let pad_v = h * 0.15;
    let left = pad_h;
    let right = w - pad_h;
    let top = pad_v;
    let bottom = h - pad_v;
    let mid_y = (top + bottom) * 0.5;
    // Slope of the upper and lower edges going from the left side to
    // the right vertex (mirror of each other across mid_y).
    let slope = (mid_y - top) / (right - left);

    let step = 1.0 / SAMPLES as f32;
    let total = (SAMPLES * SAMPLES) as f32;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut hits = 0u32;
            for sy in 0..SAMPLES {
                let fy = y as f32 + (sy as f32 + 0.5) * step;
                for sx in 0..SAMPLES {
                    let fx = x as f32 + (sx as f32 + 0.5) * step;
                    if fx < left || fx > right {
                        continue;
                    }
                    let upper_bound = top + (fx - left) * slope;
                    let lower_bound = bottom - (fx - left) * slope;
                    if fy >= upper_bound && fy <= lower_bound {
                        hits += 1;
                    }
                }
            }
            if hits == 0 {
                continue;
            }
            let alpha = ((hits as f32 / total) * 255.0) as u8;
            put_alpha(&mut pixels, x, y, alpha);
        }
    }

    into_static_image(pixels, SIZE, SIZE)
}

/// Render a filled rounded square. Visual weight matches the play
/// triangle (the two glyphs swap in and out of the same toggle row, so
/// they should feel like one family).
fn render_stop() -> Image<'static> {
    let mut pixels = alloc_buffer();
    let w = SIZE as f32;
    let h = SIZE as f32;
    let pad = w * 0.22;
    let left = pad;
    let right = w - pad;
    let top = pad;
    let bottom = h - pad;
    // Corner radius ~14 % of the inner side, matching the SF Symbols
    // "stop.fill" rounding.
    let radius = (right - left).min(bottom - top) * 0.18;

    let step = 1.0 / SAMPLES as f32;
    let total = (SAMPLES * SAMPLES) as f32;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut hits = 0u32;
            for sy in 0..SAMPLES {
                let fy = y as f32 + (sy as f32 + 0.5) * step;
                for sx in 0..SAMPLES {
                    let fx = x as f32 + (sx as f32 + 0.5) * step;
                    if fx < left || fx > right || fy < top || fy > bottom {
                        continue;
                    }
                    // Inside the bounding rectangle. Check the four
                    // corner discs by computing the distance to the
                    // nearest corner centre when we're inside the
                    // corner inset square.
                    let dx = if fx < left + radius {
                        left + radius - fx
                    } else if fx > right - radius {
                        fx - (right - radius)
                    } else {
                        0.0
                    };
                    let dy = if fy < top + radius {
                        top + radius - fy
                    } else if fy > bottom - radius {
                        fy - (bottom - radius)
                    } else {
                        0.0
                    };
                    if dx * dx + dy * dy <= radius * radius {
                        hits += 1;
                    }
                }
            }
            if hits == 0 {
                continue;
            }
            let alpha = ((hits as f32 / total) * 255.0) as u8;
            put_alpha(&mut pixels, x, y, alpha);
        }
    }

    into_static_image(pixels, SIZE, SIZE)
}
