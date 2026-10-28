//! Pure state machine for the zone-editor overlay's drag-and-draw / resize
//! interaction model. Notes:
//! - Uses [`crate::profile_store::Zone`] directly, so [`AppState::apply`]'s
//!   `Save` output plugs into `Config::zone` with no conversion step.
//! - Holds no taskbar state -- the real
//!   mode-aware taskbar-hide/restore mechanism lives in [`crate::win32`]
//!   (`hide_taskbar`/`restore_taskbar`) and is driven by the OS-facing
//!   overlay window ([`crate::zone_overlay`]) around the editing session,
//!   not modeled as part of this pure state machine.
//!
//! - Works entirely in physical pixels; [`points_to_pixels`] and friends
//!   convert at the egui boundary in [`crate::zone_overlay`].
//!
//! No window/OS dependency here -- fully unit tested below.

use crate::profile_store::Zone;

/// A pointer position in the overlay window's own coordinate space (the
/// overlay is always positioned at the target monitor's origin, so these
/// are monitor-local, not necessarily absolute screen coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// The drawable area's size. Origin is always (0, 0) -- see [`Point`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    N,
    S,
    E,
    W,
    Ne,
    Nw,
    Se,
    Sw,
}

impl Handle {
    pub const ALL: [Handle; 8] = [
        Handle::N,
        Handle::S,
        Handle::E,
        Handle::W,
        Handle::Ne,
        Handle::Nw,
        Handle::Se,
        Handle::Sw,
    ];
}

/// What a pointer position over a placed Zone grabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A corner handle or an edge (handle or the edge's band): resizes.
    Resize(Handle),
    /// The Zone's interior: moves the whole Zone.
    Move,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorState {
    /// No zone exists yet -- nothing drawn.
    Empty,
    /// Corner-to-corner initial draw, live between the anchor and the
    /// current pointer position.
    Dragging { anchor: Point, current: Point },
    /// A zone exists and is idle; all eight handles are grabbable.
    Placed { zone: Zone },
    /// A specific handle is being dragged; `original` is kept so `Cancel`
    /// can discard back to it without needing a separate undo stack.
    Resizing {
        original: Zone,
        handle: Handle,
        /// Pointer-to-edge offset at press (edge minus pointer, per axis the
        /// handle owns; 0 on the others), so grabbing off the outline
        /// doesn't make the edge jump to the pointer.
        offset: Point,
        live: Zone,
    },
    /// The whole Zone is being dragged; `grab` is where the pointer pressed,
    /// so the Zone follows the pointer's offset from it.
    Moving {
        original: Zone,
        grab: Point,
        live: Zone,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    pub monitor: Monitor,
    pub editor: EditorState,
    pub last_message: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    Start(Point),
    Drag(Point),
    /// Finish the current drag or resize, committing it to `Placed`.
    Commit,
    /// Start resizing via Handle, pressed at this pointer position.
    Grab(Handle, Point),
    /// Start moving the whole Zone, pressed at this pointer position.
    GrabMove(Point),
    Save,
    /// Discard any in-progress edit and close the editing session.
    Cancel,
}

/// What an [`Action::Save`] or [`Action::Cancel`] produced -- the editing
/// session is over once this is returned, so the overlay window can close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    Saved(Zone),
    Cancelled,
}

impl AppState {
    /// Starts a fresh editing session with nothing drawn yet.
    pub fn new(monitor: Monitor) -> Self {
        Self {
            monitor,
            editor: EditorState::Empty,
            last_message: "Click and drag to draw a zone".to_string(),
        }
    }

    /// Starts an editing session pre-loaded with an existing zone (e.g. the
    /// already-configured Zone), so resizing it doesn't require
    /// redrawing from scratch.
    pub fn new_with_zone(monitor: Monitor, zone: Zone) -> Self {
        Self {
            monitor,
            editor: EditorState::Placed { zone },
            last_message: "editing existing zone -- grab a handle".to_string(),
        }
    }

    /// Applies `action`, returning `Some(SessionOutcome)` only when the
    /// editing session is now over (a legal `Save`, or any `Cancel`).
    pub fn apply(&mut self, action: Action) -> Option<SessionOutcome> {
        match action {
            Action::Start(p) => {
                match self.editor {
                    EditorState::Empty => {
                        self.editor = EditorState::Dragging {
                            anchor: p,
                            current: p,
                        };
                        self.last_message = "drawing from corner".to_string();
                    }
                    _ => self.reject("`start` is only legal from Empty"),
                }
                None
            }
            Action::Drag(p) => {
                match &mut self.editor {
                    EditorState::Dragging { current, .. } => {
                        *current = p;
                        self.last_message = "live preview updated".to_string();
                    }
                    EditorState::Resizing {
                        original,
                        handle,
                        offset,
                        live,
                    } => {
                        let target = Point {
                            x: p.x + offset.x,
                            y: p.y + offset.y,
                        };
                        *live = apply_handle_drag(*original, *handle, target, self.monitor);
                        self.last_message = "live resize updated".to_string();
                    }
                    EditorState::Moving {
                        original,
                        grab,
                        live,
                    } => {
                        *live = move_zone(*original, *grab, p, self.monitor);
                        self.last_message = "live move updated".to_string();
                    }
                    _ => self.reject("`drag` needs an active draw or resize"),
                }
                None
            }
            Action::Commit => {
                match &self.editor {
                    EditorState::Dragging { anchor, current } => {
                        let zone = normalize(*anchor, *current, self.monitor);
                        self.editor = EditorState::Placed { zone };
                        self.last_message = "zone drawn".to_string();
                    }
                    EditorState::Resizing { live, .. } => {
                        self.editor = EditorState::Placed { zone: *live };
                        self.last_message = "resize committed".to_string();
                    }
                    EditorState::Moving { live, .. } => {
                        self.editor = EditorState::Placed { zone: *live };
                        self.last_message = "move committed".to_string();
                    }
                    _ => self.reject("nothing to commit"),
                }
                None
            }
            Action::Grab(handle, press) => {
                match &self.editor {
                    EditorState::Placed { zone } => {
                        let offset = grab_offset(zone, handle, press);
                        self.editor = EditorState::Resizing {
                            original: *zone,
                            handle,
                            offset,
                            live: *zone,
                        };
                        self.last_message = format!("resizing via {handle:?}");
                    }
                    _ => self.reject("`grab` needs a placed zone"),
                }
                None
            }
            Action::GrabMove(p) => {
                match &self.editor {
                    EditorState::Placed { zone } => {
                        self.editor = EditorState::Moving {
                            original: *zone,
                            grab: p,
                            live: *zone,
                        };
                        self.last_message = "moving zone".to_string();
                    }
                    _ => self.reject("`grab` needs a placed zone"),
                }
                None
            }
            Action::Save => match &self.editor {
                EditorState::Placed { zone, .. } => {
                    let zone = *zone;
                    self.last_message = format!("saved: {zone:?}");
                    Some(SessionOutcome::Saved(zone))
                }
                _ => {
                    self.reject("`save` needs a placed zone -- finish the drag/resize first");
                    None
                }
            },
            Action::Cancel => {
                // Intentionally does not reset `self.editor` back to `Empty` (unlike the
                // prototype this was lifted from). The caller (`zone_overlay`) drops the whole
                // `AppState` as soon as `apply` returns `Some(outcome)`, so it never re-reads
                // `editor` after a session-ending outcome -- resetting it here would be dead work.
                self.last_message = "cancelled -- discarded any in-progress edit".to_string();
                Some(SessionOutcome::Cancelled)
            }
        }
    }

    fn reject(&mut self, reason: &str) {
        self.last_message = format!("reject: {reason}");
    }
}

/// A drawn/resized edge within this many pixels of a monitor boundary snaps
/// to it exactly, rather than requiring the pointer to land on the exact
/// boundary pixel -- mouse-drag precision rarely lands there, and no
/// deliberately-configured zone edge sits closer than this to the monitor
/// edge without meaning to be flush with it.
const EDGE_SNAP_PX: i32 = 20;

/// The scale factor (egui points to physical pixels) for a monitor DPI;
/// 96 DPI is 100 %.
pub fn scale_from_dpi(dpi: u32) -> f32 {
    dpi as f32 / 96.0
}

/// Converts a length or coordinate in egui points to physical pixels,
/// rounding to the nearest pixel (halves away from zero).
pub fn points_to_pixels(points: f32, scale: f32) -> i32 {
    (points * scale).round() as i32
}

/// Converts physical pixels to egui points. Inverse of [`points_to_pixels`]:
/// `points_to_pixels(pixels_to_points(px, s), s) == px`.
pub fn pixels_to_points(pixels: i32, scale: f32) -> f32 {
    pixels as f32 / scale
}

/// Converts an egui pointer position (points) to a pixel [`Point`].
pub fn point_from_points(x: f32, y: f32, scale: f32) -> Point {
    Point {
        x: points_to_pixels(x, scale),
        y: points_to_pixels(y, scale),
    }
}

/// Snaps `value` to `edge` when within [`EDGE_SNAP_PX`] of it.
fn snap_to_edge(value: i32, edge: i32) -> i32 {
    if (value - edge).abs() <= EDGE_SNAP_PX {
        edge
    } else {
        value
    }
}

/// Normalizes a two-corner drag into a positive-area zone, clamped to the
/// monitor. Mirrors `region_calc::resolve_region`'s Zone semantics
/// (clamp to fit rather than reject) since a live drag has no "invalid
/// input" concept the way a typed X/Y/width/height field does.
fn normalize(anchor: Point, current: Point, monitor: Monitor) -> Zone {
    let x0 = snap_to_edge(anchor.x.min(current.x).clamp(0, monitor.width), 0);
    let y0 = snap_to_edge(anchor.y.min(current.y).clamp(0, monitor.height), 0);
    let x1 = snap_to_edge(
        anchor.x.max(current.x).clamp(0, monitor.width),
        monitor.width,
    );
    let y1 = snap_to_edge(
        anchor.y.max(current.y).clamp(0, monitor.height),
        monitor.height,
    );
    Zone {
        x: x0,
        y: y0,
        width: (x1 - x0).max(1),
        height: (y1 - y0).max(1),
    }
}

/// Which [`Target`] `pointer` grabs on `zone`, or `None` outside it.
///
/// `grab_px` is the hit size of a handle in pixels: corners and edges are
/// grabbable within `grab_px / 2` of the Zone's outline (straddling it), so
/// the grab area covers the drawn handle. Corners win over edges, and the
/// closer edge wins on a Zone thinner than the grab band; everything else
/// inside the Zone is [`Target::Move`].
pub fn hit_test(zone: &Zone, pointer: Point, grab_px: i32) -> Option<Target> {
    let half_x = (grab_px / 2).min(zone.width / 3);
    let half_y = (grab_px / 2).min(zone.height / 3);
    let (left, top) = (zone.x, zone.y);
    let (right, bottom) = (zone.x + zone.width, zone.y + zone.height);

    let corners = [
        (Handle::Nw, left, top),
        (Handle::Ne, right, top),
        (Handle::Sw, left, bottom),
        (Handle::Se, right, bottom),
    ];
    let corner = corners
        .iter()
        .filter(|(_, cx, cy)| (pointer.x - cx).abs() <= half_x && (pointer.y - cy).abs() <= half_y)
        .min_by_key(|(_, cx, cy)| (pointer.x - cx).pow(2) + (pointer.y - cy).pow(2));
    if let Some((handle, _, _)) = corner {
        return Some(Target::Resize(*handle));
    }

    let within_x = pointer.x >= left && pointer.x <= right;
    let within_y = pointer.y >= top && pointer.y <= bottom;
    let edges = [
        (Handle::N, within_x, (pointer.y - top).abs(), half_y),
        (Handle::S, within_x, (pointer.y - bottom).abs(), half_y),
        (Handle::W, within_y, (pointer.x - left).abs(), half_x),
        (Handle::E, within_y, (pointer.x - right).abs(), half_x),
    ];
    let edge = edges
        .iter()
        .filter(|(_, along, dist, half)| *along && *dist <= *half)
        .min_by_key(|(_, _, dist, _)| *dist);
    if let Some((handle, _, _, _)) = edge {
        return Some(Target::Resize(*handle));
    }

    (within_x && within_y).then_some(Target::Move)
}

/// Moves `original` by the pointer's offset from `grab` without resizing it.
/// The Zone stays within the monitor (clamped), and snaps flush to a monitor
/// edge when a side lands within [`EDGE_SNAP_PX`] of it.
fn move_zone(original: Zone, grab: Point, pointer: Point, monitor: Monitor) -> Zone {
    let max_x = (monitor.width - original.width).max(0);
    let max_y = (monitor.height - original.height).max(0);
    let x = (original.x + pointer.x - grab.x).clamp(0, max_x);
    let y = (original.y + pointer.y - grab.y).clamp(0, max_y);
    Zone {
        x: snap_axis(x, original.width, monitor.width),
        y: snap_axis(y, original.height, monitor.height),
        width: original.width,
        height: original.height,
    }
}

/// Snaps a moved span's start so its near side or far side lands flush on
/// the monitor boundary when within [`EDGE_SNAP_PX`].
fn snap_axis(start: i32, len: i32, extent: i32) -> i32 {
    if snap_to_edge(start, 0) == 0 {
        0
    } else if snap_to_edge(start + len, extent) == extent {
        (extent - len).max(0)
    } else {
        start
    }
}

/// Edge-minus-pointer offset at a press, for the axes handle owns.
fn grab_offset(zone: &Zone, handle: Handle, press: Point) -> Point {
    let x = match handle {
        Handle::W | Handle::Nw | Handle::Sw => zone.x - press.x,
        Handle::E | Handle::Ne | Handle::Se => zone.x + zone.width - press.x,
        _ => 0,
    };
    let y = match handle {
        Handle::N | Handle::Nw | Handle::Ne => zone.y - press.y,
        Handle::S | Handle::Sw | Handle::Se => zone.y + zone.height - press.y,
        _ => 0,
    };
    Point { x, y }
}

/// Applies a live pointer position to a single handle drag: only the
/// coordinate(s) that handle's own edges own are allowed to move; edges
/// the handle doesn't touch stay put. Each moving edge is clamped against
/// its opposite edge (min 1px) and the monitor bounds.
fn apply_handle_drag(original: Zone, handle: Handle, pointer: Point, monitor: Monitor) -> Zone {
    let mut left = original.x;
    let mut top = original.y;
    let mut right = original.x + original.width;
    let mut bottom = original.y + original.height;

    let touches_left = matches!(handle, Handle::W | Handle::Nw | Handle::Sw);
    let touches_right = matches!(handle, Handle::E | Handle::Ne | Handle::Se);
    let touches_top = matches!(handle, Handle::N | Handle::Nw | Handle::Ne);
    let touches_bottom = matches!(handle, Handle::S | Handle::Sw | Handle::Se);

    if touches_left {
        left = snap_to_edge(pointer.x.clamp(0, right - 1), 0);
    }
    if touches_right {
        right = snap_to_edge(pointer.x.clamp(left + 1, monitor.width), monitor.width);
    }
    if touches_top {
        top = snap_to_edge(pointer.y.clamp(0, bottom - 1), 0);
    }
    if touches_bottom {
        bottom = snap_to_edge(pointer.y.clamp(top + 1, monitor.height), monitor.height);
    }

    Zone {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALES: [f32; 4] = [1.0, 1.25, 1.5, 2.0];

    #[test]
    fn scale_from_dpi_is_dpi_over_96() {
        assert_eq!(scale_from_dpi(96), 1.0);
        assert_eq!(scale_from_dpi(120), 1.25);
        assert_eq!(scale_from_dpi(144), 1.5);
        assert_eq!(scale_from_dpi(192), 2.0);
    }

    #[test]
    fn points_to_pixels_multiplies_by_scale_and_rounds_to_nearest() {
        assert_eq!(points_to_pixels(100.0, 1.0), 100);
        assert_eq!(points_to_pixels(100.0, 1.25), 125);
        assert_eq!(points_to_pixels(100.0, 1.5), 150);
        assert_eq!(points_to_pixels(100.0, 2.0), 200);
        // Odd results round to the nearest pixel.
        assert_eq!(points_to_pixels(101.0, 1.25), 126); // 126.25
        assert_eq!(points_to_pixels(101.0, 1.5), 152); // 151.5
        assert_eq!(points_to_pixels(0.3, 1.25), 0); // 0.375
        assert_eq!(points_to_pixels(-10.0, 1.5), -15);
    }

    #[test]
    fn pixels_to_points_divides_by_scale() {
        assert_eq!(pixels_to_points(1920, 1.0), 1920.0);
        assert_eq!(pixels_to_points(1920, 1.25), 1536.0);
        assert_eq!(pixels_to_points(1920, 1.5), 1280.0);
        assert_eq!(pixels_to_points(1920, 2.0), 960.0);
        assert_eq!(pixels_to_points(1081, 2.0), 540.5);
    }

    #[test]
    fn pixels_survive_a_round_trip_through_points_at_every_scale() {
        for scale in SCALES {
            for px in (-3000..=8000).chain([1, 3, 1079, 1081, 2559, 3839]) {
                assert_eq!(
                    points_to_pixels(pixels_to_points(px, scale), scale),
                    px,
                    "px {px} at scale {scale}"
                );
            }
        }
    }

    #[test]
    fn a_drawn_zone_converts_to_the_pixels_it_was_drawn_over_at_150_percent() {
        // Dragging from point (100, 50) to (500, 350) at 150 % covers
        // physical pixels (150, 75) to (750, 525).
        let a = point_from_points(100.0, 50.0, 1.5);
        let b = point_from_points(500.0, 350.0, 1.5);
        assert_eq!(a, Point { x: 150, y: 75 });
        assert_eq!(b, Point { x: 750, y: 525 });
    }

    fn monitor() -> Monitor {
        Monitor {
            width: 1920,
            height: 1080,
        }
    }

    fn placed(zone: Zone) -> AppState {
        AppState::new_with_zone(monitor(), zone)
    }

    #[test]
    fn new_state_machine_starts_empty() {
        let state = AppState::new(monitor());
        assert_eq!(state.editor, EditorState::Empty);
    }

    #[test]
    fn new_state_shows_draw_instruction_before_any_drag_starts() {
        let state = AppState::new(monitor());
        assert_eq!(state.last_message, "Click and drag to draw a zone");
    }

    #[test]
    fn new_with_zone_starts_placed_with_the_given_zone() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let state = AppState::new_with_zone(monitor(), zone);
        assert_eq!(state.editor, EditorState::Placed { zone });
    }

    #[test]
    fn start_from_empty_transitions_to_dragging() {
        let mut state = AppState::new(monitor());
        let outcome = state.apply(Action::Start(Point { x: 10, y: 20 }));
        assert_eq!(outcome, None);
        assert_eq!(
            state.editor,
            EditorState::Dragging {
                anchor: Point { x: 10, y: 20 },
                current: Point { x: 10, y: 20 }
            }
        );
    }

    #[test]
    fn start_is_illegal_outside_empty_and_leaves_state_unchanged() {
        let zone = Zone {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let mut state = placed(zone);
        let before = state.editor.clone();

        let outcome = state.apply(Action::Start(Point { x: 5, y: 5 }));

        assert_eq!(outcome, None);
        assert_eq!(state.editor, before);
    }

    #[test]
    fn drag_while_dragging_updates_the_live_current_point() {
        let mut state = AppState::new(monitor());
        state.apply(Action::Start(Point { x: 10, y: 10 }));

        state.apply(Action::Drag(Point { x: 200, y: 150 }));

        assert_eq!(
            state.editor,
            EditorState::Dragging {
                anchor: Point { x: 10, y: 10 },
                current: Point { x: 200, y: 150 }
            }
        );
    }

    #[test]
    fn drag_is_illegal_from_empty() {
        let mut state = AppState::new(monitor());
        let outcome = state.apply(Action::Drag(Point { x: 1, y: 1 }));
        assert_eq!(outcome, None);
        assert_eq!(state.editor, EditorState::Empty);
    }

    #[test]
    fn commit_from_dragging_normalizes_into_a_placed_zone() {
        let mut state = AppState::new(monitor());
        state.apply(Action::Start(Point { x: 300, y: 250 }));
        state.apply(Action::Drag(Point { x: 100, y: 50 }));

        let outcome = state.apply(Action::Commit);

        assert_eq!(outcome, None);
        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: Zone {
                    x: 100,
                    y: 50,
                    width: 200,
                    height: 200
                }
            }
        );
    }

    #[test]
    fn commit_clamps_a_drag_that_overshoots_the_monitor_bounds() {
        let mut state = AppState::new(Monitor {
            width: 800,
            height: 600,
        });
        state.apply(Action::Start(Point { x: -50, y: -50 }));
        state.apply(Action::Drag(Point { x: 5000, y: 5000 }));

        state.apply(Action::Commit);

        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: Zone {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600
                }
            }
        );
    }

    #[test]
    fn commit_snaps_a_drag_within_threshold_of_the_monitor_edges_flush_to_them() {
        let mut state = AppState::new(monitor());
        state.apply(Action::Start(Point { x: 10, y: 10 }));
        state.apply(Action::Drag(Point { x: 1910, y: 1070 }));

        state.apply(Action::Commit);

        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: Zone {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080
                }
            }
        );
    }

    #[test]
    fn commit_does_not_snap_a_drag_just_outside_the_edge_threshold() {
        let mut state = AppState::new(monitor());
        state.apply(Action::Start(Point { x: 21, y: 21 }));
        state.apply(Action::Drag(Point { x: 1899, y: 1059 }));

        state.apply(Action::Commit);

        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: Zone {
                    x: 21,
                    y: 21,
                    width: 1878,
                    height: 1038
                }
            }
        );
    }

    #[test]
    fn commit_is_illegal_from_empty_and_leaves_state_unchanged() {
        let mut state = AppState::new(monitor());
        let outcome = state.apply(Action::Commit);
        assert_eq!(outcome, None);
        assert_eq!(state.editor, EditorState::Empty);
    }

    #[test]
    fn grab_is_illegal_from_empty() {
        let mut state = AppState::new(monitor());
        let outcome = grab_on_outline(&mut state, Handle::E);
        assert_eq!(outcome, None);
        assert_eq!(state.editor, EditorState::Empty);
    }

    #[test]
    fn drag_while_resizing_moves_only_the_grabbed_handles_edges() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let mut state = placed(zone);
        grab_on_outline(&mut state, Handle::E);

        state.apply(Action::Drag(Point { x: 800, y: 999 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                handle: Handle::E,
                offset: at(0, 0),
                // Only the right edge (E) should move; y-axis pointer motion
                // is ignored since E doesn't touch top/bottom.
                live: Zone {
                    x: 100,
                    y: 100,
                    width: 700,
                    height: 300
                }
            }
        );
    }

    #[test]
    fn resize_snaps_the_west_handle_within_threshold_to_the_left_edge() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let mut state = placed(zone);
        grab_on_outline(&mut state, Handle::W);

        state.apply(Action::Drag(Point { x: 15, y: 100 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                handle: Handle::W,
                offset: at(0, 0),
                live: Zone {
                    x: 0,
                    y: 100,
                    width: 500,
                    height: 300
                }
            }
        );
    }

    #[test]
    fn resize_snaps_the_east_handle_within_threshold_to_the_right_edge() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let mut state = placed(zone);
        grab_on_outline(&mut state, Handle::E);

        state.apply(Action::Drag(Point { x: 1910, y: 100 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                handle: Handle::E,
                offset: at(0, 0),
                live: Zone {
                    x: 100,
                    y: 100,
                    width: 1820,
                    height: 300
                }
            }
        );
    }

    #[test]
    fn resize_does_not_snap_the_grabbed_handle_just_outside_the_edge_threshold() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let mut state = placed(zone);
        grab_on_outline(&mut state, Handle::E);

        state.apply(Action::Drag(Point { x: 1899, y: 100 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                handle: Handle::E,
                offset: at(0, 0),
                live: Zone {
                    x: 100,
                    y: 100,
                    width: 1799,
                    height: 300
                }
            }
        );
    }

    #[test]
    fn commit_from_resizing_moves_back_to_placed_with_the_live_zone() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let mut state = placed(zone);
        grab_on_outline(&mut state, Handle::E);
        state.apply(Action::Drag(Point { x: 800, y: 100 }));

        let outcome = state.apply(Action::Commit);

        assert_eq!(outcome, None);
        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: Zone {
                    x: 100,
                    y: 100,
                    width: 700,
                    height: 300
                }
            }
        );
    }

    #[test]
    fn save_from_placed_returns_the_saved_zone() {
        let zone = Zone {
            x: 10,
            y: 20,
            width: 300,
            height: 400,
        };
        let mut state = placed(zone);

        let outcome = state.apply(Action::Save);

        assert_eq!(outcome, Some(SessionOutcome::Saved(zone)));
    }

    #[test]
    fn save_is_illegal_from_empty() {
        let mut state = AppState::new(monitor());
        let outcome = state.apply(Action::Save);
        assert_eq!(outcome, None);
        assert_eq!(state.editor, EditorState::Empty);
    }

    #[test]
    fn save_is_illegal_from_dragging() {
        let mut state = AppState::new(monitor());
        state.apply(Action::Start(Point { x: 0, y: 0 }));

        let outcome = state.apply(Action::Save);

        assert_eq!(outcome, None);
    }

    #[test]
    fn save_is_illegal_from_resizing() {
        let zone = Zone {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let mut state = placed(zone);
        grab_on_outline(&mut state, Handle::E);

        let outcome = state.apply(Action::Save);

        assert_eq!(outcome, None);
    }

    #[test]
    fn cancel_is_legal_from_empty() {
        let mut state = AppState::new(monitor());
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    #[test]
    fn cancel_is_legal_from_dragging() {
        let mut state = AppState::new(monitor());
        state.apply(Action::Start(Point { x: 0, y: 0 }));
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    #[test]
    fn cancel_is_legal_from_placed() {
        let zone = Zone {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let mut state = placed(zone);
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    #[test]
    fn cancel_is_legal_from_resizing() {
        let zone = Zone {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let mut state = placed(zone);
        grab_on_outline(&mut state, Handle::E);
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    fn big_zone() -> Zone {
        Zone {
            x: 500,
            y: 300,
            width: 400,
            height: 300,
        }
    }

    fn at(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    /// Grabs `handle` pressing exactly on its outline point (no offset).
    fn grab_on_outline(state: &mut AppState, handle: Handle) -> Option<SessionOutcome> {
        let press = match &state.editor {
            EditorState::Placed { zone } => {
                let x = match handle {
                    Handle::W | Handle::Nw | Handle::Sw => zone.x,
                    Handle::E | Handle::Ne | Handle::Se => zone.x + zone.width,
                    _ => zone.x + zone.width / 2,
                };
                let y = match handle {
                    Handle::N | Handle::Nw | Handle::Ne => zone.y,
                    Handle::S | Handle::Sw | Handle::Se => zone.y + zone.height,
                    _ => zone.y + zone.height / 2,
                };
                at(x, y)
            }
            _ => at(0, 0),
        };
        state.apply(Action::Grab(handle, press))
    }

    #[test]
    fn resize_grab_off_the_outline_does_not_make_the_edge_jump() {
        // E edge at x=500; press 8 px outside it, then drag 10 px right.
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let mut state = placed(zone);
        state.apply(Action::Grab(Handle::E, at(508, 250)));
        // No movement yet: the edge stays at 500.
        state.apply(Action::Drag(at(508, 250)));
        let EditorState::Resizing { live, .. } = state.editor.clone() else {
            panic!("expected Resizing");
        };
        assert_eq!(live, zone);
        state.apply(Action::Drag(at(518, 250)));
        let EditorState::Resizing { live, .. } = state.editor else {
            panic!("expected Resizing");
        };
        assert_eq!((live.x, live.width), (100, 410));
    }

    #[test]
    fn corner_grab_off_the_outline_offsets_both_axes() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let mut state = placed(zone);
        // Nw corner at (100,100); press 6 px inside it on each axis.
        state.apply(Action::Grab(Handle::Nw, at(106, 106)));
        state.apply(Action::Drag(at(56, 76)));
        let EditorState::Resizing { live, .. } = state.editor else {
            panic!("expected Resizing");
        };
        assert_eq!(
            live,
            Zone {
                x: 50,
                y: 70,
                width: 450,
                height: 330
            }
        );
    }

    #[test]
    fn hit_test_small_zone_keeps_a_move_area_in_its_middle() {
        let z = Zone {
            x: 500,
            y: 300,
            width: 30,
            height: 30,
        };
        assert_eq!(hit_test(&z, at(515, 315), 40), Some(Target::Move));
        assert_eq!(
            hit_test(&z, at(500, 315), 40),
            Some(Target::Resize(Handle::W))
        );
        assert_eq!(
            hit_test(&z, at(515, 300), 40),
            Some(Target::Resize(Handle::N))
        );
        assert_eq!(
            hit_test(&z, at(500, 300), 40),
            Some(Target::Resize(Handle::Nw))
        );
    }
    #[test]
    fn hit_test_finds_each_corner() {
        let z = big_zone();
        assert_eq!(
            hit_test(&z, at(500, 300), 40),
            Some(Target::Resize(Handle::Nw))
        );
        assert_eq!(
            hit_test(&z, at(900, 300), 40),
            Some(Target::Resize(Handle::Ne))
        );
        assert_eq!(
            hit_test(&z, at(500, 600), 40),
            Some(Target::Resize(Handle::Sw))
        );
        assert_eq!(
            hit_test(&z, at(900, 600), 40),
            Some(Target::Resize(Handle::Se))
        );
    }

    #[test]
    fn hit_test_corner_grab_area_straddles_the_outline() {
        let z = big_zone();
        // 20 px (half of 40) outside and inside both hit; 21 px does not.
        assert_eq!(
            hit_test(&z, at(480, 280), 40),
            Some(Target::Resize(Handle::Nw))
        );
        assert_eq!(
            hit_test(&z, at(520, 320), 40),
            Some(Target::Resize(Handle::Nw))
        );
        assert_eq!(hit_test(&z, at(479, 280), 40), None);
    }

    #[test]
    fn hit_test_finds_edges_anywhere_along_them() {
        let z = big_zone();
        assert_eq!(
            hit_test(&z, at(700, 300), 40),
            Some(Target::Resize(Handle::N))
        );
        assert_eq!(
            hit_test(&z, at(600, 610), 40),
            Some(Target::Resize(Handle::S))
        );
        assert_eq!(
            hit_test(&z, at(495, 450), 40),
            Some(Target::Resize(Handle::W))
        );
        assert_eq!(
            hit_test(&z, at(910, 350), 40),
            Some(Target::Resize(Handle::E))
        );
    }

    #[test]
    fn hit_test_interior_moves_the_zone() {
        let z = big_zone();
        assert_eq!(hit_test(&z, at(700, 450), 40), Some(Target::Move));
        assert_eq!(hit_test(&z, at(530, 330), 40), Some(Target::Move));
    }

    #[test]
    fn hit_test_outside_the_zone_hits_nothing() {
        let z = big_zone();
        assert_eq!(hit_test(&z, at(100, 100), 40), None);
        assert_eq!(hit_test(&z, at(700, 200), 40), None);
        assert_eq!(hit_test(&z, at(1000, 450), 40), None);
    }

    #[test]
    fn hit_test_on_a_thin_zone_picks_the_nearer_edge() {
        let z = Zone {
            x: 500,
            y: 300,
            width: 400,
            height: 10,
        };
        assert_eq!(
            hit_test(&z, at(700, 302), 40),
            Some(Target::Resize(Handle::N))
        );
        assert_eq!(
            hit_test(&z, at(700, 308), 40),
            Some(Target::Resize(Handle::S))
        );
    }

    #[test]
    fn grab_move_then_drag_moves_the_zone_by_the_pointer_offset_keeping_its_size() {
        let mut state = placed(big_zone());
        state.apply(Action::GrabMove(at(700, 450)));
        state.apply(Action::Drag(at(760, 400)));

        assert_eq!(
            state.editor,
            EditorState::Moving {
                original: big_zone(),
                grab: at(700, 450),
                live: Zone {
                    x: 560,
                    y: 250,
                    width: 400,
                    height: 300
                }
            }
        );
    }

    #[test]
    fn commit_from_moving_places_the_moved_zone() {
        let mut state = placed(big_zone());
        state.apply(Action::GrabMove(at(700, 450)));
        state.apply(Action::Drag(at(760, 400)));

        assert_eq!(state.apply(Action::Commit), None);

        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: Zone {
                    x: 560,
                    y: 250,
                    width: 400,
                    height: 300
                }
            }
        );
    }

    #[test]
    fn moving_clamps_the_zone_to_the_monitor() {
        let mut state = placed(big_zone());
        state.apply(Action::GrabMove(at(700, 450)));

        state.apply(Action::Drag(at(-5000, -5000)));
        let EditorState::Moving { live, .. } = state.editor.clone() else {
            panic!("expected Moving");
        };
        assert_eq!((live.x, live.y), (0, 0));

        state.apply(Action::Drag(at(9000, 9000)));
        let EditorState::Moving { live, .. } = state.editor.clone() else {
            panic!("expected Moving");
        };
        assert_eq!((live.x, live.y), (1920 - 400, 1080 - 300));
        assert_eq!((live.width, live.height), (400, 300));
    }

    #[test]
    fn moving_snaps_a_side_within_threshold_flush_to_the_monitor_edge() {
        let mut state = placed(big_zone());
        state.apply(Action::GrabMove(at(700, 450)));

        // Left side lands at x=10, top at y=1080-300-15=765 (bottom 15 px short).
        state.apply(Action::Drag(at(700 - 490, 450 + 465)));

        let EditorState::Moving { live, .. } = state.editor else {
            panic!("expected Moving");
        };
        assert_eq!((live.x, live.y), (0, 780));
    }

    #[test]
    fn moving_does_not_snap_just_outside_the_threshold() {
        let mut state = placed(big_zone());
        state.apply(Action::GrabMove(at(700, 450)));

        // Left side lands at x=21.
        state.apply(Action::Drag(at(700 - 479, 450)));

        let EditorState::Moving { live, .. } = state.editor else {
            panic!("expected Moving");
        };
        assert_eq!(live.x, 21);
    }

    #[test]
    fn grab_move_is_illegal_from_empty() {
        let mut state = AppState::new(monitor());
        assert_eq!(state.apply(Action::GrabMove(at(1, 1))), None);
        assert_eq!(state.editor, EditorState::Empty);
    }

    #[test]
    fn cancel_is_legal_from_moving_and_save_is_not() {
        let mut state = placed(big_zone());
        state.apply(Action::GrabMove(at(700, 450)));
        assert_eq!(state.apply(Action::Save), None);
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    #[test]
    fn grabbing_any_of_the_eight_handles_on_a_placed_zone_transitions_to_resizing() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        for handle in Handle::ALL {
            let mut state = placed(zone);

            let outcome = grab_on_outline(&mut state, handle);

            assert_eq!(outcome, None);
            assert_eq!(
                state.editor,
                EditorState::Resizing {
                    original: zone,
                    handle,
                    offset: at(0, 0),
                    live: zone
                },
                "{handle:?} should start a resize"
            );
        }
    }
}
