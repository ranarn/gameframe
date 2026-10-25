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
//! Drives [`crate::zone_editor::AppState`] from real pointer input, and
//! hides the taskbar for the duration of the editing session via
//! [`crate::win32::hide_taskbar`] (mode-aware -- see
//! [`crate::profile_store::TaskbarHideMode`]), restoring exactly the
//! previous state via [`crate::win32::restore_taskbar`] once the session
//! ends -- Save, Cancel, or the window being closed any other way (an RAII
//! guard, [`TaskbarRestoreGuard`], holds the remembered
//! [`crate::win32::TaskbarHideRestore`] and restores it on drop, covering
//! early returns). A panic is covered separately: the release profile is
//! `panic = "abort"`, so the guard's `Drop` does NOT run then; instead the
//! panic hook ([`crate::taskbar_recovery::install_panic_hook`]) restores the
//! taskbar before the abort, and a kill is covered by the marker file
//! restored at the next start (see [`crate::taskbar_recovery`]).
//!
//! OS/GPU-facing: not exercised by this crate's automated tests, for the
//! same reason [`crate::settings_ui::run_settings_window`] isn't -- a
//! headless/CI sandbox cannot provide a real GPU-backed window to render
//! into or a real taskbar to hide. The pure interaction logic it drives
//! ([`crate::zone_editor`]) and the pure taskbar hide/remember decision
//! ([`crate::win32::plan_taskbar_hide`]) are both fully unit tested
//! instead.

use crate::profile_store::{TaskbarHideMode, Zone};
use crate::region_calc::MonitorGeometry;
use crate::win32;
use crate::zone_editor::{
    Action, AppState, EditorState, Handle, Monitor, Point, SessionOutcome, pixels_to_points,
    point_from_points, scale_from_dpi,
};
use eframe::egui;
use std::cell::RefCell;
use std::rc::Rc;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

/// Reasons opening the zone-editor overlay (or restoring the taskbar
/// afterward) can fail.
#[derive(Debug)]
pub enum ZoneOverlayError {
    Taskbar(win32::Win32Error),
    Window(eframe::Error),
}

impl std::fmt::Display for ZoneOverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZoneOverlayError::Taskbar(e) => {
                write!(f, "failed to force taskbar auto-hide for zone editing: {e}")
            }
            ZoneOverlayError::Window(e) => write!(f, "failed to open the zone editor overlay: {e}"),
        }
    }
}

impl std::error::Error for ZoneOverlayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ZoneOverlayError::Taskbar(e) => Some(e),
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
/// Hides the taskbar for the session using whichever technique `mode`
/// selects (see module docs) and always restores exactly the state found
/// beforehand before returning -- whether the window ran cleanly, failed to
/// run, or returned early -- via the [`TaskbarRestoreGuard`] RAII guard held
/// across the call. A panic mid-session is handled by the panic hook, not
/// the guard (see the module docs).
pub fn run_zone_editor_overlay(
    initial: Option<Zone>,
    mode: TaskbarHideMode,
) -> Result<Option<Zone>, ZoneOverlayError> {
    let monitor_geometry = crate::settings_ui::primary_monitor_geometry();
    let monitor = Monitor {
        width: monitor_geometry.width,
        height: monitor_geometry.height,
    };

    let remembered_taskbar_state = win32::hide_taskbar(mode).map_err(ZoneOverlayError::Taskbar)?;

    // RAII guard, not a manual "call restore after run_native" step, so
    // every non-panic exit path -- normal return and early `?` propagation
    // -- restores the taskbar. It does not run on a panic in the release
    // build (`panic = "abort"`); the panic hook installed at startup
    // (`crate::taskbar_recovery`) restores the taskbar in that case, e.g.
    // after a wgpu device-lost panic on sleep/resume or a monitor unplug.
    let _taskbar_restore_guard = TaskbarRestoreGuard::new(remembered_taskbar_state);

    let state = match initial {
        Some(zone) => AppState::new_with_zone(monitor, zone),
        None => AppState::new(monitor),
    };

    let outcome: Rc<RefCell<Option<SessionOutcome>>> = Rc::new(RefCell::new(None));
    let outcome_for_app = Rc::clone(&outcome);

    let scale = primary_monitor_scale();
    let native_options = native_options(&monitor_geometry, scale);

    let run_result = eframe::run_native(
        "GameFrame zone editor",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(OverlayApp {
                state,
                outcome: outcome_for_app,
            }))
        }),
    );

    run_result.map_err(ZoneOverlayError::Window)?;

    match outcome.borrow_mut().take() {
        Some(SessionOutcome::Saved(zone)) => Ok(Some(zone)),
        Some(SessionOutcome::Cancelled) | None => Ok(None),
    }
}

/// RAII guard that restores the taskbar's remembered state (which technique
/// hid it, plus whatever it needs to restore exactly -- see
/// [`win32::TaskbarHideRestore`]) when dropped, whether that happens via
/// normal return or early `?` propagation. It does not cover a panic in the
/// release build (`panic = "abort"` skips destructors); the panic hook in
/// `crate::taskbar_recovery` does.
///
/// `restore_taskbar`'s own error is logged rather than propagated or
/// unwrapped -- a `Drop` impl must not panic (it could run during a debug
/// build's unwind, where a second panic aborts).
struct TaskbarRestoreGuard {
    remembered: win32::TaskbarHideRestore,
}

impl TaskbarRestoreGuard {
    fn new(remembered: win32::TaskbarHideRestore) -> Self {
        Self { remembered }
    }
}

impl Drop for TaskbarRestoreGuard {
    fn drop(&mut self) {
        if let Err(e) = win32::restore_taskbar(self.remembered) {
            log!("zone editor: failed to restore taskbar state: {e}");
        }
    }
}

/// The primary monitor's live DPI as a points-to-pixels factor, for placing
/// the viewport before a window (and so a `Context`) exists. Winit derives
/// the window's scale factor, and so egui's `pixels_per_point`, from the same
/// monitor DPI; every conversion after startup goes through [`scale_of`].
/// Falls back to 100 % if the query fails.
fn primary_monitor_scale() -> f32 {
    // Safety: `MonitorFromPoint` takes a point by value; `GetDpiForMonitor`
    // writes only to the two `u32` out-params, which outlive the call.
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let hr = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        if hr == 0 && dpi_x > 0 {
            scale_from_dpi(dpi_x)
        } else {
            log!("zone editor: GetDpiForMonitor failed (hr={hr}, dpi={dpi_x}); assuming 100 % scale");
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
            .with_title("GameFrame zone editor")
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
struct OverlayApp {
    state: AppState,
    outcome: Rc<RefCell<Option<SessionOutcome>>>,
}

impl eframe::App for OverlayApp {
    /// Fully transparent clear color -- otherwise eframe/wgpu paints an
    /// opaque background behind the egui content even with
    /// `with_transparent(true)` on the viewport.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw_dim_and_zone(ui);
        self.handle_draw_interaction(ui);
        if matches!(
            self.state.editor,
            EditorState::Placed { .. } | EditorState::Resizing { .. }
        ) {
            self.handle_resize_interaction(ui);
        }

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

impl OverlayApp {
    /// Dims the whole desktop, then draws the current/live zone as a
    /// brighter cutout with an outline, plus one small colored square per
    /// resize handle. Deliberately plain: no hover cursors or other styling.
    fn draw_dim_and_zone(&self, ui: &egui::Ui) {
        let painter = ui.painter();
        let screen = ui.max_rect();
        let scale = scale_of(ui.ctx());
        painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(110));

        let live_rect = match &self.state.editor {
            EditorState::Empty => None,
            EditorState::Dragging { anchor, current } => {
                Some(rect_from_points(*anchor, *current, scale))
            }
            EditorState::Placed { zone, .. } => Some(rect_from_zone(zone, scale)),
            EditorState::Resizing { live, .. } => Some(rect_from_zone(live, scale)),
        };

        if let Some(rect) = live_rect {
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

        if let EditorState::Placed { zone } | EditorState::Resizing { live: zone, .. } =
            &self.state.editor
        {
            for handle in Handle::ALL {
                let center = handle_center(zone, handle, scale);
                let color = egui::Color32::from_rgb(80, 200, 255);
                painter.rect_filled(
                    egui::Rect::from_center_size(center, egui::vec2(14.0, 14.0)),
                    2.0,
                    color,
                );
            }
        }
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
        let point = point_from_points(pos.x, pos.y, scale_of(ui.ctx()));

        if response.drag_started() {
            self.state.apply(Action::Start(point));
        } else if response.dragged() {
            self.state.apply(Action::Drag(point));
        } else if response.drag_stopped() {
            self.state.apply(Action::Commit);
        }
    }

    /// One drag-sense rect per resize handle, for `Placed`/
    /// `Resizing` states.
    fn handle_resize_interaction(&mut self, ui: &egui::Ui) {
        let zone = match &self.state.editor {
            EditorState::Placed { zone } => *zone,
            EditorState::Resizing { live, .. } => *live,
            _ => return,
        };

        let scale = scale_of(ui.ctx());
        for handle in Handle::ALL {
            let center = handle_center(&zone, handle, scale);
            let rect = egui::Rect::from_center_size(center, egui::vec2(20.0, 20.0));
            let response = ui.interact(
                rect,
                ui.scope_id().with(("handle", format!("{handle:?}"))),
                egui::Sense::drag(),
            );
            let Some(pos) = response.interact_pointer_pos() else {
                continue;
            };
            let point = point_from_points(pos.x, pos.y, scale);

            if response.drag_started() {
                self.state.apply(Action::Grab(handle));
            } else if response.dragged() {
                self.state.apply(Action::Drag(point));
            } else if response.drag_stopped() {
                self.state.apply(Action::Commit);
            }
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
                        ui.horizontal(|ui| {
                            let save_button =
                                control_button("Save", egui::Color32::from_rgb(34, 160, 60));
                            if ui.add_enabled(can_save, save_button).clicked()
                                && let Some(outcome) = self.state.apply(Action::Save)
                            {
                                *self.outcome.borrow_mut() = Some(outcome);
                            }
                            let cancel_button =
                                control_button("Cancel", egui::Color32::from_rgb(214, 69, 33));
                            if ui.add(cancel_button).clicked()
                                && let Some(outcome) = self.state.apply(Action::Cancel)
                            {
                                *self.outcome.borrow_mut() = Some(outcome);
                            }
                        });
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(&self.state.last_message)
                                    .color(egui::Color32::LIGHT_GRAY)
                                    .size(16.0),
                            );
                        });
                    });
            });
    }
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

/// The helpers below convert pixel-space editor state to the egui points the
/// overlay draws in (see [`crate::zone_editor::pixels_to_points`]).
fn rect_from_points(a: Point, b: Point, scale: f32) -> egui::Rect {
    egui::Rect::from_two_pos(
        egui::pos2(pixels_to_points(a.x, scale), pixels_to_points(a.y, scale)),
        egui::pos2(pixels_to_points(b.x, scale), pixels_to_points(b.y, scale)),
    )
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
