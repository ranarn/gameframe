//! PROTOTYPE — throwaway. Pure state machine for wayfinder ticket #18
//! ("Zone overlay: drag-and-drop interaction model"). No I/O, no terminal
//! code — `main.rs` is a thin TUI shell over this module. See `NOTES.md`
//! for the question being answered and the verdict once captured.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Zone {
    fn right(&self) -> i32 {
        self.x + self.width
    }
    fn bottom(&self) -> i32 {
        self.y + self.height
    }
}

/// Origin is always (0, 0) for this prototype — good enough to exercise the
/// interaction model without pulling in multi-monitor geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub width: i32,
    pub height: i32,
}

/// Which of a zone's four edges are pinned to the monitor boundary and
/// therefore not draggable. Derived from which preset is being edited —
/// deliberately independent of ticket #20's still-undecided `Preset` enum
/// shape, so this ticket's answer doesn't have to wait on that one.
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

    /// A handle is enabled only if every edge it would move is unpinned — an
    /// edge handle needs its one edge free, a corner handle needs BOTH
    /// adjacent edges free. This is what makes e.g. a Left-Half preset
    /// expose only its `E` handle (right edge) and no corner handles at
    /// all, since top/bottom are pinned to the screen edge.
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

#[derive(Debug, Clone, PartialEq)]
pub enum EditorState {
    /// No zone exists yet — nothing drawn.
    Empty,
    /// Corner-to-corner initial draw, live between the anchor and the
    /// current pointer position.
    Dragging { anchor: Point, current: Point },
    /// A zone exists and is idle; `pinned` determines which handles render
    /// as active.
    Placed { zone: Zone, pinned: PinnedEdges },
    /// A specific handle is being dragged; `original` is kept so `Cancel`
    /// can discard back to it without needing a separate undo stack.
    Resizing { original: Zone, pinned: PinnedEdges, handle: Handle, live: Zone },
}

/// Whether the OS taskbar is currently forced into auto-hide for the
/// duration of the editing session (ticket #18's "must support zones that
/// extend below the taskbar" requirement) — reused from `win32`'s existing
/// `TaskbarAutohideState`/`toggle_taskbar_autohide` machinery in the real
/// implementation, not reinvented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarState {
    AlwaysVisible,
    AutoHidden,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub monitor: Monitor,
    pub editor: EditorState,
    pub taskbar: TaskbarState,
    pub last_message: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    /// Start drawing a brand-new freeform (fully unpinned) zone from
    /// scratch — simulates picking "Add custom zone" in Settings.
    New,
    /// Open the editor pre-loaded with an existing preset's bounds —
    /// simulates clicking "Edit" on an already-configured preset row.
    EditPreset { pinned: PinnedEdges, zone: Zone },
    Start(Point),
    Drag(Point),
    /// Finish the current drag or resize, committing it to `Placed`.
    Commit,
    Grab(Handle),
    Save,
    /// Discard any in-progress edit and close the editing session.
    Cancel,
}

impl AppState {
    pub fn new(monitor: Monitor) -> Self {
        Self {
            monitor,
            editor: EditorState::Empty,
            taskbar: TaskbarState::AlwaysVisible,
            last_message: "ready".to_string(),
        }
    }

    pub fn apply(&mut self, action: Action) {
        match action {
            Action::New => {
                self.editor = EditorState::Empty;
                self.taskbar = TaskbarState::AlwaysVisible;
                self.last_message = "cleared — draw a new zone with `start x y`".to_string();
            }
            Action::EditPreset { pinned, zone } => {
                self.editor = EditorState::Placed { zone, pinned };
                self.taskbar = TaskbarState::AutoHidden;
                self.last_message = "editing existing preset — grab an enabled handle".to_string();
            }
            Action::Start(p) => match self.editor {
                EditorState::Empty => {
                    self.editor = EditorState::Dragging { anchor: p, current: p };
                    self.taskbar = TaskbarState::AutoHidden;
                    self.last_message = "drawing from corner — `drag x y` then `commit`".to_string();
                }
                _ => self.reject("`start` is only legal from Empty (use `new` first)"),
            },
            Action::Drag(p) => match &mut self.editor {
                EditorState::Dragging { current, .. } => {
                    *current = p;
                    self.last_message = "live preview updated".to_string();
                }
                EditorState::Resizing { original, pinned, handle, live } => {
                    *live = apply_handle_drag(*original, *pinned, *handle, p, self.monitor);
                    self.last_message = "live resize updated".to_string();
                }
                _ => self.reject("`drag` needs an active draw or resize (`start` or `grab` first)"),
            },
            Action::Commit => match &self.editor {
                EditorState::Dragging { anchor, current } => {
                    let zone = normalize(*anchor, *current, self.monitor);
                    self.editor = EditorState::Placed { zone, pinned: PinnedEdges::NONE };
                    self.last_message = "zone drawn — `save` or grab a handle to adjust".to_string();
                }
                EditorState::Resizing { pinned, live, .. } => {
                    self.editor = EditorState::Placed { zone: *live, pinned: *pinned };
                    self.last_message = "resize committed".to_string();
                }
                _ => self.reject("nothing to commit"),
            },
            Action::Grab(handle) => match &self.editor {
                EditorState::Placed { zone, pinned } => {
                    if handle.enabled(*pinned) {
                        self.editor =
                            EditorState::Resizing { original: *zone, pinned: *pinned, handle, live: *zone };
                        self.last_message = format!("resizing via {handle:?} — `drag x y` then `commit`");
                    } else {
                        self.reject(&format!("{handle:?} is locked — pinned edges for this preset"));
                    }
                }
                _ => self.reject("`grab` needs a placed zone"),
            },
            Action::Save => match &self.editor {
                EditorState::Placed { zone, .. } => {
                    self.last_message = format!("saved: {zone:?}");
                    self.taskbar = TaskbarState::AlwaysVisible;
                }
                _ => self.reject("`save` needs a placed zone — finish the drag/resize first"),
            },
            Action::Cancel => {
                self.editor = EditorState::Empty;
                self.taskbar = TaskbarState::AlwaysVisible;
                self.last_message = "cancelled — discarded any in-progress edit".to_string();
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
fn normalize(anchor: Point, current: Point, monitor: Monitor) -> Zone {
    let x0 = anchor.x.min(current.x).clamp(0, monitor.width);
    let y0 = anchor.y.min(current.y).clamp(0, monitor.height);
    let x1 = anchor.x.max(current.x).clamp(0, monitor.width);
    let y1 = anchor.y.max(current.y).clamp(0, monitor.height);
    Zone { x: x0, y: y0, width: (x1 - x0).max(1), height: (y1 - y0).max(1) }
}

/// Applies a live pointer position to a single handle drag: only the
/// coordinate(s) that handle's own edges own are allowed to move; pinned
/// edges (and edges the handle doesn't touch at all) stay put. Each moving
/// edge is clamped against its opposite edge (min 1px) and the monitor
/// bounds.
fn apply_handle_drag(original: Zone, pinned: PinnedEdges, handle: Handle, pointer: Point, monitor: Monitor) -> Zone {
    let mut left = original.x;
    let mut top = original.y;
    let mut right = original.right();
    let mut bottom = original.bottom();

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

    Zone { x: left, y: top, width: right - left, height: bottom - top }
}
