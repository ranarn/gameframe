//! OS-facing zone-editor overlay window: a fully transparent, borderless,
//! always-on-top egui/wgpu window covering the primary monitor's full
//! bounds (not the taskbar-excluded work area -- a drawn zone may extend
//! into the taskbar's screen strip), used to drag-draw or resize the
//! Zone interactively instead of typing X/Y/width/height
//! by hand.
//!
//! Rendering setup: the DX12 + DirectComposition
//! (`Dx12SwapchainKind::DxgiFromVisual`)-backed swapchain is required for
//! true per-pixel transparency on the tested hardware --
//! `ViewportBuilder::with_transparent(true)` alone renders opaque without
//! it -- and the `clear_color` override is required because eframe/wgpu
//! otherwise paints an opaque background behind the egui content regardless
//! of the viewport's transparency flag.
//!
//! Drives [`crate::zone_editor::AppState`] from real pointer input. The
//! taskbar is left as found when the editor opens; a control-box toggle hides
//! it via [`crate::taskbar::hide_taskbar`] (mode-aware -- see
//! [`crate::profile_store::TaskbarHideMode`]) and shows it again; the toggle
//! is disabled when the taskbar was already hidden at open. When the
//! session ends -- Save, Cancel, or the window being closed any other way --
//! a hide made by the editor is undone (see [`crate::zone_taskbar`];
//! an RAII guard, [`TaskbarRestoreGuard`], does it on drop, covering
//! early returns). A panic is covered separately: the release profile is
//! `panic = "abort"`, so the guard's `Drop` does NOT run then; instead the
//! panic hook ([`crate::taskbar_recovery::install_panic_hook`]) restores the
//! taskbar before the abort, and a kill is covered by the marker file
//! restored at the next start (see [`crate::taskbar_recovery`]).
//!
//! The control box's taskbar control is a toggle bound to the "Hide taskbar
//! when the Zone is applied" setting: it starts at the setting, hides/shows
//! the taskbar live, and sets what the overlay covers -- the full monitor when
//! ON, the work area when OFF so the taskbar stays visible and clickable. The
//! Zone stays in physical screen pixels whichever it covers (the overlay's own
//! origin moves). Save returns the Zone and the toggle value together
//! ([`ZoneEditorResult`]).
//!
//! OS/GPU-facing: the overlay itself is not exercised by this crate's
//! automated tests, for the same reason [`crate::settings_ui::run_settings_window`]
//! isn't -- a headless/CI sandbox cannot provide a real GPU-backed window to
//! render into or a real taskbar to hide. The pure interaction logic it
//! drives ([`crate::zone_editor`]) and the pure taskbar hide/remember
//! decision ([`crate::win32::plan_taskbar_hide`]) are fully unit tested
//! instead, and [`strip_window_frame`] is tested on a plain Win32 window.

use crate::profile_store::{TaskbarHideMode, Zone};
use crate::region_calc::MonitorGeometry;
use crate::taskbar::taskbar_hotkey_warning;
use crate::win32;
use crate::zone_editor::{
    Action, AppState, EditorState, Handle, Monitor, Point, SessionOutcome, Target, hit_test,
    pixels_to_points, point_from_points, points_to_pixels, scale_from_dpi,
};
use crate::zone_taskbar::{TaskbarRestoreGuard, TaskbarSession};
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows_sys::Win32::Foundation::{HWND, POINT, SetLastError};
use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
    WS_SYSMENU, WS_THICKFRAME,
};

/// The title of the zone editor's window (also what the hang watchdog of the
/// zone-editor child process finds it by).
pub(crate) const ZONE_EDITOR_TITLE: &str = "GameFrame zone editor";

/// Drawn size of a resize handle square, in egui points (scales with DPI, so
/// it stays usable at 100-200 %).
const HANDLE_DRAW_PT: f32 = 28.0;
/// Grab size of a handle, in egui points; larger than the drawn handle. Edge
/// bands extend half of this on each side of the outline, capped at a third
/// of the Zone's width/height so a small Zone keeps a Move area.
const HANDLE_HIT_PT: f32 = 40.0;
/// Gap, in egui points, between the Save/Cancel buttons and the taskbar toggle.
const TOGGLE_GAP_PT: f32 = 48.0;

/// Reasons opening the zone-editor overlay can fail.
#[derive(Debug)]
pub enum ZoneOverlayError {
    Window(eframe::Error),
}

impl std::fmt::Display for ZoneOverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZoneOverlayError::Window(e) => write!(f, "failed to open the zone editor overlay: {e}"),
        }
    }
}

impl std::error::Error for ZoneOverlayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ZoneOverlayError::Window(e) => Some(e),
        }
    }
}

/// Opens the zone-editor overlay, blocking until the user Saves, Cancels, or
/// closes the window some other way. Returns `Ok(Some(region))` only on a
/// legal Save; `Ok(None)` on Cancel or an unclean close.
///
/// `initial` pre-loads the overlay with an existing region (e.g. the
/// already-configured Zone) so resizing it doesn't require
/// redrawing from scratch. All eight resize handles are always active.
///
/// Leaves the taskbar as it is on open. The control box has a toggle that
/// hides/shows it using whichever technique `mode` selects (see module
/// docs); a hide the editor made is undone before returning --
/// whether the window ran cleanly, failed to run, or returned early -- via
/// the [`TaskbarRestoreGuard`] RAII guard held across the call. A panic
/// mid-session is handled by the panic hook, not the guard (see the module
/// docs).
///
/// `on_tick` is called every frame (at least every
/// [`crate::instance_channel::QUIT_POLL`]) so the owner of this window can
/// follow along: the Settings child reports which window is ticking to the
/// tray process (see [`crate::settings_link`]).
pub fn run_zone_editor_overlay(
    initial: Option<Zone>,
    hide_taskbar_on_apply: bool,
    taskbar_hotkey: &str,
    mode: TaskbarHideMode,
    on_tick: &mut dyn FnMut(),
) -> Result<Option<ZoneEditorResult>, ZoneOverlayError> {
    let monitor_geometry = crate::win32::primary_monitor_geometry();
    let monitor = Monitor {
        width: monitor_geometry.width,
        height: monitor_geometry.height,
    };

    // The taskbar's state is read once, here. Hidden already (a Zone-apply
    // path, the hotkey, Windows auto-hide): the toggle is locked and the
    // editor never touches it.
    let taskbar = Rc::new(RefCell::new(TaskbarSession::new(
        win32::taskbar_hidden_flag(),
    )));

    // RAII guard, not a manual "call restore after run_native" step, so
    // every non-panic exit path -- normal return and early `?` propagation
    // -- restores the taskbar. It does not run on a panic in the release
    // build (`panic = "abort"`); the panic hook installed at startup
    // (`crate::taskbar_recovery`) restores the taskbar in that case, e.g.
    // after a wgpu device-lost panic on sleep/resume or a monitor unplug.
    let _taskbar_restore_guard = TaskbarRestoreGuard::new(Rc::clone(&taskbar));

    // The toggle starts at the setting: ON hides the taskbar right away
    // (through the marked session, so the recovery marker rules hold). A
    // locked session ignores this; a failed hide shows the error but the
    // toggle stays at the setting. The toggle is the user's choice (what Save
    // persists); the live hide is only a preview of it.
    if hide_taskbar_on_apply {
        taskbar.borrow_mut().toggle(mode);
    }
    let hide_now = overlay_hides_taskbar(&taskbar.borrow(), hide_taskbar_on_apply);
    let bounds = overlay_bounds(hide_now, monitor_geometry, win32::primary_work_area);
    let choice = Rc::new(Cell::new(hide_taskbar_on_apply));
    let choice_for_app = Rc::clone(&choice);

    let state = match initial {
        Some(zone) => AppState::new_with_zone(monitor, zone),
        None => AppState::new(monitor),
    };

    let outcome: Rc<RefCell<Option<SessionOutcome>>> = Rc::new(RefCell::new(None));
    let outcome_for_app = Rc::clone(&outcome);

    let scale = primary_monitor_scale();
    let native_options = native_options(&bounds, scale);

    let run_result = eframe::run_native(
        ZONE_EDITOR_TITLE,
        native_options,
        Box::new(move |cc| {
            // The GPU path the overlay got, for tester logs. egui-wgpu logs
            // the chosen adapter only at `debug`, and the surface's alpha mode
            // is not exposed by egui-wgpu at all.
            match cc.wgpu_render_state.as_ref() {
                Some(rs) => log!(
                    "zone editor: wgpu adapter {:?}, target format {:?}",
                    rs.adapter.get_info(),
                    rs.target_format
                ),
                None => log!("zone editor: no wgpu render state"),
            }
            // WORKAROUND for egui #8750: strip the window frame so it can't
            // show through the transparent pixels, see `strip_window_frame`.
            // winit re-applies the frame when the window is first shown, so
            // `OverlayApp::ui` strips it again every frame.
            let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get() as HWND),
                other => {
                    log!("zone editor: no Win32 window handle ({other:?}); window frame kept");
                    None
                }
            };
            let app = OverlayApp {
                state,
                outcome: outcome_for_app,
                hwnd,
                taskbar,
                taskbar_mode: mode,
                hide_choice: choice_for_app,
                taskbar_hotkey: taskbar_hotkey.to_string(),
                monitor: monitor_geometry,
                bounds,
                on_tick,
            };
            app.strip_frame();
            Ok(Box::new(app))
        }),
    );

    run_result.map_err(ZoneOverlayError::Window)?;

    let outcome = outcome.borrow_mut().take();
    Ok(outcome.and_then(|o| ZoneEditorResult::from_outcome(o, choice.get())))
}

/// What a saved zone-editor session hands back: the Zone and the value of the
/// "Hide taskbar when the Zone is applied" toggle, which the caller persists
/// together (see `settings_ui::persist_editor_result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneEditorResult {
    pub zone: Zone,
    pub hide_taskbar_on_apply: bool,
}

impl ZoneEditorResult {
    /// The result of a finished session: `Some` only for a Save. Cancel (or an
    /// unclean close) has nothing to persist.
    pub fn from_outcome(outcome: SessionOutcome, hide_taskbar_on_apply: bool) -> Option<Self> {
        match outcome {
            SessionOutcome::Saved(zone) => Some(Self {
                zone,
                hide_taskbar_on_apply,
            }),
            SessionOutcome::Cancelled => None,
        }
    }
}

/// The screen rectangle the overlay covers, in physical pixels: the whole
/// monitor while the taskbar is being hidden, the work area (monitor minus
/// taskbar) while it is shown, so the taskbar stays visible and clickable.
fn overlay_bounds(
    hide_taskbar: bool,
    monitor: MonitorGeometry,
    work_area: impl FnOnce() -> MonitorGeometry,
) -> MonitorGeometry {
    if hide_taskbar { monitor } else { work_area() }
}

/// Whether the taskbar is out of the way, so the overlay may cover the full
/// monitor: what the session has hidden, or -- when the taskbar was already
/// hidden at open and the toggle is locked -- the setting the editor opened
/// with. Not the persisted value: a failed hide leaves the taskbar visible
/// while the user's choice stays ON.
fn overlay_hides_taskbar(session: &TaskbarSession, locked_value: bool) -> bool {
    if session.is_locked() {
        locked_value
    } else {
        session.is_hidden()
    }
}

/// `zone` (screen pixels) relative to the overlay's top-left corner `origin`,
/// which moves when the overlay switches between the monitor and the work
/// area.
fn zone_in_overlay(zone: &Zone, origin: MonitorGeometry) -> Zone {
    Zone {
        x: zone.x - origin.x,
        y: zone.y - origin.y,
        ..*zone
    }
}

/// Window-style bits that give a window a non-client frame: caption (title
/// bar plus border), sizing border, system menu and the min/max buttons.
const FRAME_STYLE_BITS: u32 =
    WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;

/// Strips the frame style bits from the overlay's own window, so the DWM has
/// no frame (and drop shadow) to draw behind the transparent pixels. Returns
/// whether anything had to be changed.
///
/// WORKAROUND for egui #8750 (https://github.com/emilk/egui/issues/8750):
/// egui-winit requests winit's "undecorated shadow" on every undecorated,
/// non-fullscreen window. winit 0.30 implements an "undecorated" window by
/// keeping `WS_CAPTION | WS_BORDER | WS_SYSMENU` and shrinking the non-client
/// area in `WM_NCCALCSIZE` (plus a 1 px top offset for the shadow), so the DWM
/// still sees a captioned window. On a transparent DX12 + DirectComposition
/// window that frame then shows through every transparent pixel as solid
/// light grey (seen on an NVIDIA machine). Remove this, [`OverlayApp`]'s
/// per-frame call to it and the call in [`run_zone_editor_overlay`] once an
/// egui release fixes it.
///
/// winit re-applies its full style set (caption included) whenever one of its
/// window flags changes -- notably when eframe shows the window after the
/// first frame -- so this has to be repeated; it is a cheap style read when
/// there is nothing to strip.
fn strip_window_frame(hwnd: HWND) -> Result<bool, win32::Win32Error> {
    // Safety: `hwnd` is a live window handle; these calls only read and write
    // the window's own style and re-evaluate its frame.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        if style & FRAME_STYLE_BITS == 0 {
            return Ok(false);
        }
        SetLastError(0);
        let previous = SetWindowLongPtrW(hwnd, GWL_STYLE, (style & !FRAME_STYLE_BITS) as isize);
        if previous == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(0) {
                return Err(win32::Win32Error::Os(e));
            }
        }
        if SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        ) == 0
        {
            return Err(win32::Win32Error::Os(std::io::Error::last_os_error()));
        }
    }
    Ok(true)
}
/// The primary monitor's live DPI as a points-to-pixels factor, for placing
/// the viewport before a window (and so a `Context`) exists. Winit derives
/// the window's scale factor, and so egui's `pixels_per_point`, from the same
/// monitor DPI; every conversion after startup goes through [`scale_of`].
/// Falls back to 100 % if the query fails.
pub(crate) fn primary_monitor_scale() -> f32 {
    // Safety: `MonitorFromPoint` takes a point by value; `GetDpiForMonitor`
    // writes only to the two `u32` out-params, which outlive the call.
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let hr = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        if hr == 0 && dpi_x > 0 {
            scale_from_dpi(dpi_x)
        } else {
            log!(
                "zone editor: GetDpiForMonitor failed (hr={hr}, dpi={dpi_x}); assuming 100 % scale"
            );
            1.0
        }
    }
}

/// The single scale source for pointer and drawing conversions once the
/// overlay is running.
fn scale_of(ctx: &egui::Context) -> f32 {
    ctx.pixels_per_point()
}

/// `scale` is the primary monitor's points-to-pixels factor: the viewport is
/// specified in egui points, the monitor geometry is in physical pixels.
fn native_options(monitor: &MonitorGeometry, scale: f32) -> eframe::NativeOptions {
    // The default wgpu adapter/backend reports only
    // `CompositeAlphaMode::Opaque` for a plain top-level-window-bound
    // swapchain on this project's tested hardware -- true per-pixel window
    // transparency on Windows requires the swapchain to be bound to an
    // `IDCompositionVisual` (DirectComposition) instead of the raw HWND.
    let mut wgpu_setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    wgpu_setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    wgpu_setup
        .instance_descriptor
        .backend_options
        .dx12
        .presentation_system = eframe::wgpu::Dx12SwapchainKind::DxgiFromVisual;

    eframe::NativeOptions {
        persist_window: false,
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(wgpu_setup),
            ..Default::default()
        },
        viewport: egui::ViewportBuilder::default()
            .with_title(ZONE_EDITOR_TITLE)
            .with_position([
                pixels_to_points(monitor.x, scale),
                pixels_to_points(monitor.y, scale),
            ])
            .with_inner_size([
                pixels_to_points(monitor.width, scale),
                pixels_to_points(monitor.height, scale),
            ])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false),
        ..Default::default()
    }
}

/// The live egui app backing the overlay window. Torn down entirely when
/// [`run_zone_editor_overlay`] returns -- nothing here outlives that call.
struct OverlayApp<'a> {
    /// See [`run_zone_editor_overlay`].
    on_tick: &'a mut dyn FnMut(),
    state: AppState,
    outcome: Rc<RefCell<Option<SessionOutcome>>>,
    /// The overlay's window, for [`strip_window_frame`] (egui #8750).
    hwnd: Option<HWND>,
    /// What this session has hidden; shared with the [`TaskbarRestoreGuard`].
    taskbar: Rc<RefCell<TaskbarSession>>,
    /// The configured technique for the toggle button's hide.
    taskbar_mode: TaskbarHideMode,
    /// The user's choice for the setting: starts at the setting, flips on a
    /// toggle click, and is what Save persists. Shared so it is readable
    /// after the window closes.
    hide_choice: Rc<Cell<bool>>,
    /// The bound Toggle-taskbar-auto-hide hotkey ("" = none), for the warning.
    taskbar_hotkey: String,
    /// The full primary monitor, in screen pixels.
    monitor: MonitorGeometry,
    /// What the overlay window currently covers, in screen pixels.
    bounds: MonitorGeometry,
}

impl eframe::App for OverlayApp<'_> {
    /// Fully transparent clear color -- otherwise eframe/wgpu paints an
    /// opaque background behind the egui content even with
    /// `with_transparent(true)` on the viewport.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Heartbeat for the hang watchdog; idle again when the frame is built.
        crate::watchdog::beat();
        let _idle = crate::watchdog::IdleOnDrop;
        (self.on_tick)();
        self.strip_frame();
        self.sync_bounds(ui.ctx());
        self.draw_dim_and_zone(ui);
        self.handle_draw_interaction(ui);
        self.handle_placed_interaction(ui);

        let ctx = ui.ctx().clone();
        self.control_panel(&ctx);

        // A Quit (installer/updater) ends the session like Cancel so the
        // taskbar is restored and the graceful exit can run.
        if crate::instance_channel::quit_pending() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(crate::instance_channel::QUIT_POLL);

        if self.outcome.borrow().is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl OverlayApp<'_> {
    /// Keeps the window frame stripped (egui #8750). Logs each strip, and
    /// each failing frame's error.
    fn strip_frame(&self) {
        let Some(hwnd) = self.hwnd else { return };
        match strip_window_frame(hwnd) {
            Ok(true) => log!("zone editor: window frame stripped"),
            Ok(false) => {}
            Err(e) => log!("zone editor: could not strip the window frame: {e}"),
        }
    }

    /// Applies a toggle click: records the user's choice, then brings the
    /// live taskbar state in line with it (a preview; a failed hide leaves the
    /// choice as clicked and the session's error shown).
    fn set_choice(&self, choice: bool) {
        self.hide_choice.set(choice);
        let mut taskbar = self.taskbar.borrow_mut();
        if taskbar.is_hidden() != choice {
            taskbar.toggle(self.taskbar_mode);
        }
    }

    /// Keeps the overlay covering the full monitor (toggle ON) or the work
    /// area (OFF), resizing the window live. Checked every frame: the work
    /// area changes a moment after the taskbar is shown again, not when the
    /// toggle is clicked.
    fn sync_bounds(&mut self, ctx: &egui::Context) {
        let hide = overlay_hides_taskbar(&self.taskbar.borrow(), self.hide_choice.get());
        let want = overlay_bounds(hide, self.monitor, win32::primary_work_area);
        if want == self.bounds {
            return;
        }
        let scale = scale_of(ctx);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            pixels_to_points(want.x, scale),
            pixels_to_points(want.y, scale),
        )));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            pixels_to_points(want.width, scale),
            pixels_to_points(want.height, scale),
        )));
        self.bounds = want;
    }

    /// A pointer position in the overlay (points) as a screen-pixel point.
    fn screen_point(&self, pos: egui::Pos2, scale: f32) -> Point {
        let p = point_from_points(pos.x, pos.y, scale);
        Point {
            x: p.x + self.bounds.x,
            y: p.y + self.bounds.y,
        }
    }

    /// Dims the whole desktop, then draws the current/live zone as a
    /// brighter cutout with an outline, plus one colored square per resize
    /// handle. Deliberately plain styling.
    fn draw_dim_and_zone(&self, ui: &egui::Ui) {
        let painter = ui.painter();
        let screen = ui.max_rect();
        let scale = scale_of(ui.ctx());
        painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(110));

        if let Some(rect) = self.live_rect(scale) {
            painter.rect_filled(
                rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 30),
            );
            painter.rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 200, 255)),
                egui::StrokeKind::Outside,
            );
        }

        if let EditorState::Placed { zone }
        | EditorState::Resizing { live: zone, .. }
        | EditorState::Moving { live: zone, .. } = &self.state.editor
        {
            let local = zone_in_overlay(zone, self.bounds);
            for handle in Handle::ALL {
                let center = handle_center(&local, handle, scale);
                let color = egui::Color32::from_rgb(80, 200, 255);
                let size = handle_draw_size_pt(
                    pixels_to_points(zone.width, scale),
                    pixels_to_points(zone.height, scale),
                );
                painter.rect_filled(
                    egui::Rect::from_center_size(center, egui::vec2(size.0, size.1)),
                    2.0,
                    color,
                );
            }
        }
    }

    /// The Zone as currently drawn (in points), if any: the live drag, the
    /// placed Zone, or the Zone being moved/resized.
    fn live_rect(&self, scale: f32) -> Option<egui::Rect> {
        self.state
            .live_zone()
            .map(|zone| rect_from_zone(&zone_in_overlay(&zone, self.bounds), scale))
    }

    /// Whole-screen drag capture for the initial corner-to-corner draw
    /// (`Empty`/`Dragging` states only).
    fn handle_draw_interaction(&mut self, ui: &egui::Ui) {
        if !matches!(
            self.state.editor,
            EditorState::Empty | EditorState::Dragging { .. }
        ) {
            return;
        }
        let response = ui.interact(
            ui.max_rect(),
            ui.scope_id().with("draw-area"),
            egui::Sense::click_and_drag(),
        );
        let Some(pos) = response.interact_pointer_pos() else {
            return;
        };
        let point = self.screen_point(pos, scale_of(ui.ctx()));

        if response.drag_started() {
            self.state.apply(Action::Start(point));
        } else if response.dragged() {
            self.state.apply(Action::Drag(point));
        } else if response.drag_stopped() {
            self.state.apply(Action::Commit);
        }
    }

    /// Whole-screen drag capture for a placed Zone: what a press grabs (a
    /// handle, an edge, or the interior) is decided by
    /// [`crate::zone_editor::hit_test`], which also picks the hover cursor.
    fn handle_placed_interaction(&mut self, ui: &egui::Ui) {
        let zone = match &self.state.editor {
            EditorState::Placed { zone } => *zone,
            EditorState::Resizing { live, .. } | EditorState::Moving { live, .. } => *live,
            _ => return,
        };

        let ctx = ui.ctx();
        let scale = scale_of(ctx);
        let grab_px = points_to_pixels(HANDLE_HIT_PT, scale);
        let response = ui.interact(
            ui.max_rect(),
            ui.scope_id().with("placed-area"),
            egui::Sense::drag(),
        );

        // The cursor follows the active drag, else whatever is hovered.
        let target = match &self.state.editor {
            EditorState::Resizing { handle, .. } => Some(Target::Resize(*handle)),
            EditorState::Moving { .. } => Some(Target::Move),
            _ => response
                .hover_pos()
                .and_then(|pos| hit_test(&zone, self.screen_point(pos, scale), grab_px)),
        };
        if let Some(target) = target {
            ctx.set_cursor_icon(cursor_for(target));
        }

        if response.drag_started() {
            // The press position, not the position after egui's drag
            // threshold, decides what was grabbed.
            let press = ctx
                .input(|i| i.pointer.press_origin())
                .or(response.interact_pointer_pos());
            let Some(press) = press else { return };
            let press = self.screen_point(press, scale);
            if let Some(action) = press_action(&zone, press, grab_px) {
                self.state.apply(action);
            }
        }

        let active = matches!(
            self.state.editor,
            EditorState::Resizing { .. } | EditorState::Moving { .. }
        );
        if !active {
            return;
        }
        if response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                self.state
                    .apply(Action::Drag(self.screen_point(pos, scale)));
            }
        } else if response.drag_stopped() {
            self.state.apply(Action::Commit);
        }
    }

    /// The overlay's own Save/Cancel controls; the overlay is fully modal for
    /// its editing session.
    fn control_panel(&mut self, ctx: &egui::Context) {
        // "Save" is disabled outside `Placed` rather than silently rejected,
        // since there's no separate settings window backdrop to surface a
        // rejection message against here.
        let can_save = matches!(self.state.editor, EditorState::Placed { .. });

        // Large, color-coded buttons placed well below the top edge, with an
        // enlarged centered hint label, so the controls are impossible to
        // miss against the darkened overlay.
        egui::Area::new(egui::Id::unique("zone-editor-controls"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 96.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 20, 230))
                    .corner_radius(6.0)
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        let (locked, error) = {
                            let taskbar = self.taskbar.borrow();
                            (taskbar.is_locked(), taskbar.error().map(str::to_owned))
                        };
                        let hide = self.hide_choice.get();
                        ui.horizontal_top(|ui| {
                            let (save, cancel) = save_cancel_buttons(ui, can_save);
                            if save.clicked()
                                && let Some(outcome) = self.state.apply(Action::Save)
                            {
                                *self.outcome.borrow_mut() = Some(outcome);
                            }
                            if cancel.clicked()
                                && let Some(outcome) = self.state.apply(Action::Cancel)
                            {
                                *self.outcome.borrow_mut() = Some(outcome);
                            }
                            // A visible gap sets the taskbar toggle apart from
                            // the Save/Cancel pair.
                            ui.add_space(TOGGLE_GAP_PT);
                            ui.vertical(|ui| {
                                let toggle_text = format!(
                                    "Hide taskbar when the Zone is applied: {}",
                                    if hide { "ON" } else { "OFF" }
                                );
                                let fill = if hide {
                                    egui::Color32::from_rgb(40, 110, 190)
                                } else {
                                    egui::Color32::from_gray(70)
                                };
                                if ui
                                    .add_enabled(!locked, control_button(&toggle_text, fill))
                                    .clicked()
                                {
                                    self.set_choice(!hide);
                                }
                                ui.label(
                                    egui::RichText::new(if hide {
                                        "Games placed in this Zone will hide the taskbar."
                                    } else {
                                        "The taskbar stays visible when games are placed in this Zone."
                                    })
                                    .color(egui::Color32::LIGHT_GRAY)
                                    .size(16.0),
                                );
                                if taskbar_hotkey_warning(hide, &self.taskbar_hotkey) {
                                    ui.label(
                                        egui::RichText::new(
                                            "No hotkey set for Toggle taskbar auto-hide \u{2014} set one in Settings so you can always bring the taskbar back",
                                        )
                                        .color(egui::Color32::from_rgb(255, 200, 80))
                                        .size(16.0),
                                    );
                                }
                            });
                        });
                        ui.vertical_centered(|ui| {
                            if locked {
                                ui.label(
                                    egui::RichText::new("The taskbar is already hidden")
                                        .color(egui::Color32::LIGHT_GRAY)
                                        .size(16.0),
                                );
                            }
                            if let Some(error) = &error {
                                ui.label(
                                    egui::RichText::new(error)
                                        .color(egui::Color32::from_rgb(255, 120, 100))
                                        .size(16.0),
                                );
                            }
                            ui.label(
                                egui::RichText::new(format_zone_readout(self.state.live_zone()))
                                    .color(egui::Color32::WHITE)
                                    .monospace()
                                    .size(18.0),
                            );
                            if let Some(hint) = self.state.hint() {
                                ui.label(
                                    egui::RichText::new(hint)
                                        .color(egui::Color32::LIGHT_GRAY)
                                        .size(16.0),
                                );
                            }
                        });
                    });
            });
    }
}

/// Lays out the Save and Cancel buttons and returns their responses.
///
/// The nested centre-aligned `horizontal` keeps each label centred in its
/// button even when the caller's row is top-aligned.
fn save_cancel_buttons(ui: &mut egui::Ui, can_save: bool) -> (egui::Response, egui::Response) {
    ui.horizontal(|ui| {
        let save = ui.add_enabled(
            can_save,
            control_button("Save and Exit", egui::Color32::from_rgb(34, 160, 60)),
        );
        let cancel = ui.add(control_button(
            "Cancel",
            egui::Color32::from_rgb(214, 69, 33),
        ));
        (save, cancel)
    })
    .inner
}

/// Builds a Save/Cancel control button with the enlarged size and
/// color-coded fill used by the zone editor's control panel.
fn control_button(text: &str, fill: egui::Color32) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text)
            .size(22.0)
            .color(egui::Color32::WHITE),
    )
    .fill(fill)
    .min_size(egui::vec2(140.0, 56.0))
}

fn rect_from_zone(zone: &Zone, scale: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(
            pixels_to_points(zone.x, scale),
            pixels_to_points(zone.y, scale),
        ),
        egui::vec2(
            pixels_to_points(zone.width, scale),
            pixels_to_points(zone.height, scale),
        ),
    )
}

/// Drawn size (width, height) in points of a resize handle on a Zone that is
/// `zone_w_pt` x `zone_h_pt` points: [`HANDLE_DRAW_PT`], capped per axis at a
/// third of the Zone's size (like the hit bands in [`hit_test`]) so handles
/// don't overlap on a very small Zone.
fn handle_draw_size_pt(zone_w_pt: f32, zone_h_pt: f32) -> (f32, f32) {
    (
        HANDLE_DRAW_PT.min(zone_w_pt / 3.0),
        HANDLE_DRAW_PT.min(zone_h_pt / 3.0),
    )
}

/// The editor action a press at `press` starts on a placed `zone`, or `None`
/// when it lands outside the Zone. A press outside is ignored: a new Zone
/// can't be drawn from the Placed state.
fn press_action(zone: &Zone, press: Point, grab_px: i32) -> Option<Action> {
    match hit_test(zone, press, grab_px)? {
        Target::Resize(handle) => Some(Action::Grab(handle, press)),
        Target::Move => Some(Action::GrabMove(press)),
    }
}

/// The control box's readout of the Zone being edited: x, y, width and height
/// in physical pixels (the unit the Zone is saved in), taken from the very
/// Zone `Save` would store. A placeholder when no Zone exists yet.
fn format_zone_readout(zone: Option<Zone>) -> String {
    let Some(zone) = zone else {
        return "x \u{2014}  y \u{2014}  w \u{2014}  h \u{2014}  px".to_string();
    };
    format!(
        "x {}  y {}  w {}  h {}  px",
        zone.x, zone.y, zone.width, zone.height
    )
}

/// The mouse cursor shown over (or while dragging) a grab target.
fn cursor_for(target: Target) -> egui::CursorIcon {
    match target {
        Target::Resize(Handle::N | Handle::S) => egui::CursorIcon::ResizeVertical,
        Target::Resize(Handle::E | Handle::W) => egui::CursorIcon::ResizeHorizontal,
        Target::Resize(Handle::Nw | Handle::Se) => egui::CursorIcon::ResizeNwSe,
        Target::Resize(Handle::Ne | Handle::Sw) => egui::CursorIcon::ResizeNeSw,
        Target::Move => egui::CursorIcon::Move,
    }
}

fn handle_center(zone: &Zone, handle: Handle, scale: f32) -> egui::Pos2 {
    let (x, y, w, h) = (
        pixels_to_points(zone.x, scale),
        pixels_to_points(zone.y, scale),
        pixels_to_points(zone.width, scale),
        pixels_to_points(zone.height, scale),
    );
    match handle {
        Handle::N => egui::pos2(x + w / 2.0, y),
        Handle::S => egui::pos2(x + w / 2.0, y + h),
        Handle::E => egui::pos2(x + w, y + h / 2.0),
        Handle::W => egui::pos2(x, y + h / 2.0),
        Handle::Ne => egui::pos2(x + w, y),
        Handle::Nw => egui::pos2(x, y),
        Handle::Se => egui::pos2(x + w, y + h),
        Handle::Sw => egui::pos2(x, y + h),
    }
}

#[cfg(test)]
mod editor_ui_tests {
    use super::*;

    /// Paints `add_buttons` in a top-aligned row beside a tall sibling and
    /// returns each "Save and Exit" / "Cancel" label's vertical offset from the
    /// centre of its button, and each button's top relative to the row's top.
    fn label_offsets(
        add_buttons: impl Fn(&mut egui::Ui) -> (egui::Response, egui::Response),
    ) -> ([f32; 2], [f32; 2]) {
        fn text_center_y(shape: &egui::Shape, text: &str) -> Option<f32> {
            match shape {
                egui::Shape::Text(t) if t.galley.text() == text => {
                    Some(t.galley.rect.translate(t.pos.to_vec2()).center().y)
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| text_center_y(s, text)),
                _ => None,
            }
        }
        let ctx = egui::Context::default();
        let mut rects = None;
        let mut output = None;
        let mut row_top = 0.0;
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 400.0),
                )),
                ..Default::default()
            };
            let mut out: egui::FullOutput = ctx.run_ui(input, |ui| {
                ui.horizontal_top(|ui| {
                    row_top = ui.cursor().top();
                    let (save, cancel) = add_buttons(ui);
                    rects = Some((save.rect, cancel.rect));
                    ui.allocate_space(egui::vec2(10.0, 120.0));
                });
            });
            out.textures_delta.clear();
            output = Some(out);
        }
        let output = output.unwrap();
        let (save, cancel) = rects.unwrap();
        let offset = |text: &str, rect: egui::Rect| {
            let y = output
                .shapes
                .iter()
                .find_map(|s| text_center_y(&s.shape, text))
                .unwrap();
            y - rect.center().y
        };
        (
            [offset("Save and Exit", save), offset("Cancel", cancel)],
            [save.top() - row_top, cancel.top() - row_top],
        )
    }

    #[test]
    fn save_and_cancel_labels_are_vertically_centred_beside_a_taller_sibling() {
        let (offsets, tops) = label_offsets(|ui| save_cancel_buttons(ui, true));
        for offset in offsets {
            assert!(offset.abs() < 1.0, "label is {offset} pt off centre");
        }
        for top in tops {
            assert!(top.abs() < 1.0, "button starts {top} pt below the row top");
        }
    }

    #[test]
    fn readout_shows_the_zone_in_physical_pixels() {
        let zone = Zone {
            x: 15,
            y: 30,
            width: 450,
            height: 300,
        };
        assert_eq!(
            format_zone_readout(Some(zone)),
            "x 15  y 30  w 450  h 300  px"
        );
    }

    #[test]
    fn readout_shows_a_placeholder_without_a_zone() {
        let text = format_zone_readout(None);
        assert_eq!(text, "x \u{2014}  y \u{2014}  w \u{2014}  h \u{2014}  px");
    }

    #[test]
    fn readout_of_a_live_drag_matches_the_zone_save_stores() {
        let mut state = AppState::new(Monitor {
            width: 1920,
            height: 1080,
        });
        state.apply(Action::Start(Point { x: 303, y: 251 }));
        state.apply(Action::Drag(Point { x: 101, y: 49 }));
        let live = format_zone_readout(state.live_zone());
        state.apply(Action::Commit);
        let Some(SessionOutcome::Saved(saved)) = state.apply(Action::Save) else {
            panic!("save failed");
        };
        assert_eq!(live, format_zone_readout(Some(saved)));
    }

    const MONITOR: MonitorGeometry = MonitorGeometry {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    const WORK_AREA: MonitorGeometry = MonitorGeometry {
        x: 0,
        y: 0,
        width: 1920,
        height: 1040,
    };

    #[test]
    fn overlay_covers_the_full_monitor_when_hiding_and_the_work_area_otherwise() {
        assert_eq!(overlay_bounds(true, MONITOR, || WORK_AREA), MONITOR);
        assert_eq!(overlay_bounds(false, MONITOR, || WORK_AREA), WORK_AREA);
    }

    #[test]
    fn zone_keeps_its_screen_position_when_the_overlay_origin_moves() {
        // A taskbar on the left: the work area starts 48 px in.
        let origin = MonitorGeometry { x: 48, ..MONITOR };
        let zone = Zone {
            x: 100,
            y: 20,
            width: 300,
            height: 200,
        };
        let local = zone_in_overlay(&zone, origin);
        assert_eq!((local.x, local.y), (52, 20));
        assert_eq!((local.width, local.height), (300, 200));
        assert_eq!(zone_in_overlay(&zone, MONITOR), zone);
    }

    #[test]
    fn only_a_save_yields_a_result_carrying_the_toggle_value() {
        let zone = Zone {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        assert_eq!(
            ZoneEditorResult::from_outcome(SessionOutcome::Saved(zone), true),
            Some(ZoneEditorResult {
                zone,
                hide_taskbar_on_apply: true
            })
        );
        assert_eq!(
            ZoneEditorResult::from_outcome(SessionOutcome::Cancelled, true),
            None
        );
    }

    #[test]
    fn overlay_hides_taskbar_follows_the_session_unless_locked() {
        let open = TaskbarSession::new(false);
        assert!(
            !overlay_hides_taskbar(&open, true),
            "unlocked: what the session hid"
        );
        let locked = TaskbarSession::new(true);
        assert!(overlay_hides_taskbar(&locked, true));
        assert!(!overlay_hides_taskbar(&locked, false));
    }

    #[test]
    fn drawn_handles_are_28_points_on_a_big_zone() {
        assert_eq!(handle_draw_size_pt(900.0, 600.0), (28.0, 28.0));
    }

    #[test]
    fn drawn_handles_shrink_to_a_third_of_a_small_zone() {
        assert_eq!(handle_draw_size_pt(30.0, 600.0), (10.0, 28.0));
        assert_eq!(handle_draw_size_pt(900.0, 45.0), (28.0, 15.0));
    }

    #[test]
    fn press_on_a_handle_grabs_it() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let press = Point { x: 100, y: 100 };
        assert!(matches!(
            press_action(&zone, press, 40),
            Some(Action::Grab(Handle::Nw, _))
        ));
    }

    #[test]
    fn press_inside_the_zone_moves_it() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let press = Point { x: 300, y: 250 };
        assert!(matches!(
            press_action(&zone, press, 40),
            Some(Action::GrabMove(_))
        ));
    }

    #[test]
    fn press_outside_the_zone_does_nothing() {
        let zone = Zone {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let press = Point { x: 10, y: 10 };
        assert!(press_action(&zone, press, 40).is_none());
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    #[test]
    fn cursor_matches_the_grab_target() {
        use egui::CursorIcon::*;
        let cases = [
            (Handle::N, ResizeVertical),
            (Handle::S, ResizeVertical),
            (Handle::E, ResizeHorizontal),
            (Handle::W, ResizeHorizontal),
            (Handle::Nw, ResizeNwSe),
            (Handle::Se, ResizeNwSe),
            (Handle::Ne, ResizeNeSw),
            (Handle::Sw, ResizeNeSw),
        ];
        for (handle, icon) in cases {
            assert_eq!(cursor_for(Target::Resize(handle)), icon, "{handle:?}");
        }
        assert_eq!(cursor_for(Target::Move), Move);
    }
}

#[cfg(test)]
mod window_frame_tests {
    use super::*;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetClientRect, GetWindowRect, WS_POPUP,
    };

    /// Destroys the window even when an assertion panics.
    struct TestWindow(HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // Safety: the window was created by this thread and not yet destroyed.
            unsafe { DestroyWindow(self.0) };
        }
    }

    fn create_window(style: u32) -> TestWindow {
        let class = crate::wide::encode_wide("STATIC");
        // Safety: built-in STATIC class, no parent; destroyed by `TestWindow`.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                style,
                0,
                0,
                200,
                100,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        TestWindow(hwnd)
    }

    fn style_of(hwnd: HWND) -> u32 {
        // Safety: `hwnd` is live.
        unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 }
    }

    fn sizes(hwnd: HWND) -> ((i32, i32), (i32, i32)) {
        // SAFETY: all-zero bytes are a valid value of this plain C struct.
        let (mut window, mut client) = (unsafe { std::mem::zeroed::<RECT>() }, unsafe {
            std::mem::zeroed::<RECT>()
        });
        // Safety: valid out-buffers, live window.
        unsafe {
            GetWindowRect(hwnd, &mut window);
            GetClientRect(hwnd, &mut client);
        }
        (
            (window.right - window.left, window.bottom - window.top),
            (client.right - client.left, client.bottom - client.top),
        )
    }

    #[test]
    fn strip_window_frame_removes_the_frame_and_keeps_popup() {
        let w = create_window(WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX);
        let (window, client) = sizes(w.0);
        assert_ne!(window, client, "a captioned window has a non-client area");

        assert!(strip_window_frame(w.0).unwrap());

        let style = style_of(w.0);
        assert_eq!(style & FRAME_STYLE_BITS, 0);
        assert_ne!(style & WS_POPUP, 0);
        let (window, client) = sizes(w.0);
        assert_eq!(window, client, "no non-client area is left");
    }

    #[test]
    fn strip_window_frame_is_a_no_op_on_a_frameless_window() {
        let w = create_window(WS_POPUP);
        let before = style_of(w.0);
        assert!(!strip_window_frame(w.0).unwrap());
        assert_eq!(style_of(w.0), before);
    }

    #[test]
    fn strip_window_frame_strips_again_after_the_style_is_reapplied() {
        let w = create_window(WS_POPUP);
        // Safety: live window; mimics winit re-applying its style set.
        unsafe { SetWindowLongPtrW(w.0, GWL_STYLE, (WS_POPUP | WS_CAPTION) as isize) };
        assert!(strip_window_frame(w.0).unwrap());
        assert_eq!(style_of(w.0) & FRAME_STYLE_BITS, 0);
    }
}
