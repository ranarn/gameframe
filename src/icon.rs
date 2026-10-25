//! GameFrame's visual mark: four inward-facing corner brackets -- a
//! viewfinder/crop-frame motif that ties directly to the app's core feature
//! of framing/snapping windows into position -- rendered as a violet-to-cyan
//! diagonal gradient on a transparent background.
//!
//! The mark is rendered fresh at whatever size is needed (tray icon, window
//! icon, the exe's `.ico` resource) rather than scaling one fixed bitmap, so
//! every size stays crisp. 4x supersampling anti-aliases the bracket edges
//! even at 16x16.

const GRADIENT_START: [f32; 3] = [124.0, 58.0, 237.0]; // violet
const GRADIENT_END: [f32; 3] = [6.0, 182.0, 212.0]; // cyan
const SUPERSAMPLE: u32 = 4;

/// Renders the mark as straight-alpha RGBA8, `size * size * 4` bytes --
/// directly usable as `tray_icon::Icon::from_rgba`'s or `egui::IconData`'s
/// pixel buffer.
pub fn mark_rgba(size: u32) -> Vec<u8> {
    let hi = (size * SUPERSAMPLE) as f32;
    let margin = hi * 0.14;
    let arm_len = hi * 0.28;
    let thickness = (hi * 0.11).max(SUPERSAMPLE as f32);

    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let covered = supersampled_coverage(px, py, hi, margin, arm_len, thickness);
            if covered == 0 {
                continue;
            }
            let alpha = (covered * 255 / (SUPERSAMPLE * SUPERSAMPLE)) as u8;
            let t = ((px + py) as f32 / (2.0 * size as f32)).clamp(0.0, 1.0);
            let [r, g, b] = lerp_gradient(t);
            let idx = ((py * size + px) * 4) as usize;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = alpha;
        }
    }
    rgba
}

/// Counts how many of the `SUPERSAMPLE * SUPERSAMPLE` sub-pixel samples of
/// pixel `(px, py)` fall inside a bracket, for edge anti-aliasing.
fn supersampled_coverage(px: u32, py: u32, hi: f32, margin: f32, arm_len: f32, thickness: f32) -> u32 {
    let mut covered = 0;
    for sy in 0..SUPERSAMPLE {
        for sx in 0..SUPERSAMPLE {
            let x = (px * SUPERSAMPLE + sx) as f32 + 0.5;
            let y = (py * SUPERSAMPLE + sy) as f32 + 0.5;
            if in_bracket(x, y, hi, margin, arm_len, thickness) {
                covered += 1;
            }
        }
    }
    covered
}

fn lerp_gradient(t: f32) -> [u8; 3] {
    std::array::from_fn(|i| (GRADIENT_START[i] + (GRADIENT_END[i] - GRADIENT_START[i]) * t).round() as u8)
}

/// True if `(x, y)`, in the supersampled `hi`-sized canvas, falls inside one
/// of the four corner brackets, each an "L" of two arms opening toward the
/// center.
fn in_bracket(x: f32, y: f32, hi: f32, margin: f32, arm_len: f32, thickness: f32) -> bool {
    let far = hi - margin;
    // (corner_x, corner_y, arm direction toward center)
    let corners = [
        (margin, margin, 1.0f32, 1.0f32),
        (far, margin, -1.0, 1.0),
        (margin, far, 1.0, -1.0),
        (far, far, -1.0, -1.0),
    ];
    corners.iter().any(|&corner| in_arm_pair(x, y, corner, arm_len, thickness))
}

/// One corner's two arms: a horizontal band running along the corner's row
/// and a vertical band running along its column, both `thickness` wide and
/// `arm_len` long, extending from `(cx, cy)` toward the center by `(dx, dy)`.
fn in_arm_pair(x: f32, y: f32, (cx, cy, dx, dy): (f32, f32, f32, f32), arm_len: f32, thickness: f32) -> bool {
    let in_range = |v: f32, a: f32, b: f32| v >= a.min(b) && v <= a.max(b);

    let horizontal = in_range(x, cx, cx + dx * arm_len) && in_range(y, cy, cy + dy * thickness);
    let vertical = in_range(x, cx, cx + dx * thickness) && in_range(y, cy, cy + dy * arm_len);
    horizontal || vertical
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_a_correctly_sized_straight_alpha_rgba_buffer() {
        let rgba = mark_rgba(32);
        assert_eq!(rgba.len(), 32 * 32 * 4);
    }

    #[test]
    fn the_exact_corner_pixel_is_opaque_and_the_center_is_transparent() {
        let size = 64;
        let rgba = mark_rgba(size);
        let pixel_alpha = |x: u32, y: u32| rgba[((y * size + x) * 4 + 3) as usize];

        // Just inside the top-left margin, on the horizontal arm.
        let margin = (size as f32 * 0.14).round() as u32;
        assert!(pixel_alpha(margin + 2, margin) > 200, "corner bracket pixel should be opaque");

        // Dead center falls between all four inward-opening brackets.
        assert_eq!(pixel_alpha(size / 2, size / 2), 0, "center should stay transparent");
    }

    #[test]
    fn gradient_shifts_from_violet_toward_cyan_across_the_diagonal() {
        let size = 64;
        let rgba = mark_rgba(size);

        // Find the opaque pixel closest to the top-left corner (lowest
        // x+y, near GRADIENT_START) and the one closest to the
        // bottom-right corner (highest x+y, near GRADIENT_END) by scanning
        // rather than assuming exact geometry -- avoids coupling this test
        // to margin/thickness rounding internals.
        let mut nearest_top_left: Option<(u32, [u8; 4])> = None;
        let mut nearest_bottom_right: Option<(u32, [u8; 4])> = None;
        for y in 0..size {
            for x in 0..size {
                let idx = ((y * size + x) * 4) as usize;
                let pixel = [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]];
                if pixel[3] == 0 {
                    continue;
                }
                let key = x + y;
                if nearest_top_left.is_none_or(|(best, _)| key < best) {
                    nearest_top_left = Some((key, pixel));
                }
                if nearest_bottom_right.is_none_or(|(best, _)| key > best) {
                    nearest_bottom_right = Some((key, pixel));
                }
            }
        }

        let (_, top_left) = nearest_top_left.expect("top-left bracket should have opaque pixels");
        let (_, bottom_right) = nearest_bottom_right.expect("bottom-right bracket should have opaque pixels");

        // Violet (GRADIENT_START) -> cyan (GRADIENT_END): red falls sharply,
        // green rises sharply. Blue stays high throughout (237 -> 212), so
        // it isn't a reliable discriminator between the two ends.
        assert!(top_left[0] > bottom_right[0], "red should fall from top-left to bottom-right");
        assert!(top_left[1] < bottom_right[1], "green should rise from top-left to bottom-right");
    }

    #[test]
    fn stays_legible_at_tray_icon_size() {
        // At 16x16 the minimum-thickness clamp must still produce visible,
        // non-degenerate brackets rather than vanishing entirely.
        let rgba = mark_rgba(16);
        let opaque_pixel_count = rgba.as_chunks::<4>().0.iter().filter(|p| p[3] > 128).count();
        assert!(opaque_pixel_count > 8, "brackets should not disappear at 16x16, got {opaque_pixel_count} opaque pixels");
    }
}
