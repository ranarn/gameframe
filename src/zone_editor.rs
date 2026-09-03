//! Pure state machine for the zone-editor overlay's drag-and-draw / resize
//! interaction model (issue #28). Lifted directly from the throwaway
//! prototype validated at `examples/zone_overlay_prototype/zone_editor.rs`
//! (see that file's module doc for the original research question), with
//! two production differences:
//! - Uses [`crate::profile_store::CustomRegion`] in place of the
//!   prototype's own `Zone` type (identical fields: `x`/`y`/`width`/
//!   `height`, all `i32`) so [`AppState::apply`]'s `Save` output plugs
//!   directly into `Presets.custom` with no conversion step.
//! - Drops the prototype's simulated `TaskbarState` entirely -- the real
//!   taskbar-autohide force-on/restore-previous mechanism lives in
//!   [`crate::win32`] (`force_taskbar_autohide_on`/`restore_taskbar_autohide`)
//!   and is driven by the OS-facing overlay window
//!   ([`crate::zone_overlay`]) around the editing session, not modeled as
//!   part of this pure state machine.
//!
//! No window/OS dependency here -- fully unit tested below.

use crate::profile_store::CustomRegion;

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

/// Which of a zone's four edges are pinned to the monitor boundary and
/// therefore not draggable. Derived from which preset is being edited --
/// deliberately independent of [`crate::region_calc::Preset`]'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PinnedEdges {
    pub left: bool,
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
}

impl PinnedEdges {
    pub const NONE: Self = Self { left: false, top: false, right: false, bottom: false };
    pub const LEFT_HALF: Self = Self { left: true, top: true, bottom: true, right: false };
    pub const RIGHT_HALF: Self = Self { right: true, top: true, bottom: true, left: false };
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

    /// A handle is enabled only if every edge it would move is unpinned --
    /// an edge handle needs its one edge free, a corner handle needs BOTH
    /// adjacent edges free.
    pub fn enabled(self, pinned: PinnedEdges) -> bool {
        match self {
            Handle::N => !pinned.top,
            Handle::S => !pinned.bottom,
            Handle::E => !pinned.right,
            Handle::W => !pinned.left,
            Handle::Ne => !pinned.top && !pinned.right,
            Handle::Nw => !pinned.top && !pinned.left,
            Handle::Se => !pinned.bottom && !pinned.right,
            Handle::Sw => !pinned.bottom && !pinned.left,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorState {
    /// No zone exists yet -- nothing drawn.
    Empty,
    /// Corner-to-corner initial draw, live between the anchor and the
    /// current pointer position.
    Dragging { anchor: Point, current: Point },
    /// A zone exists and is idle; `pinned` determines which handles render
    /// as active.
    Placed { zone: CustomRegion, pinned: PinnedEdges },
    /// A specific handle is being dragged; `original` is kept so `Cancel`
    /// can discard back to it without needing a separate undo stack.
    Resizing { original: CustomRegion, pinned: PinnedEdges, handle: Handle, live: CustomRegion },
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
    Saved(CustomRegion),
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
    /// already-configured Custom preset), so resizing it doesn't require
    /// redrawing from scratch.
    pub fn new_with_zone(monitor: Monitor, pinned: PinnedEdges, zone: CustomRegion) -> Self {
        Self {
            monitor,
            editor: EditorState::Placed { zone, pinned },
            last_message: "editing existing zone -- grab an enabled handle".to_string(),
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
                    EditorState::Resizing { original, pinned, handle, live } => {
                        *live = apply_handle_drag(*original, *pinned, *handle, p, self.monitor);
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
                        self.editor = EditorState::Placed { zone, pinned: PinnedEdges::NONE };
                        self.last_message = "zone drawn".to_string();
                    }
                    EditorState::Resizing { pinned, live, .. } => {
                        self.editor = EditorState::Placed { zone: *live, pinned: *pinned };
                        self.last_message = "resize committed".to_string();
                    }
                    _ => self.reject("nothing to commit"),
                }
                None
            }
            Action::Grab(handle) => {
                match &self.editor {
                    EditorState::Placed { zone, pinned } => {
                        if handle.enabled(*pinned) {
                            self.editor = EditorState::Resizing {
                                original: *zone,
                                pinned: *pinned,
                                handle,
                                live: *zone,
                            };
                            self.last_message = format!("resizing via {handle:?}");
                        } else {
                            self.reject(&format!("{handle:?} is locked -- pinned edges for this preset"));
                        }
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

/// Normalizes a two-corner drag into a positive-area zone, clamped to the
/// monitor. Mirrors `region_calc::resolve_region`'s Custom-preset semantics
/// (clamp to fit rather than reject) since a live drag has no "invalid
/// input" concept the way a typed X/Y/width/height field does.
fn normalize(anchor: Point, current: Point, monitor: Monitor) -> CustomRegion {
    let x0 = anchor.x.min(current.x).clamp(0, monitor.width);
    let y0 = anchor.y.min(current.y).clamp(0, monitor.height);
    let x1 = anchor.x.max(current.x).clamp(0, monitor.width);
    let y1 = anchor.y.max(current.y).clamp(0, monitor.height);
    CustomRegion { x: x0, y: y0, width: (x1 - x0).max(1), height: (y1 - y0).max(1) }
}

/// Applies a live pointer position to a single handle drag: only the
/// coordinate(s) that handle's own edges own are allowed to move; pinned
/// edges (and edges the handle doesn't touch at all) stay put. Each moving
/// edge is clamped against its opposite edge (min 1px) and the monitor
/// bounds.
fn apply_handle_drag(
    original: CustomRegion,
    pinned: PinnedEdges,
    handle: Handle,
    pointer: Point,
    monitor: Monitor,
) -> CustomRegion {
    let mut left = original.x;
    let mut top = original.y;
    let mut right = original.x + original.width;
    let mut bottom = original.y + original.height;

    let touches_left = matches!(handle, Handle::W | Handle::Nw | Handle::Sw);
    let touches_right = matches!(handle, Handle::E | Handle::Ne | Handle::Se);
    let touches_top = matches!(handle, Handle::N | Handle::Nw | Handle::Ne);
    let touches_bottom = matches!(handle, Handle::S | Handle::Sw | Handle::Se);

    if touches_left && !pinned.left {
        left = pointer.x.clamp(0, right - 1);
    }
    if touches_right && !pinned.right {
        right = pointer.x.clamp(left + 1, monitor.width);
    }
    if touches_top && !pinned.top {
        top = pointer.y.clamp(0, bottom - 1);
    }
    if touches_bottom && !pinned.bottom {
        bottom = pointer.y.clamp(top + 1, monitor.height);
    }

    CustomRegion { x: left, y: top, width: right - left, height: bottom - top }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> Monitor {
        Monitor { width: 1920, height: 1080 }
    }

    fn placed(zone: CustomRegion, pinned: PinnedEdges) -> AppState {
        AppState::new_with_zone(monitor(), pinned, zone)
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
    fn new_with_zone_starts_placed_with_the_given_zone_and_pinned_edges() {
        let zone = CustomRegion { x: 100, y: 100, width: 400, height: 300 };
        let state = AppState::new_with_zone(monitor(), PinnedEdges::LEFT_HALF, zone);
        assert_eq!(state.editor, EditorState::Placed { zone, pinned: PinnedEdges::LEFT_HALF });
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
        let zone = CustomRegion { x: 0, y: 0, width: 100, height: 100 };
        let mut state = placed(zone, PinnedEdges::NONE);
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
    fn commit_from_dragging_normalizes_into_a_placed_zone_with_no_pinned_edges() {
        let mut state = AppState::new(monitor());
        state.apply(Action::Start(Point { x: 300, y: 250 }));
        state.apply(Action::Drag(Point { x: 100, y: 50 }));

        let outcome = state.apply(Action::Commit);

        assert_eq!(outcome, None);
        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: CustomRegion { x: 100, y: 50, width: 200, height: 200 },
                pinned: PinnedEdges::NONE
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
                zone: CustomRegion { x: 0, y: 0, width: 800, height: 600 },
                pinned: PinnedEdges::NONE
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
    fn grab_an_enabled_handle_from_placed_transitions_to_resizing() {
        let zone = CustomRegion { x: 100, y: 100, width: 400, height: 300 };
        let mut state = placed(zone, PinnedEdges::NONE);

        let outcome = state.apply(Action::Grab(Handle::Se));

        assert_eq!(outcome, None);
        assert_eq!(
            state.editor,
            EditorState::Resizing { original: zone, pinned: PinnedEdges::NONE, handle: Handle::Se, live: zone }
        );
    }

    #[test]
    fn grab_a_handle_locked_by_pinned_edges_is_rejected_and_leaves_state_placed() {
        let zone = CustomRegion { x: 0, y: 0, width: 960, height: 1080 };
        let mut state = placed(zone, PinnedEdges::LEFT_HALF);

        // West edge is pinned under LEFT_HALF, so W (and both NW/SW corners,
        // which touch W) must be locked.
        let outcome = state.apply(Action::Grab(Handle::W));

        assert_eq!(outcome, None);
        assert_eq!(state.editor, EditorState::Placed { zone, pinned: PinnedEdges::LEFT_HALF });
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
        let zone = CustomRegion { x: 100, y: 100, width: 400, height: 300 };
        let mut state = placed(zone, PinnedEdges::NONE);
        state.apply(Action::Grab(Handle::E));

        state.apply(Action::Drag(Point { x: 800, y: 999 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                pinned: PinnedEdges::NONE,
                handle: Handle::E,
                // Only the right edge (E) should move; y-axis pointer motion
                // is ignored since E doesn't touch top/bottom.
                live: CustomRegion { x: 100, y: 100, width: 700, height: 300 }
            }
        );
    }

    #[test]
    fn drag_while_resizing_a_pinned_edge_handle_never_moves_a_pinned_edge() {
        // LEFT_HALF pins left/top/bottom -- only E is enabled/grabbable, and
        // dragging it must never move x/y/bottom even if the handle math is
        // asked to.
        let zone = CustomRegion { x: 0, y: 0, width: 960, height: 1080 };
        let mut state = placed(zone, PinnedEdges::LEFT_HALF);
        state.apply(Action::Grab(Handle::E));

        state.apply(Action::Drag(Point { x: 1500, y: 40 }));

        assert_eq!(
            state.editor,
            EditorState::Resizing {
                original: zone,
                pinned: PinnedEdges::LEFT_HALF,
                handle: Handle::E,
                live: CustomRegion { x: 0, y: 0, width: 1500, height: 1080 }
            }
        );
    }

    #[test]
    fn commit_from_resizing_moves_back_to_placed_with_the_live_zone() {
        let zone = CustomRegion { x: 100, y: 100, width: 400, height: 300 };
        let mut state = placed(zone, PinnedEdges::NONE);
        state.apply(Action::Grab(Handle::E));
        state.apply(Action::Drag(Point { x: 800, y: 100 }));

        let outcome = state.apply(Action::Commit);

        assert_eq!(outcome, None);
        assert_eq!(
            state.editor,
            EditorState::Placed {
                zone: CustomRegion { x: 100, y: 100, width: 700, height: 300 },
                pinned: PinnedEdges::NONE
            }
        );
    }

    #[test]
    fn save_from_placed_returns_the_saved_zone() {
        let zone = CustomRegion { x: 10, y: 20, width: 300, height: 400 };
        let mut state = placed(zone, PinnedEdges::NONE);

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
        let zone = CustomRegion { x: 0, y: 0, width: 100, height: 100 };
        let mut state = placed(zone, PinnedEdges::NONE);
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
        let zone = CustomRegion { x: 0, y: 0, width: 100, height: 100 };
        let mut state = placed(zone, PinnedEdges::NONE);
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    #[test]
    fn cancel_is_legal_from_resizing() {
        let zone = CustomRegion { x: 0, y: 0, width: 100, height: 100 };
        let mut state = placed(zone, PinnedEdges::NONE);
        state.apply(Action::Grab(Handle::E));
        assert_eq!(state.apply(Action::Cancel), Some(SessionOutcome::Cancelled));
    }

    #[test]
    fn pinned_edges_none_enables_all_eight_handles() {
        for handle in Handle::ALL {
            assert!(handle.enabled(PinnedEdges::NONE), "{handle:?} should be enabled under PinnedEdges::NONE");
        }
    }

    #[test]
    fn pinned_edges_left_half_enables_only_the_east_edge_handle() {
        let enabled: Vec<Handle> = Handle::ALL.into_iter().filter(|h| h.enabled(PinnedEdges::LEFT_HALF)).collect();
        assert_eq!(enabled, vec![Handle::E]);
    }

    #[test]
    fn pinned_edges_right_half_enables_only_the_west_edge_handle() {
        let enabled: Vec<Handle> = Handle::ALL.into_iter().filter(|h| h.enabled(PinnedEdges::RIGHT_HALF)).collect();
        assert_eq!(enabled, vec![Handle::W]);
    }

    #[test]
    fn a_corner_handle_needs_both_adjacent_edges_free() {
        // Pin only the top edge: Nw/Ne (which touch top) must be locked, but
        // Sw/Se (which don't touch top) stay enabled.
        let pinned = PinnedEdges { top: true, ..PinnedEdges::NONE };
        assert!(!Handle::Nw.enabled(pinned));
        assert!(!Handle::Ne.enabled(pinned));
        assert!(Handle::Sw.enabled(pinned));
        assert!(Handle::Se.enabled(pinned));
    }
}
