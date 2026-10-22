//! Pure state machine for the zone-editor overlay's drag-and-draw / resize
//! interaction model (issue #28). Lifted directly from the throwaway
//! prototype validated at `examples/zone_overlay_prototype/zone_editor.rs`
//! (see that file's module doc for the original research question), with
//! two production differences:
//! - Uses [`crate::profile_store::Zone`] in place of the
//!   prototype's own `Zone` type (identical fields: `x`/`y`/`width`/
//!   `height`, all `i32`) so [`AppState::apply`]'s `Save` output plugs
//!   directly into `Config::zone` with no conversion step.
//! - Drops the prototype's simulated `TaskbarState` entirely -- the real
//!   mode-aware taskbar-hide/restore mechanism lives in [`crate::win32`]
//!   (`hide_taskbar`/`restore_taskbar`) and is driven by the OS-facing
//!   overlay window ([`crate::zone_overlay`]) around the editing session,
//!   not modeled as part of this pure state machine.
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
    pub const ALL: [Handle; 8] =
        [Handle::N, Handle::S, Handle::E, Handle::W, Handle::Ne, Handle::Nw, Handle::Se, Handle::Sw];
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
    Resizing { original: Zone, handle: Handle, live: Zone },
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
    Grab(Handle),
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
                        self.editor = EditorState::Dragging { anchor: p, current: p };
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
                    EditorState::Resizing { original, handle, live } => {
                        *live = apply_handle_drag(*original, *handle, p, self.monitor);
                        self.last_message = "live resize updated".to_string();
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
                    _ => self.reject("nothing to commit"),
                }
                None
            }
            Action::Grab(handle) => {
                match &self.editor {
                    EditorState::Placed { zone } => {
                        self.editor = EditorState::Resizing { original: *zone, handle, live: *zone };
                        self.last_message = format!("resizing via {handle:?}");
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

/// Snaps `value` to `edge` when within [`EDGE_SNAP_PX`] of it.
fn snap_to_edge(value: i32, edge: i32) -> i32 {
    if (value - edge).abs() <= EDGE_SNAP_PX { edge } else { value }
}

/// Normalizes a two-corner drag into a positive-area zone, clamped to the
/// monitor. Mirrors `region_calc::resolve_region`'s Zone semantics
/// (clamp to fit rather than reject) since a live drag has no "invalid
/// input" concept the way a typed X/Y/width/height field does.
fn normalize(anchor: Point, current: Point, monitor: Monitor) -> Zone {
    let x0 = snap_to_edge(anchor.x.min(current.x).clamp(0, monitor.width), 0);
    let y0 = snap_to_edge(anchor.y.min(current.y).clamp(0, monitor.height), 0);
    let x1 = snap_to_edge(anchor.x.max(current.x).clamp(0, monitor.width), monitor.width);
    let y1 = snap_to_edge(anchor.y.max(current.y).clamp(0, monitor.height), monitor.height);
    Zone { x: x0, y: y0, width: (x1 - x0).max(1), height: (y1 - y0).max(1) }
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

    Zone { x: left, y: top, width: right - left, height: bottom - top }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> Monitor {
        Monitor { width: 1920, height: 1080 }
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
        let zone = Zone { x: 100, y: 100, width: 400, height: 300 };
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
            EditorState::Dragging { anchor: Point { x: 10, y: 20 }, current: Point { x: 10, y: 20 } }
        );
    }

    #[test]
    fn start_is_illegal_outside_empty_and_leaves_state_unchanged() {
        let zone = Zone { x: 0, y: 0, width: 100, height: 100 };
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
            EditorState::Dragging { anchor: Point { x: 10, y: 10 }, current: Point { x: 200, y: 150 } }
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
                zone: Zone { x: 100, y: 50, width: 200, height: 200 }
            }
        );
    }

    #[test]
    fn commit_clamps_a_drag_that_overshoots_the_monitor_bounds() {
        let mut state = AppState::new(Monitor { width: 800, height: 600 });
        state.apply(Action::Start(Point { x: -50, y: -50 }));
        state.apply(Action::Drag(Point { x: 5000, y: 5000 }));

        state.apply(Action::Commit);

        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: Zone { x: 0, y: 0, width: 800, height: 600 }
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
                zone: Zone { x: 0, y: 0, width: 1920, height: 1080 }
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
                zone: Zone { x: 21, y: 21, width: 1878, height: 1038 }
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
        let outcome = state.apply(Action::Grab(Handle::E));
        assert_eq!(outcome, None);
        assert_eq!(state.editor, EditorState::Empty);
    }

    #[test]
    fn drag_while_resizing_moves_only_the_grabbed_handles_edges() {
        let zone = Zone { x: 100, y: 100, width: 400, height: 300 };
        let mut state = placed(zone);
        state.apply(Action::Grab(Handle::E));

        state.apply(Action::Drag(Point { x: 800, y: 999 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                handle: Handle::E,
                // Only the right edge (E) should move; y-axis pointer motion
                // is ignored since E doesn't touch top/bottom.
                live: Zone { x: 100, y: 100, width: 700, height: 300 }
            }
        );
    }

    #[test]
    fn resize_snaps_the_west_handle_within_threshold_to_the_left_edge() {
        let zone = Zone { x: 100, y: 100, width: 400, height: 300 };
        let mut state = placed(zone);
        state.apply(Action::Grab(Handle::W));

        state.apply(Action::Drag(Point { x: 15, y: 100 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                handle: Handle::W,
                live: Zone { x: 0, y: 100, width: 500, height: 300 }
            }
        );
    }

    #[test]
    fn resize_snaps_the_east_handle_within_threshold_to_the_right_edge() {
        let zone = Zone { x: 100, y: 100, width: 400, height: 300 };
        let mut state = placed(zone);
        state.apply(Action::Grab(Handle::E));

        state.apply(Action::Drag(Point { x: 1910, y: 100 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                handle: Handle::E,
                live: Zone { x: 100, y: 100, width: 1820, height: 300 }
            }
        );
    }

    #[test]
    fn resize_does_not_snap_the_grabbed_handle_just_outside_the_edge_threshold() {
        let zone = Zone { x: 100, y: 100, width: 400, height: 300 };
        let mut state = placed(zone);
        state.apply(Action::Grab(Handle::E));

        state.apply(Action::Drag(Point { x: 1899, y: 100 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                handle: Handle::E,
                live: Zone { x: 100, y: 100, width: 1799, height: 300 }
            }
        );
    }

    #[test]
    fn commit_from_resizing_moves_back_to_placed_with_the_live_zone() {
        let zone = Zone { x: 100, y: 100, width: 400, height: 300 };
        let mut state = placed(zone);
        state.apply(Action::Grab(Handle::E));
        state.apply(Action::Drag(Point { x: 800, y: 100 }));

        let outcome = state.apply(Action::Commit);

        assert_eq!(outcome, None);
        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: Zone { x: 100, y: 100, width: 700, height: 300 }
            }
        );
    }

    #[test]
    fn save_from_placed_returns_the_saved_zone() {
        let zone = Zone { x: 10, y: 20, width: 300, height: 400 };
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
        let zone = Zone { x: 0, y: 0, width: 100, height: 100 };
        let mut state = placed(zone);
        state.apply(Action::Grab(Handle::E));

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
        let zone = Zone { x: 0, y: 0, width: 100, height: 100 };
        let mut state = placed(zone);
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    #[test]
    fn cancel_is_legal_from_resizing() {
        let zone = Zone { x: 0, y: 0, width: 100, height: 100 };
        let mut state = placed(zone);
        state.apply(Action::Grab(Handle::E));
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    #[test]
    fn grabbing_any_of_the_eight_handles_on_a_placed_zone_transitions_to_resizing() {
        let zone = Zone { x: 100, y: 100, width: 400, height: 300 };
        for handle in Handle::ALL {
            let mut state = placed(zone);

            let outcome = state.apply(Action::Grab(handle));

            assert_eq!(outcome, None);
            assert_eq!(
                state.editor,
                EditorState::Resizing { original: zone, handle, live: zone },
                "{handle:?} should start a resize"
            );
        }
    }
}
