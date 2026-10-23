//! The **game cursor** -- a large, high-contrast pointer GameFrame draws for
//! tracked games whose own cursor is easy to lose (see `CONTEXT.md`).
//!
//! Pure half of the feature, split the same way as [`crate::zone_editor`] /
//! [`crate::zone_overlay`]:
//! - [`CursorSettings`] (persisted in [`crate::profile_store::Config`]),
//!   the software rasterizer that turns them into a premultiplied RGBA
//!   [`Bitmap`] ([`render_pointer`], [`draw_pulse_ring`]), the "find my
//!   cursor" pulse animation ([`pulse_frame`]), shake detection
//!   ([`ShakeDetector`]) and the show/hide decision ([`should_show`]) --
//!   no OS calls, fully unit tested below.
//! - [`crate::cursor_overlay`] is the OS-facing half: the click-through
//!   layered window that follows the mouse.
//!
//! Every shape is described by a signed distance function (negative inside,
//! in pixels, relative to the hotspot), which makes the border (the shape
//! grown by `border_width`), the glow (a falloff outside the shape) and
//! one-pixel anti-aliasing all fall out of the same number.

use std::time::Duration;

/// The pointer variants a user can pick. The hotspot (where the real
/// cursor position lands) is the arrow's tip for [`CursorShape::Arrow`] and
/// the center for every other drawn shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CursorShape {
    /// Builds on the game's own cursor instead of drawing a separate one: an
    /// enlarged copy of its current image behind it, with the border and
    /// glow following its outline (see [`render_game_own`]). A game's own
    /// cursor is a hardware cursor Windows draws above every window, so it
    /// stays on top as the core of the enlarged one rather than fighting a
    /// different shape. Outside the game (the Settings previews) the arrow
    /// stands in for it.
    GameOwn,
    #[default]
    Ring,
    RingDot,
    Arrow,
    Crosshair,
    Reticle,
    Disc,
    Dot,
}

impl CursorShape {
    pub const ALL: [CursorShape; 8] = [
        CursorShape::GameOwn,
        CursorShape::Ring,
        CursorShape::RingDot,
        CursorShape::Arrow,
        CursorShape::Crosshair,
        CursorShape::Reticle,
        CursorShape::Disc,
        CursorShape::Dot,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CursorShape::GameOwn => "Game's own",
            CursorShape::Ring => "Ring",
            CursorShape::RingDot => "Ring + dot",
            CursorShape::Arrow => "Arrow",
            CursorShape::Crosshair => "Crosshair",
            CursorShape::Reticle => "Reticle",
            CursorShape::Disc => "Disc",
            CursorShape::Dot => "Dot",
        }
    }

    /// Whether `thickness` changes this shape (the filled shapes ignore it).
    pub fn uses_thickness(self) -> bool {
        !matches!(self, CursorShape::Arrow | CursorShape::Dot | CursorShape::GameOwn)
    }
}

/// The persisted game-cursor settings (config table `[cursor]`). Every field
/// has a default, so a config written before this feature existed -- or one
/// missing any single field -- loads unchanged. Unknown keys are ignored, so
/// configs with the since-removed `method`/`scope` keys load too.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CursorSettings {
    pub enabled: bool,
    pub shape: CursorShape,
    /// Overall size in physical pixels: the diameter of round shapes, the
    /// height of the arrow.
    pub size: u32,
    /// Stroke width of the line shapes (see [`CursorShape::uses_thickness`]).
    pub thickness: u32,
    /// sRGB.
    pub color: [u8; 3],
    /// 10..=100 percent; scales every layer, glow and border included.
    pub opacity: u8,
    pub border: bool,
    pub border_color: [u8; 3],
    pub border_width: u32,
    pub glow: bool,
    pub glow_color: [u8; 3],
    /// How far the glow reaches beyond the shape (and its border), pixels.
    pub glow_size: u32,
    /// 0..=100 percent peak glow opacity.
    pub glow_strength: u8,
    /// Shaking the mouse plays the "find my cursor" pulse.
    pub shake_to_find: bool,
    /// Hide the game cursor whenever the game hides the real one (e.g. while
    /// it captures the mouse for camera control). Off by default: a game
    /// that draws its own software cursor hides the real one permanently.
    pub hide_when_cursor_hidden: bool,
}

impl Default for CursorSettings {
    fn default() -> Self {
        CursorSettings {
            enabled: false,
            shape: CursorShape::default(),
            size: 48,
            thickness: 4,
            color: [0, 230, 255],
            opacity: 90,
            border: true,
            border_color: [0, 0, 0],
            border_width: 2,
            glow: true,
            glow_color: [0, 230, 255],
            glow_size: 12,
            glow_strength: 60,
            shake_to_find: true,
            hide_when_cursor_hidden: false,
        }
    }
}

pub const SIZE_RANGE: std::ops::RangeInclusive<u32> = 12..=160;
pub const THICKNESS_RANGE: std::ops::RangeInclusive<u32> = 1..=16;
pub const OPACITY_RANGE: std::ops::RangeInclusive<u8> = 10..=100;
pub const BORDER_WIDTH_RANGE: std::ops::RangeInclusive<u32> = 1..=8;
pub const GLOW_SIZE_RANGE: std::ops::RangeInclusive<u32> = 2..=48;
pub const GLOW_STRENGTH_RANGE: std::ops::RangeInclusive<u8> = 0..=100;

impl CursorSettings {
    /// Whether `color` changes the pointer ([`CursorShape::GameOwn`] keeps the
    /// game's own colors).
    pub fn uses_color(&self) -> bool {
        self.shape != CursorShape::GameOwn
    }

    /// A copy with every numeric field clamped into the range Settings
    /// offers, so a hand-edited config can't produce a zero-sized or
    /// screen-filling canvas.
    pub fn sanitized(&self) -> Self {
        let mut s = self.clone();
        s.size = s.size.clamp(*SIZE_RANGE.start(), *SIZE_RANGE.end());
        s.thickness = s
            .thickness
            .clamp(*THICKNESS_RANGE.start(), *THICKNESS_RANGE.end());
        s.opacity = s
            .opacity
            .clamp(*OPACITY_RANGE.start(), *OPACITY_RANGE.end());
        s.border_width = s
            .border_width
            .clamp(*BORDER_WIDTH_RANGE.start(), *BORDER_WIDTH_RANGE.end());
        s.glow_size = s
            .glow_size
            .clamp(*GLOW_SIZE_RANGE.start(), *GLOW_SIZE_RANGE.end());
        s.glow_strength = s
            .glow_strength
            .clamp(*GLOW_STRENGTH_RANGE.start(), *GLOW_STRENGTH_RANGE.end());
        s
    }
}

/// Decides whether the game cursor should be on screen right now.
/// `over_game` is whether the window under the real cursor belongs to a
/// tracked game with its per-game "Game cursor" flag on -- like YoloMouse,
/// the game cursor exists only in and over the game, never over other
/// windows (whichever window has focus).
pub fn should_show(settings: &CursorSettings, over_game: bool, cursor_showing: bool) -> bool {
    settings.enabled && over_game && (!settings.hide_when_cursor_hidden || cursor_showing)
}

/// A premultiplied-alpha RGBA image with the pixel the real cursor position
/// maps to (`hotspot_x`, `hotspot_y`).
#[derive(Debug, Clone, PartialEq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    /// `width * height * 4` bytes, row-major, premultiplied RGBA.
    pub pixels: Vec<u8>,
}

impl Bitmap {
    pub fn blank(width: u32, height: u32, hotspot_x: i32, hotspot_y: i32) -> Self {
        Bitmap {
            width,
            height,
            hotspot_x,
            hotspot_y,
            pixels: vec![0; (width * height * 4) as usize],
        }
    }

    #[cfg(test)]
    pub fn alpha_at(&self, x: u32, y: u32) -> u8 {
        self.pixels[((y * self.width + x) * 4 + 3) as usize]
    }

}

/// The arrow outline in units of its height, tip at the origin, y down --
/// proportions of the classic Windows arrow.
const ARROW: [(f32, f32); 7] = [
    (0.0, 0.0),
    (0.0, 0.80),
    (0.19, 0.63),
    (0.32, 0.94),
    (0.45, 0.88),
    (0.32, 0.58),
    (0.57, 0.58),
];

/// The largest radius the find-my-cursor pulse ring starts at.
pub fn pulse_max_radius(settings: &CursorSettings) -> f32 {
    (settings.size as f32 * 2.5).clamp(110.0, 280.0)
}

/// How far the shape's bounding box extends from the hotspot:
/// `(min_x, min_y, max_x, max_y)`, before border/glow.
fn shape_bounds(settings: &CursorSettings) -> (f32, f32, f32, f32) {
    let s = settings.size as f32;
    match settings.shape {
        CursorShape::Arrow | CursorShape::GameOwn => (0.0, 0.0, 0.57 * s, 0.94 * s),
        _ => (-s / 2.0, -s / 2.0, s / 2.0, s / 2.0),
    }
}

/// Extra room the border and glow need around the shape.
fn decoration_margin(settings: &CursorSettings) -> f32 {
    let border = if settings.border {
        settings.border_width as f32
    } else {
        0.0
    };
    let glow = if settings.glow {
        settings.glow_size as f32
    } else {
        0.0
    };
    border + glow + 2.0
}

/// The canvas for the overlay window: centered on the hotspot and large
/// enough for both the pointer and the biggest pulse ring, so the window
/// never has to resize while the pulse plays. Returns `(side, hotspot)`.
pub fn overlay_canvas(settings: &CursorSettings) -> (u32, i32) {
    let (min_x, min_y, max_x, max_y) = shape_bounds(settings);
    let reach = [min_x.abs(), min_y.abs(), max_x, max_y]
        .into_iter()
        .fold(0.0f32, f32::max);
    let pulse = pulse_max_radius(settings) + settings.thickness.max(3) as f32 + 4.0;
    let half = (reach + decoration_margin(settings)).max(pulse).ceil() as i32;
    ((half * 2) as u32, half)
}

/// The tight canvas for the pointer alone (the overlay outside a pulse, and
/// the Settings previews): just the shape plus its border/glow. Returns `(width, height, hotspot_x, hotspot_y)`.
pub fn cursor_canvas(settings: &CursorSettings) -> (u32, u32, i32, i32) {
    let (min_x, min_y, max_x, max_y) = shape_bounds(settings);
    let m = decoration_margin(settings);
    let left = (-min_x + m).ceil() as i32;
    let top = (-min_y + m).ceil() as i32;
    let width = (left + (max_x + m).ceil() as i32) as u32;
    let height = (top + (max_y + m).ceil() as i32) as u32;
    (width, height, left, top)
}

/// Renders the pointer described by `settings` onto a transparent canvas of
/// `width` x `height` with the hotspot at (`hotspot_x`, `hotspot_y`).
pub fn render_pointer(
    settings: &CursorSettings,
    width: u32,
    height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
) -> Bitmap {
    let s = settings.sanitized();
    let mut bitmap = Bitmap::blank(width, height, hotspot_x, hotspot_y);
    let opacity = s.opacity as f32 / 100.0;
    let border_w = if s.border { s.border_width as f32 } else { 0.0 };
    let glow_strength = if s.glow {
        s.glow_strength as f32 / 100.0
    } else {
        0.0
    };
    let glow_size = s.glow_size as f32;

    for y in 0..height {
        for x in 0..width {
            // Sample at the pixel center.
            let px = x as f32 + 0.5 - hotspot_x as f32;
            let py = y as f32 + 0.5 - hotspot_y as f32;
            let (sd, fill_sd) = shape_sdf(&s, px, py);

            let mut out = [0.0f32; 4];
            if glow_strength > 0.0 {
                let outside = (sd - border_w).max(0.0);
                let falloff = (1.0 - outside / glow_size).clamp(0.0, 1.0);
                let a = glow_strength * falloff * falloff * opacity;
                blend_over(&mut out, s.glow_color, a);
            }
            if border_w > 0.0 {
                blend_over(&mut out, s.border_color, coverage(sd - border_w) * opacity);
            }
            if let Some(fill_sd) = fill_sd {
                blend_over(
                    &mut out,
                    s.color,
                    coverage(fill_sd) * DISC_FILL_ALPHA * opacity,
                );
            }
            blend_over(&mut out, s.color, coverage(sd) * opacity);
            write_pixel(&mut bitmap, x, y, out);
        }
    }
    bitmap
}

/// Interior opacity of [`CursorShape::Disc`] relative to its rim.
const DISC_FILL_ALPHA: f32 = 0.35;

/// Draws the find-my-cursor pulse ring of `radius` around the hotspot on top
/// of `bitmap`, at `alpha` (0..=1) times the configured opacity.
pub fn draw_pulse_ring(bitmap: &mut Bitmap, settings: &CursorSettings, radius: f32, alpha: f32) {
    let s = settings.sanitized();
    let half_t = s.thickness.max(3) as f32 / 2.0;
    let border_w = if s.border { s.border_width as f32 } else { 0.0 };
    let a = alpha.clamp(0.0, 1.0) * s.opacity as f32 / 100.0;
    // Only rows/columns the ring can touch need visiting.
    let reach = radius + half_t + border_w + 1.0;
    let (hx, hy) = (bitmap.hotspot_x as f32, bitmap.hotspot_y as f32);
    let x0 = (hx - reach).floor().max(0.0) as u32;
    let y0 = (hy - reach).floor().max(0.0) as u32;
    let x1 = ((hx + reach).ceil() as u32).min(bitmap.width);
    let y1 = ((hy + reach).ceil() as u32).min(bitmap.height);
    for y in y0..y1 {
        for x in x0..x1 {
            let px = x as f32 + 0.5 - hx;
            let py = y as f32 + 0.5 - hy;
            let sd = ((px * px + py * py).sqrt() - radius).abs() - half_t;
            if sd - border_w > 1.0 {
                continue;
            }
            let mut out = read_pixel(bitmap, x, y);
            if border_w > 0.0 {
                blend_over(&mut out, s.border_color, coverage(sd - border_w) * a);
            }
            blend_over(&mut out, s.color, coverage(sd) * a);
            write_pixel(bitmap, x, y, out);
        }
    }
}

/// Builds a premultiplied [`Bitmap`] from a cursor image as Windows hands it
/// over: `bgra` is 32-bit top-down BGRA with straight alpha. Cursors without
/// an alpha channel (every alpha byte 0) take their transparency from
/// `and_mask` (same layout; a set pixel is transparent) -- that also covers
/// monochrome cursors, passed with their XOR half as `bgra`.
pub fn cursor_image(
    width: u32,
    height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    bgra: &[u8],
    and_mask: Option<&[u8]>,
) -> Bitmap {
    let mut image = Bitmap::blank(width, height, hotspot_x, hotspot_y);
    let n = ((width * height) as usize).min(bgra.len() / 4);
    let has_alpha = bgra[..n * 4].chunks_exact(4).any(|p| p[3] != 0);
    for i in 0..n {
        let p = &bgra[i * 4..i * 4 + 4];
        let alpha = if has_alpha {
            p[3]
        } else {
            match and_mask.and_then(|m| m.get(i * 4)) {
                Some(&and) if and != 0 => 0,
                _ => 255,
            }
        };
        let premultiply = |c: u8| ((c as u32 * alpha as u32 + 127) / 255) as u8;
        image.pixels[i * 4..i * 4 + 4]
            .copy_from_slice(&[premultiply(p[2]), premultiply(p[1]), premultiply(p[0]), alpha]);
    }
    image
}

/// Where in its hotspot pixel [`render_game_own`] enlarges the game's cursor
/// around, as a fraction of a pixel right and down from the pixel's top-left
/// corner -- here its bottom-right corner, so the enlarged hotspot pixel
/// ends where the real one does.
///
/// For an arrow whose tip is the hotspot (Last Epoch: a 1-pixel dark edge
/// along its left and top), that puts the game's cursor on the inside of the
/// enlarged copy's edge, like the tip of a bigger pen, on the left and top
/// as on the right; the copy reaches `scale - 1` pixels past those edges.
/// Tried in game: the top-left corner (0.0) kept every edge through it in
/// place, so the game's cursor sat on the copy's left edge; the center (0.5)
/// was better but still one step short.
const TIP_PIVOT: f32 = 1.0;

/// Renders [`CursorShape::GameOwn`] around the game's current cursor
/// `image` (from [`cursor_image`]): the image enlarged to `size` pixels tall
/// (never shrunk -- at or below its own height only the decoration is drawn,
/// since the real cursor already shows the image), with the border and glow
/// following its outline. Both stay strictly outside the outline, so the
/// game's cursor on top is never tinted.
///
/// The image is enlarged around [`TIP_PIVOT`] in its hotspot pixel, which
/// lands on the same point of the canvas hotspot pixel.
pub fn render_game_own(settings: &CursorSettings, image: &Bitmap) -> Bitmap {
    let s = settings.sanitized();
    let scale = (s.size as f32 / image.height.max(1) as f32).max(1.0);
    // Room for the decoration plus bilinear magnification's soft edge.
    let pad = decoration_margin(&s).ceil() as i32 + (scale / 2.0).ceil() as i32;
    // Per axis: the canvas hotspot pixel and the canvas length.
    let axis = |len: u32, hot: i32| {
        let pivot = hot as f32 + TIP_PIVOT;
        let canvas_hot = pad + (pivot * scale - TIP_PIVOT).ceil() as i32;
        let end = canvas_hot as f32 + TIP_PIVOT + (len as f32 - pivot) * scale;
        (canvas_hot, (end.ceil() as i32 + pad) as u32)
    };
    let (hotspot_x, width) = axis(image.width, image.hotspot_x);
    let (hotspot_y, height) = axis(image.height, image.hotspot_y);
    let mut bitmap = Bitmap::blank(width, height, hotspot_x, hotspot_y);

    // Canvas pixel center -> image texel-center coordinates, the pivots
    // aligned.
    let to_image = |canvas: u32, canvas_hot: i32, hot: i32| {
        let offset = canvas as f32 + 0.5 - (canvas_hot as f32 + TIP_PIVOT);
        hot as f32 + TIP_PIVOT + offset / scale - 0.5
    };
    let copy: Vec<[f32; 4]> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .map(|(x, y)| {
            let ix = to_image(x, hotspot_x, image.hotspot_x);
            let iy = to_image(y, hotspot_y, image.hotspot_y);
            sample_bilinear(image, ix, iy)
        })
        .collect();
    let inside: Vec<bool> = copy.iter().map(|p| p[3] >= 0.5).collect();
    let distance2 = distance_transform(&inside, width, height);

    let opacity = s.opacity as f32 / 100.0;
    let border_w = if s.border { s.border_width as f32 } else { 0.0 };
    let glow_strength = if s.glow {
        s.glow_strength as f32 / 100.0
    } else {
        0.0
    };
    let glow_size = s.glow_size as f32;
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            let mut out = [0.0f32; 4];
            if !inside[i] {
                // Distance from this pixel's edge to the outline.
                let sd = distance2[i].sqrt() - 0.5;
                if glow_strength > 0.0 {
                    let outside = (sd - border_w).max(0.0);
                    let falloff = (1.0 - outside / glow_size).clamp(0.0, 1.0);
                    blend_over(&mut out, s.glow_color, glow_strength * falloff * falloff * opacity);
                }
                if border_w > 0.0 {
                    blend_over(&mut out, s.border_color, coverage(sd - border_w) * opacity);
                }
            }
            if scale > 1.0 {
                let src = copy[i].map(|c| c * opacity);
                let inv = 1.0 - src[3];
                for c in 0..4 {
                    out[c] = src[c] + out[c] * inv;
                }
            }
            write_pixel(&mut bitmap, x, y, out);
        }
    }
    bitmap
}

/// `pointer` centered (by hotspot) on a square canvas big enough for it and
/// for the largest pulse ring -- the pulse base for a pointer that wasn't
/// rendered from the settings alone ([`CursorShape::GameOwn`]).
pub fn pulse_canvas_for(settings: &CursorSettings, pointer: &Bitmap) -> Bitmap {
    let (_, pulse_half) = overlay_canvas(settings);
    let reach = [
        pointer.hotspot_x,
        pointer.hotspot_y,
        pointer.width as i32 - pointer.hotspot_x,
        pointer.height as i32 - pointer.hotspot_y,
    ]
    .into_iter()
    .fold(pulse_half, i32::max);
    let side = (reach * 2) as u32;
    let mut canvas = Bitmap::blank(side, side, reach, reach);
    let (dx, dy) = (reach - pointer.hotspot_x, reach - pointer.hotspot_y);
    for y in 0..pointer.height {
        let src = (y * pointer.width * 4) as usize;
        let dst = (((y as i32 + dy) as u32 * side + dx as u32) * 4) as usize;
        let row = (pointer.width * 4) as usize;
        canvas.pixels[dst..dst + row].copy_from_slice(&pointer.pixels[src..src + row]);
    }
    canvas
}

/// The pointer as the Settings previews show it: [`render_pointer`] on its
/// tight canvas, or for [`CursorShape::GameOwn`] what the game shows --
/// [`render_game_own`] with the game's own cursor on top -- built on
/// `game_cursor` (the last one read from a game) or, before any was,
/// [`sample_game_cursor`].
pub fn render_preview(settings: &CursorSettings, game_cursor: Option<&Bitmap>) -> Bitmap {
    if settings.shape == CursorShape::GameOwn {
        let sample;
        let image = match game_cursor {
            Some(image) => image,
            None => {
                sample = sample_game_cursor();
                &sample
            }
        };
        let mut out = render_game_own(settings, image);
        // The real cursor, drawn by Windows above the overlay in game.
        let (dx, dy) = (out.hotspot_x - image.hotspot_x, out.hotspot_y - image.hotspot_y);
        for y in 0..image.height {
            for x in 0..image.width {
                let (tx, ty) = (x as i32 + dx, y as i32 + dy);
                if tx < 0 || ty < 0 || tx >= out.width as i32 || ty >= out.height as i32 {
                    continue;
                }
                let src = read_pixel(image, x, y);
                let mut dst = read_pixel(&out, tx as u32, ty as u32);
                let inv = 1.0 - src[3];
                for c in 0..4 {
                    dst[c] = src[c] + dst[c] * inv;
                }
                write_pixel(&mut out, tx as u32, ty as u32, dst);
            }
        }
        return out;
    }
    let (w, h, hx, hy) = cursor_canvas(settings);
    render_pointer(settings, w, h, hx, hy)
}

/// A stand-in for a game's own cursor in the previews: the classic white
/// arrow with a black edge at a typical 32-pixel cursor size.
fn sample_game_cursor() -> Bitmap {
    let arrow = CursorSettings {
        shape: CursorShape::Arrow,
        size: 28,
        color: [255, 255, 255],
        opacity: 100,
        border: true,
        border_color: [0, 0, 0],
        border_width: 1,
        glow: false,
        ..CursorSettings::default()
    };
    let (w, h, hx, hy) = cursor_canvas(&arrow);
    render_pointer(&arrow, w, h, hx, hy)
}

/// Bilinear sample of a premultiplied image at pixel-center coordinates
/// (`x`, `y`) as 0..=1 components; transparent outside the image.
fn sample_bilinear(image: &Bitmap, x: f32, y: f32) -> [f32; 4] {
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let texel = |tx: i32, ty: i32| -> [f32; 4] {
        if tx < 0 || ty < 0 || tx >= image.width as i32 || ty >= image.height as i32 {
            return [0.0; 4];
        }
        read_pixel(image, tx as u32, ty as u32)
    };
    let (x0, y0) = (x0 as i32, y0 as i32);
    let (a, b, c, d) = (texel(x0, y0), texel(x0 + 1, y0), texel(x0, y0 + 1), texel(x0 + 1, y0 + 1));
    std::array::from_fn(|k| {
        let top = a[k] + (b[k] - a[k]) * fx;
        let bottom = c[k] + (d[k] - c[k]) * fx;
        top + (bottom - top) * fy
    })
}

/// Squared Euclidean distance from each pixel to the nearest `inside` pixel
/// (0 for inside pixels) -- Felzenszwalb & Huttenlocher's linear-time
/// two-pass transform, columns then rows.
fn distance_transform(inside: &[bool], width: u32, height: u32) -> Vec<f32> {
    const FAR: f32 = 1.0e12;
    let (w, h) = (width as usize, height as usize);
    let mut grid: Vec<f32> = inside.iter().map(|&i| if i { 0.0 } else { FAR }).collect();
    let mut f = vec![0.0f32; w.max(h)];
    let mut d = vec![0.0f32; w.max(h)];
    for x in 0..w {
        for y in 0..h {
            f[y] = grid[y * w + x];
        }
        distance_transform_1d(&f[..h], &mut d[..h]);
        for y in 0..h {
            grid[y * w + x] = d[y];
        }
    }
    for y in 0..h {
        f[..w].copy_from_slice(&grid[y * w..(y + 1) * w]);
        distance_transform_1d(&f[..w], &mut d[..w]);
        grid[y * w..(y + 1) * w].copy_from_slice(&d[..w]);
    }
    grid
}

fn distance_transform_1d(f: &[f32], d: &mut [f32]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    // Where parabola `q` overtakes parabola `p` (f64: `f` holds huge values
    // for "no inside pixel yet", which would swamp f32 precision).
    let intersect = |q: usize, p: usize| {
        let (fq, fp) = (f[q] as f64, f[p] as f64);
        ((fq + (q * q) as f64) - (fp + (p * p) as f64)) / (2.0 * (q - p) as f64)
    };
    let mut v = vec![0usize; n];
    let mut z = vec![0.0f64; n + 1];
    let mut k = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    for q in 1..n {
        // Terminates: nothing is <= z[0] = -inf.
        let mut s = intersect(q, v[k]);
        while s <= z[k] {
            k -= 1;
            s = intersect(q, v[k]);
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    k = 0;
    for (q, out) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let p = v[k];
        let dq = q as f32 - p as f32;
        *out = dq * dq + f[p];
    }
}

/// The main shape's signed distance at (`px`, `py`) relative to the hotspot,
/// plus [`CursorShape::Disc`]'s separate interior-fill distance.
fn shape_sdf(s: &CursorSettings, px: f32, py: f32) -> (f32, Option<f32>) {
    let size = s.size as f32;
    let half_t = s.thickness as f32 / 2.0;
    let d = (px * px + py * py).sqrt();
    // Ring centerline radius such that the ring's outer edge sits at size/2.
    let ring_r = (size / 2.0 - half_t).max(half_t);
    let ring = (d - ring_r).abs() - half_t;
    match s.shape {
        CursorShape::Ring => (ring, None),
        CursorShape::RingDot => {
            let dot_r = (size * 0.08).max(half_t).max(1.5);
            (ring.min(d - dot_r), None)
        }
        CursorShape::Disc => (ring, Some(d - ring_r)),
        CursorShape::Dot => (d - size / 2.0, None),
        CursorShape::Crosshair => {
            let gap = (size * 0.18).max(3.0);
            let end = size / 2.0 - half_t;
            let arms = [
                capsule(px, py, (gap, 0.0), (end, 0.0)),
                capsule(px, py, (-gap, 0.0), (-end, 0.0)),
                capsule(px, py, (0.0, gap), (0.0, end)),
                capsule(px, py, (0.0, -gap), (0.0, -end)),
            ];
            let arm = arms.into_iter().fold(f32::MAX, f32::min) - half_t;
            let dot = d - half_t.max(1.5);
            (arm.min(dot), None)
        }
        CursorShape::Reticle => {
            let inner = ring_r * 0.45;
            let ticks = [
                capsule(px, py, (inner, 0.0), (ring_r, 0.0)),
                capsule(px, py, (-inner, 0.0), (-ring_r, 0.0)),
                capsule(px, py, (0.0, inner), (0.0, ring_r)),
                capsule(px, py, (0.0, -inner), (0.0, -ring_r)),
            ];
            let tick = ticks.into_iter().fold(f32::MAX, f32::min) - half_t;
            let dot = d - (size * 0.05).max(1.5);
            (ring.min(tick).min(dot), None)
        }
        // The arrow stands in for the game's own cursor where there is none.
        CursorShape::Arrow | CursorShape::GameOwn => {
            let points = ARROW.map(|(x, y)| (x * size, y * size));
            (polygon_sdf(&points, px, py), None)
        }
    }
}

/// Distance from (`px`, `py`) to the segment `a`-`b` (not including any
/// stroke width).
fn capsule(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (wx, wy) = (px - a.0, py - a.1);
    let (ex, ey) = (b.0 - a.0, b.1 - a.1);
    let len2 = ex * ex + ey * ey;
    let t = if len2 > 0.0 {
        ((wx * ex + wy * ey) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (dx, dy) = (wx - ex * t, wy - ey * t);
    (dx * dx + dy * dy).sqrt()
}

/// Signed distance to a simple polygon (negative inside) -- the
/// even-odd/winding formulation from Inigo Quilez's 2D SDF catalogue.
fn polygon_sdf(v: &[(f32, f32)], px: f32, py: f32) -> f32 {
    let mut d = (px - v[0].0).powi(2) + (py - v[0].1).powi(2);
    let mut sign = 1.0;
    let mut j = v.len() - 1;
    for i in 0..v.len() {
        let (ex, ey) = (v[j].0 - v[i].0, v[j].1 - v[i].1);
        let (wx, wy) = (px - v[i].0, py - v[i].1);
        let t = ((wx * ex + wy * ey) / (ex * ex + ey * ey)).clamp(0.0, 1.0);
        let (bx, by) = (wx - ex * t, wy - ey * t);
        d = d.min(bx * bx + by * by);
        let c1 = py >= v[i].1;
        let c2 = py < v[j].1;
        let c3 = ex * wy > ey * wx;
        if (c1 && c2 && c3) || (!c1 && !c2 && !c3) {
            sign = -sign;
        }
        j = i;
    }
    sign * d.sqrt()
}

/// One-pixel anti-aliased coverage of a signed distance.
fn coverage(sd: f32) -> f32 {
    (0.5 - sd).clamp(0.0, 1.0)
}

/// Porter-Duff "over" of a solid `color` at `alpha` onto a premultiplied
/// RGBA accumulator (components 0..=1).
fn blend_over(dst: &mut [f32; 4], color: [u8; 3], alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let inv = 1.0 - alpha;
    for c in 0..3 {
        dst[c] = color[c] as f32 / 255.0 * alpha + dst[c] * inv;
    }
    dst[3] = alpha + dst[3] * inv;
}

fn read_pixel(bitmap: &Bitmap, x: u32, y: u32) -> [f32; 4] {
    let i = ((y * bitmap.width + x) * 4) as usize;
    let p = &bitmap.pixels[i..i + 4];
    [
        p[0] as f32 / 255.0,
        p[1] as f32 / 255.0,
        p[2] as f32 / 255.0,
        p[3] as f32 / 255.0,
    ]
}

fn write_pixel(bitmap: &mut Bitmap, x: u32, y: u32, v: [f32; 4]) {
    let i = ((y * bitmap.width + x) * 4) as usize;
    for (dst, c) in bitmap.pixels[i..i + 4].iter_mut().zip(v) {
        *dst = (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }
}

/// How long the find-my-cursor pulse plays.
pub const PULSE_DURATION: Duration = Duration::from_millis(650);

/// The pulse ring's `(radius, alpha)` `elapsed` into the animation, or
/// `None` once it has finished: the ring starts wide and contracts onto the
/// pointer with an ease-out, fading in quickly and out at the very end.
pub fn pulse_frame(settings: &CursorSettings, elapsed: Duration) -> Option<(f32, f32)> {
    if elapsed >= PULSE_DURATION {
        return None;
    }
    let t = elapsed.as_secs_f32() / PULSE_DURATION.as_secs_f32();
    let eased = 1.0 - (1.0 - t).powi(3);
    let end = settings.size as f32 / 2.0;
    let radius = pulse_max_radius(settings) + (end - pulse_max_radius(settings)) * eased;
    let fade_in = (t / 0.12).min(1.0);
    let fade_out = ((1.0 - t) / 0.2).min(1.0);
    Some((radius, fade_in * fade_out))
}

/// Recognizes a "shake" -- several quick back-and-forth reversals of the
/// mouse -- from a stream of cursor positions, the gesture that plays the
/// find-my-cursor pulse. Each axis is tracked separately; a reversal only
/// counts if the stroke before it travelled at least [`Self::MIN_STROKE`]
/// pixels, so ordinary aiming jitter never qualifies.
#[derive(Debug, Clone, Default)]
pub struct ShakeDetector {
    last: Option<(i32, i32)>,
    axes: [AxisState; 2],
    cooldown_until: Duration,
}

#[derive(Debug, Clone, Default)]
struct AxisState {
    direction: i8,
    stroke: i32,
    reversals: Vec<Duration>,
}

impl ShakeDetector {
    pub const MIN_STROKE: i32 = 50;
    pub const REVERSALS: usize = 4;
    pub const WINDOW: Duration = Duration::from_millis(700);
    pub const COOLDOWN: Duration = Duration::from_millis(1200);

    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self {
            cooldown_until: self.cooldown_until,
            ..Self::default()
        };
    }

    /// Feeds the cursor position at time `now` (any monotonic clock);
    /// returns `true` exactly when a shake completes.
    pub fn feed(&mut self, now: Duration, x: i32, y: i32) -> bool {
        let Some((lx, ly)) = self.last.replace((x, y)) else {
            return false;
        };
        let mut shaken = false;
        for (axis, delta) in self.axes.iter_mut().zip([x - lx, y - ly]) {
            axis.reversals
                .retain(|&t| now.saturating_sub(t) <= Self::WINDOW);
            if delta == 0 {
                continue;
            }
            let direction = delta.signum() as i8;
            if direction == axis.direction {
                axis.stroke += delta.abs();
            } else {
                if axis.direction != 0 && axis.stroke >= Self::MIN_STROKE {
                    axis.reversals.push(now);
                }
                axis.direction = direction;
                axis.stroke = delta.abs();
            }
            if axis.reversals.len() >= Self::REVERSALS {
                shaken = true;
            }
        }
        if shaken {
            for axis in &mut self.axes {
                axis.reversals.clear();
            }
            if now >= self.cooldown_until {
                self.cooldown_until = now + Self::COOLDOWN;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(shape: CursorShape) -> CursorSettings {
        CursorSettings {
            enabled: true,
            shape,
            ..CursorSettings::default()
        }
    }

    fn render_centered(s: &CursorSettings) -> Bitmap {
        let (side, hot) = overlay_canvas(s);
        render_pointer(s, side, side, hot, hot)
    }

    fn alpha_at_offset(b: &Bitmap, dx: i32, dy: i32) -> u8 {
        b.alpha_at((b.hotspot_x + dx) as u32, (b.hotspot_y + dy) as u32)
    }

    #[test]
    fn a_config_without_a_cursor_table_gets_the_defaults() {
        let parsed: CursorSettings = toml::from_str("").unwrap();
        assert_eq!(parsed, CursorSettings::default());
        assert!(!parsed.enabled, "the game cursor must be opt-in");
    }

    #[test]
    fn a_partial_cursor_table_keeps_the_given_fields_and_defaults_the_rest() {
        let parsed: CursorSettings =
            toml::from_str("enabled = true\nshape = \"Arrow\"\nsize = 64").unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.shape, CursorShape::Arrow);
        assert_eq!(parsed.size, 64);
        assert_eq!(parsed.color, CursorSettings::default().color);
    }

    #[test]
    fn settings_round_trip_through_toml() {
        let s = CursorSettings {
            enabled: true,
            shape: CursorShape::Reticle,
            color: [1, 2, 3],
            ..CursorSettings::default()
        };
        let text = toml::to_string(&s).unwrap();
        assert_eq!(toml::from_str::<CursorSettings>(&text).unwrap(), s);
    }

    #[test]
    fn sanitized_clamps_out_of_range_values() {
        let s = CursorSettings {
            size: 0,
            thickness: 999,
            opacity: 0,
            border_width: 0,
            glow_size: 1000,
            ..CursorSettings::default()
        }
        .sanitized();
        assert_eq!(s.size, *SIZE_RANGE.start());
        assert_eq!(s.thickness, *THICKNESS_RANGE.end());
        assert_eq!(s.opacity, *OPACITY_RANGE.start());
        assert_eq!(s.border_width, *BORDER_WIDTH_RANGE.start());
        assert_eq!(s.glow_size, *GLOW_SIZE_RANGE.end());
    }

    #[test]
    fn should_show_requires_enabled() {
        let s = CursorSettings {
            enabled: false,
            ..CursorSettings::default()
        };
        assert!(!should_show(&s, true, true));
    }

    #[test]
    fn should_show_only_over_a_game() {
        let s = settings(CursorShape::Ring);
        assert!(should_show(&s, true, true));
        assert!(!should_show(&s, false, true));
    }

    #[test]
    fn a_config_with_the_removed_method_and_scope_keys_still_loads() {
        let parsed: CursorSettings = toml::from_str(
            "enabled = true\nmethod = \"ReplaceSystemCursor\"\nscope = \"Everywhere\"\nsize = 64",
        )
        .unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.size, 64);
    }

    #[test]
    fn hidden_real_cursor_only_hides_the_game_cursor_when_asked_to() {
        let s = settings(CursorShape::Ring);
        assert!(should_show(&s, true, false));
        let s = CursorSettings {
            hide_when_cursor_hidden: true,
            ..s
        };
        assert!(!should_show(&s, true, false));
        assert!(should_show(&s, true, true));
    }

    #[test]
    fn ring_is_opaque_on_its_stroke_and_clear_in_its_middle() {
        let s = CursorSettings {
            glow: false,
            border: false,
            opacity: 100,
            ..settings(CursorShape::Ring)
        };
        let b = render_centered(&s);
        let on_stroke = (s.size / 2 - s.thickness / 2) as i32;
        assert_eq!(alpha_at_offset(&b, on_stroke, 0), 255);
        assert_eq!(alpha_at_offset(&b, 0, 0), 0);
        assert_eq!(alpha_at_offset(&b, (s.size / 2) as i32 + 3, 0), 0);
    }

    #[test]
    fn ring_dot_and_dot_fill_the_hotspot() {
        for shape in [
            CursorShape::RingDot,
            CursorShape::Dot,
            CursorShape::Reticle,
            CursorShape::Crosshair,
        ] {
            let s = CursorSettings {
                glow: false,
                border: false,
                opacity: 100,
                ..settings(shape)
            };
            let b = render_centered(&s);
            assert_eq!(
                alpha_at_offset(&b, 0, 0),
                255,
                "{shape:?} should cover its hotspot"
            );
        }
    }

    #[test]
    fn disc_interior_is_translucent() {
        let s = CursorSettings {
            glow: false,
            border: false,
            opacity: 100,
            ..settings(CursorShape::Disc)
        };
        let b = render_centered(&s);
        let a = alpha_at_offset(&b, 0, 0);
        assert!(a > 50 && a < 150, "disc interior alpha was {a}");
    }

    #[test]
    fn arrow_hotspot_is_its_tip_and_it_extends_down_right() {
        let s = CursorSettings {
            glow: false,
            border: false,
            opacity: 100,
            ..settings(CursorShape::Arrow)
        };
        let b = render_centered(&s);
        let size = s.size as i32;
        assert_eq!(
            alpha_at_offset(&b, 2, size / 2),
            255,
            "inside the arrow body"
        );
        assert_eq!(
            alpha_at_offset(&b, -4, size / 2),
            0,
            "nothing left of the tip"
        );
        assert_eq!(alpha_at_offset(&b, 4, -4), 0, "nothing above the tip");
    }

    #[test]
    fn border_darkens_just_outside_the_shape() {
        let s = CursorSettings {
            glow: false,
            border: true,
            border_width: 3,
            opacity: 100,
            ..settings(CursorShape::Dot)
        };
        let b = render_centered(&s);
        let just_outside = (s.size / 2) as i32 + 1;
        let i =
            (((b.hotspot_y) as u32 * b.width + (b.hotspot_x + just_outside) as u32) * 4) as usize;
        assert_eq!(b.pixels[i + 3], 255, "border is opaque");
        assert_eq!(&b.pixels[i..i + 3], &[0, 0, 0], "border uses border_color");
    }

    #[test]
    fn glow_fades_out_with_distance() {
        let s = CursorSettings {
            glow: true,
            border: false,
            glow_size: 20,
            glow_strength: 100,
            opacity: 100,
            ..settings(CursorShape::Dot)
        };
        let b = render_centered(&s);
        let r = (s.size / 2) as i32;
        let near = alpha_at_offset(&b, r + 3, 0);
        let far = alpha_at_offset(&b, r + 15, 0);
        let beyond = alpha_at_offset(&b, r + 22, 0);
        assert!(near > far && far > 0, "near={near} far={far}");
        assert_eq!(beyond, 0);
    }

    #[test]
    fn opacity_scales_alpha() {
        let s = CursorSettings {
            glow: false,
            border: false,
            opacity: 50,
            ..settings(CursorShape::Dot)
        };
        let b = render_centered(&s);
        let a = alpha_at_offset(&b, 0, 0);
        assert!((126..=129).contains(&a), "alpha was {a}");
    }

    #[test]
    fn every_shape_fits_inside_its_cursor_canvas() {
        for shape in CursorShape::ALL {
            let s = settings(shape);
            let (w, h, hx, hy) = cursor_canvas(&s);
            let b = render_pointer(&s, w, h, hx, hy);
            for x in 0..w {
                assert_eq!(b.alpha_at(x, 0), 0, "{shape:?} touches the top edge");
                assert_eq!(b.alpha_at(x, h - 1), 0, "{shape:?} touches the bottom edge");
            }
            for y in 0..h {
                assert_eq!(b.alpha_at(0, y), 0, "{shape:?} touches the left edge");
                assert_eq!(b.alpha_at(w - 1, y), 0, "{shape:?} touches the right edge");
            }
        }
    }

    #[test]
    fn overlay_canvas_fits_the_largest_pulse_ring() {
        let s = settings(CursorShape::Ring);
        let (side, hot) = overlay_canvas(&s);
        let mut b = Bitmap::blank(side, side, hot, hot);
        draw_pulse_ring(&mut b, &s, pulse_max_radius(&s), 1.0);
        assert!((0..side).all(|x| b.alpha_at(x, 0) == 0 && b.alpha_at(x, side - 1) == 0));
        assert!(b.pixels.iter().any(|&p| p > 0), "the ring was drawn");
    }


    #[test]
    fn pulse_contracts_onto_the_pointer_and_ends() {
        let s = settings(CursorShape::Ring);
        let (r0, _) = pulse_frame(&s, Duration::ZERO).unwrap();
        let (r_mid, a_mid) = pulse_frame(&s, PULSE_DURATION / 2).unwrap();
        assert_eq!(r0, pulse_max_radius(&s));
        assert!(r_mid < r0 && r_mid > s.size as f32 / 2.0);
        assert!(a_mid > 0.9);
        assert!(pulse_frame(&s, PULSE_DURATION).is_none());
    }

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn shaking_left_and_right_is_detected() {
        let mut d = ShakeDetector::new();
        let xs = [500, 600, 500, 600, 500, 600];
        let hits: Vec<bool> = xs
            .iter()
            .enumerate()
            .map(|(i, &x)| d.feed(ms(i as u64 * 60), x, 300))
            .collect();
        assert_eq!(hits.iter().filter(|&&h| h).count(), 1, "{hits:?}");
    }

    #[test]
    fn small_jitter_is_not_a_shake() {
        let mut d = ShakeDetector::new();
        for i in 0..40 {
            let x = if i % 2 == 0 { 500 } else { 520 };
            assert!(!d.feed(ms(i * 20), x, 300));
        }
    }

    #[test]
    fn slow_back_and_forth_is_not_a_shake() {
        let mut d = ShakeDetector::new();
        for (i, x) in [500, 700, 500, 700, 500, 700].into_iter().enumerate() {
            assert!(!d.feed(ms(i as u64 * 400), x, 300));
        }
    }

    #[test]
    fn a_long_straight_sweep_is_not_a_shake() {
        let mut d = ShakeDetector::new();
        for i in 0..50 {
            assert!(!d.feed(ms(i * 10), 100 + i as i32 * 30, 300));
        }
    }

    #[test]
    fn shakes_are_rate_limited_by_the_cooldown() {
        let mut d = ShakeDetector::new();
        let mut hits = 0;
        for i in 0..30u64 {
            let x = if i % 2 == 0 { 500 } else { 620 };
            if d.feed(ms(i * 40), x, 300) {
                hits += 1;
            }
        }
        // 30 strokes over 1.2s -- continuous shaking yields one pulse per cooldown.
        assert_eq!(hits, 1);
    }

    /// A solid 16x16 square cursor of `bgr` with full alpha, hotspot top-left.
    fn square_cursor(bgr: [u8; 3]) -> Bitmap {
        let bgra: Vec<u8> = (0..256).flat_map(|_| [bgr[0], bgr[1], bgr[2], 255]).collect();
        cursor_image(16, 16, 0, 0, &bgra, None)
    }

    #[test]
    fn cursor_image_premultiplies_and_swaps_to_rgba() {
        // One half-transparent pure-blue pixel (BGRA).
        let image = cursor_image(1, 1, 0, 0, &[255, 0, 0, 128], None);
        assert_eq!(image.pixels, vec![0, 0, 128, 128]);
    }

    #[test]
    fn cursor_image_without_alpha_takes_transparency_from_the_and_mask() {
        // Two white pixels, no alpha; the AND mask makes the second clear.
        let bgra = [255, 255, 255, 0, 255, 255, 255, 0];
        let and = [0, 0, 0, 0, 255, 255, 255, 0];
        let image = cursor_image(2, 1, 0, 0, &bgra, Some(&and));
        assert_eq!(image.alpha_at(0, 0), 255);
        assert_eq!(image.alpha_at(1, 0), 0);
    }

    #[test]
    fn game_own_enlarges_the_image_onto_its_hotspot() {
        let s = CursorSettings {
            shape: CursorShape::GameOwn,
            size: 64, // 4x the 16-pixel square
            opacity: 100,
            border: false,
            glow: false,
            ..CursorSettings::default()
        };
        let out = render_game_own(&s, &square_cursor([0, 0, 255]));
        let at = |dx: i32, dy: i32| {
            let i = (((out.hotspot_y + dy) as u32 * out.width + (out.hotspot_x + dx) as u32) * 4) as usize;
            out.pixels[i..i + 4].to_vec()
        };
        // The red copy, 64 px wide around the hotspot.
        assert_eq!(at(32, 32), vec![255, 0, 0, 255]);
        assert_eq!(at(58, 10), vec![255, 0, 0, 255]);
        // Past its far edge, bilinear magnification only leaves a soft edge
        // fading out within a couple of pixels, inside the canvas.
        assert!(at(64, 10)[3] < 64);
        let last = out.width as i32 - 1 - out.hotspot_x;
        assert_eq!(at(last, 10)[3], 0);
    }

    #[test]
    fn game_own_reaches_past_the_left_and_top_of_a_hotspot_in_the_corner() {
        // A corner hotspot (an arrow's tip): enlarged around the hotspot
        // pixel's bottom-right corner, the copy reaches 4 - 1 = 3 px left of
        // and above it, so the game's cursor on top sits inside the copy's
        // edge there too, not on it.
        let s = CursorSettings {
            shape: CursorShape::GameOwn,
            size: 64,
            opacity: 100,
            border: false,
            glow: false,
            ..CursorSettings::default()
        };
        let out = render_game_own(&s, &square_cursor([0, 0, 255]));
        let alpha = |dx: i32, dy: i32| out.alpha_at((out.hotspot_x + dx) as u32, (out.hotspot_y + dy) as u32);
        assert!(alpha(-2, 10) > 128, "left: {}", alpha(-2, 10));
        assert!(alpha(10, -2) > 128, "top: {}", alpha(10, -2));
        assert_eq!(alpha(-6, 10), 0);
    }

    #[test]
    fn game_own_at_its_own_size_draws_only_the_decoration_outside_the_outline() {
        let s = CursorSettings {
            shape: CursorShape::GameOwn,
            size: 16, // the image's own height: no enlarged copy
            opacity: 100,
            border: true,
            border_width: 2,
            border_color: [0, 0, 0],
            glow: false,
            ..CursorSettings::default()
        };
        let out = render_game_own(&s, &square_cursor([0, 0, 255]));
        let alpha = |dx: i32, dy: i32| out.alpha_at((out.hotspot_x + dx) as u32, (out.hotspot_y + dy) as u32);
        // Inside the outline: untouched, so the game's cursor isn't tinted.
        assert_eq!(alpha(8, 8), 0);
        // Just outside it, on either side: the border.
        assert_eq!(alpha(16, 8), 255);
        assert_eq!(alpha(-1, 8), 255);
        // Beyond the border: clear.
        assert_eq!(alpha(16 + 4, 8), 0);
    }

    #[test]
    fn game_own_glow_follows_the_outline_and_fades() {
        let s = CursorSettings {
            shape: CursorShape::GameOwn,
            size: 16,
            border: false,
            glow: true,
            glow_size: 10,
            glow_strength: 100,
            ..CursorSettings::default()
        };
        let out = render_game_own(&s, &square_cursor([255, 255, 255]));
        let alpha = |dx: i32, dy: i32| out.alpha_at((out.hotspot_x + dx) as u32, (out.hotspot_y + dy) as u32);
        let near = alpha(17, 8);
        let far = alpha(23, 8);
        assert!(near > far && far > 0, "near {near}, far {far}");
    }

    #[test]
    fn distance_transform_measures_to_the_nearest_inside_pixel() {
        // 5x1 row with the middle pixel inside.
        let d = distance_transform(&[false, false, true, false, false], 5, 1);
        assert_eq!(d, vec![4.0, 1.0, 0.0, 1.0, 4.0]);
        // 3x3 with the center inside: corners are sqrt(2) away.
        let mut inside = [false; 9];
        inside[4] = true;
        let d = distance_transform(&inside, 3, 3);
        assert_eq!(d[0], 2.0);
        assert_eq!(d[1], 1.0);
    }

    #[test]
    fn pulse_canvas_for_keeps_the_pointer_on_the_hotspot() {
        let s = CursorSettings {
            shape: CursorShape::GameOwn,
            ..CursorSettings::default()
        };
        let pointer = render_game_own(&s, &square_cursor([0, 0, 255]));
        let canvas = pulse_canvas_for(&s, &pointer);
        let (_, pulse_half) = overlay_canvas(&s);
        assert!(canvas.hotspot_x >= pulse_half);
        assert_eq!(canvas.width as i32, canvas.hotspot_x * 2);
        // The pixel at the hotspot is the pointer's hotspot pixel.
        let at = |b: &Bitmap, x: i32, y: i32| b.alpha_at(x as u32, y as u32);
        assert_eq!(
            at(&canvas, canvas.hotspot_x + 2, canvas.hotspot_y + 2),
            at(&pointer, pointer.hotspot_x + 2, pointer.hotspot_y + 2)
        );
    }

    #[test]
    fn the_game_own_preview_renders_around_the_stand_in() {
        let s = CursorSettings {
            shape: CursorShape::GameOwn,
            size: 64,
            ..CursorSettings::default()
        };
        let preview = render_preview(&s, None);
        assert!(preview.width > 64 && preview.pixels.iter().any(|&p| p != 0));
    }

    #[test]
    fn the_game_own_preview_puts_the_game_cursor_on_top() {
        let s = CursorSettings {
            shape: CursorShape::GameOwn,
            size: 64,
            opacity: 50,
            ..CursorSettings::default()
        };
        let game = square_cursor([0, 0, 255]); // red, 16 px
        let preview = render_preview(&s, Some(&game));
        let alpha = |x: i32, y: i32| preview.alpha_at((preview.hotspot_x + x) as u32, (preview.hotspot_y + y) as u32);
        // Inside the real cursor's 16 px: it, fully opaque; beyond it: the
        // half-transparent enlarged copy.
        assert_eq!(alpha(8, 8), 255);
        assert!((120..=135).contains(&alpha(40, 40)), "{}", alpha(40, 40));
    }
}
