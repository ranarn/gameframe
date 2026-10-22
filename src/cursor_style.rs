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
//!   layered window that follows the mouse, and the optional system-cursor
//!   swap.
//!
//! Every shape is described by a signed distance function (negative inside,
//! in pixels, relative to the hotspot), which makes the border (the shape
//! grown by `border_width`), the glow (a falloff outside the shape) and
//! one-pixel anti-aliasing all fall out of the same number.

use std::time::Duration;

/// The pointer variants a user can pick. The hotspot (where the real
/// cursor position lands) is the arrow's tip for [`CursorShape::Arrow`] and
/// the center for every other shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CursorShape {
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
    pub const ALL: [CursorShape; 7] = [
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
        !matches!(self, CursorShape::Arrow | CursorShape::Dot)
    }
}

/// How the game cursor reaches the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CursorMethod {
    /// A click-through, always-on-top window that follows the mouse. Works
    /// in every borderless/windowed game, whatever cursor the game uses; the
    /// game's own cursor stays visible underneath, and the drawing trails the
    /// real cursor by about one frame.
    #[default]
    Overlay,
    /// Swaps the Windows arrow/hand cursors for the game cursor while a
    /// cursor-enabled game is in the foreground, and restores the user's
    /// scheme afterwards. No lag and no double cursor, but only visible in
    /// games that use the standard Windows cursor rather than their own.
    ReplaceSystemCursor,
}

/// Where the game cursor is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CursorScope {
    /// Only while a tracked game with its "Game cursor" box ticked is the
    /// foreground window.
    #[default]
    TrackedGames,
    /// Whatever is in the foreground.
    Everywhere,
}

/// The persisted game-cursor settings (config table `[cursor]`). Every field
/// has a default, so a config written before this feature existed -- or one
/// missing any single field -- loads unchanged.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CursorSettings {
    pub enabled: bool,
    pub method: CursorMethod,
    pub scope: CursorScope,
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
            method: CursorMethod::default(),
            scope: CursorScope::default(),
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
/// `foreground_has_game_cursor` is whether the foreground window belongs to
/// a tracked game with its per-game "Game cursor" flag on; it is ignored in
/// [`CursorScope::Everywhere`].
pub fn should_show(
    settings: &CursorSettings,
    foreground_has_game_cursor: bool,
    cursor_showing: bool,
) -> bool {
    settings.enabled
        && (settings.scope == CursorScope::Everywhere || foreground_has_game_cursor)
        && (!settings.hide_when_cursor_hidden || cursor_showing)
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

    /// The same image as straight (non-premultiplied) BGRA, the layout a
    /// 32-bit Windows cursor's color bitmap expects.
    pub fn to_straight_bgra(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len());
        for px in self.pixels.as_chunks::<4>().0 {
            let a = px[3];
            let un = |c: u8| {
                if a == 0 {
                    0
                } else {
                    ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
                }
            };
            out.extend_from_slice(&[un(px[2]), un(px[1]), un(px[0]), a]);
        }
        out
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
        CursorShape::Arrow => (0.0, 0.0, 0.57 * s, 0.94 * s),
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

/// The tight canvas for a system-cursor bitmap: just the shape plus its
/// border/glow. Returns `(width, height, hotspot_x, hotspot_y)`.
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
        CursorShape::Arrow => {
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
            method: CursorMethod::ReplaceSystemCursor,
            scope: CursorScope::Everywhere,
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
    fn should_show_in_tracked_games_scope_follows_the_foreground_game() {
        let s = settings(CursorShape::Ring);
        assert!(should_show(&s, true, true));
        assert!(!should_show(&s, false, true));
    }

    #[test]
    fn should_show_everywhere_ignores_the_foreground_game() {
        let s = CursorSettings {
            scope: CursorScope::Everywhere,
            ..settings(CursorShape::Ring)
        };
        assert!(should_show(&s, false, true));
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
    fn straight_bgra_unpremultiplies() {
        let b = Bitmap {
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            pixels: vec![64, 0, 128, 128],
        };
        assert_eq!(b.to_straight_bgra(), vec![255, 0, 128, 128]);
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
}
