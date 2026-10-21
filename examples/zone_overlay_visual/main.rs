//! PROTOTYPE — throwaway, not part of the shipped binary. A real on-screen
//! eframe overlay for wayfinder ticket #18 (map #14), so the visual
//! feel (transparency, borderless, always-on-top, drag-to-draw, handle
//! resize with pinned edges) can actually be seen rather than just read as
//! text — see `../zone_overlay_prototype/` for the text-driven state-machine
//! version and `NOTES.md` there for the full question/verdict.
//!
//! Run with `cargo run --example zone_overlay_visual`.
//!
//! Deliberately scoped down from the real feature for speed:
//! - Taskbar auto-hide is a simulated label only (not real `SHAppBarMessage`
//!   calls) — that OS mechanism already exists and works in `win32.rs`
//!   (built for the global-hotkey taskbar toggle); re-proving it here would
//!   risk leaving your real taskbar hidden if this throwaway code panics.
//! - No DPI-scaling handling — assumes 100% display scaling.
//! - No cursor-icon changes on hover (resize cursors, etc).

#[path = "../zone_overlay_prototype/zone_editor.rs"]
mod zone_editor;

use eframe::egui;
use zone_editor::{Action, AppState, EditorState, Handle, Monitor, PinnedEdges, Point, TaskbarState, Zone};

/// PROTOTYPE diagnostic — prints every `log::warn!`/`error!` from
/// eframe/wgpu to stderr. No logger crate is set up in this project
/// (`main.rs` never calls one), so without this, a silent fallback deep in
/// wgpu (e.g. "requested transparency but the surface doesn't support a
/// transparent `CompositeAlphaMode`") produces no visible error at all —
/// exactly the "no crash, no message, just wrong pixels" symptom this is
/// diagnosing.
struct StderrLogger;
impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}: {}", record.level(), record.target(), record.args());
        }
    }
    fn flush(&self) {}
}

fn main() -> eframe::Result<()> {
    log::set_logger(&StderrLogger).ok();
    log::set_max_level(log::LevelFilter::Warn);

    let monitor = primary_monitor_geometry();

    // The default wgpu adapter/backend on this machine reports only
    // `CompositeAlphaMode::Opaque` for a plain top-level-window-bound
    // swapchain (confirmed via the logger above) — true per-pixel window
    // transparency on Windows requires the swapchain to be bound to an
    // `IDCompositionVisual` (DirectComposition) instead of the raw HWND.
    // Same root cause and fix already verified live in the sibling
    // `poe-gizmos` project (same eframe 0.36.1) — see its `src/overlay.rs`
    // module doc comment ("Root cause of the opacity bug") for the full
    // writeup, including why forcing only DX12 (keeping its default
    // HWND-bound swapchain) is NOT sufficient on its own.
    let mut wgpu_setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    wgpu_setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    wgpu_setup.instance_descriptor.backend_options.dx12.presentation_system =
        eframe::wgpu::Dx12SwapchainKind::DxgiFromVisual;

    let native_options = eframe::NativeOptions {
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(wgpu_setup),
            ..Default::default()
        },
        viewport: egui::ViewportBuilder::default()
            .with_title("GameFrame zone overlay prototype")
            .with_position([0.0, 0.0])
            .with_inner_size([monitor.width as f32, monitor.height as f32])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "GameFrame zone overlay prototype",
        native_options,
        Box::new(move |_cc| Ok(Box::new(OverlayApp { state: AppState::new(monitor) }))),
    )
}

fn primary_monitor_geometry() -> Monitor {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    // Safety: same as `settings_ui::primary_monitor_geometry` — a
    // side-effect-free Win32 query taking a plain integer index.
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    Monitor { width, height }
}

struct OverlayApp {
    state: AppState,
}

impl eframe::App for OverlayApp {
    /// Fully transparent clear color — otherwise eframe/wgpu paints an
    /// opaque background behind the egui content even with
    /// `with_transparent(true)` on the viewport.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw_dim_and_zone(ui);
        self.handle_draw_interaction(ui);
        if let EditorState::Placed { .. } | EditorState::Resizing { .. } = &self.state.editor {
            self.handle_resize_interaction(ui);
        }

        let ctx = ui.ctx().clone();
        self.control_panel(&ctx);

        if should_close(&self.state) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn should_close(state: &AppState) -> bool {
    state.last_message.starts_with("saved:")
}

impl OverlayApp {
    /// Dims the whole desktop, then draws the current/live zone as a
    /// brighter cutout with an outline — the "translucent overlay with a
    /// visible selection" look the ticket asked for.
    fn draw_dim_and_zone(&self, ui: &egui::Ui) {
        let painter = ui.painter();
        let screen = ui.max_rect();
        painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(110));

        let live_rect = match &self.state.editor {
            EditorState::Empty => None,
            EditorState::Dragging { anchor, current } => Some(rect_from_points(*anchor, *current)),
            EditorState::Placed { zone, .. } => Some(rect_from_zone(zone)),
            EditorState::Resizing { live, .. } => Some(rect_from_zone(live)),
        };

        if let Some(rect) = live_rect {
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 30));
            painter.rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 200, 255)),
                egui::StrokeKind::Outside,
            );
        }

        if let EditorState::Placed { zone, pinned } | EditorState::Resizing { pinned, live: zone, .. } =
            &self.state.editor
        {
            for handle in Handle::ALL {
                let center = handle_center(zone, handle);
                let enabled = handle.enabled(*pinned);
                let color = if enabled {
                    egui::Color32::from_rgb(80, 200, 255)
                } else {
                    egui::Color32::from_gray(90)
                };
                painter.rect_filled(egui::Rect::from_center_size(center, egui::vec2(14.0, 14.0)), 2.0, color);
            }
        }
    }

    /// Whole-screen drag capture for the initial corner-to-corner draw
    /// (`Empty` state only — the `sense` still exists in other states but
    /// its drag events are ignored there since handle interaction below
    /// takes precedence and this region no longer represents "start a new
    /// zone").
    fn handle_draw_interaction(&mut self, ui: &egui::Ui) {
        if !matches!(self.state.editor, EditorState::Empty | EditorState::Dragging { .. }) {
            return;
        }
        let response = ui.interact(ui.max_rect(), ui.scope_id().with("draw-area"), egui::Sense::click_and_drag());
        let Some(pos) = response.interact_pointer_pos() else { return };
        let point = Point { x: pos.x as i32, y: pos.y as i32 };

        if response.drag_started() {
            self.state.apply(Action::Start(point));
        } else if response.dragged() {
            self.state.apply(Action::Drag(point));
        } else if response.drag_stopped() {
            self.state.apply(Action::Commit);
        }
    }

    fn handle_resize_interaction(&mut self, ui: &egui::Ui) {
        let zone = match &self.state.editor {
            EditorState::Placed { zone, .. } => *zone,
            EditorState::Resizing { live, .. } => *live,
            _ => return,
        };
        let pinned = match &self.state.editor {
            EditorState::Placed { pinned, .. } | EditorState::Resizing { pinned, .. } => *pinned,
            _ => return,
        };

        for handle in Handle::ALL {
            if !handle.enabled(pinned) {
                continue;
            }
            let center = handle_center(&zone, handle);
            let rect = egui::Rect::from_center_size(center, egui::vec2(20.0, 20.0));
            let response = ui.interact(rect, ui.scope_id().with(("handle", format!("{handle:?}"))), egui::Sense::drag());
            let Some(pos) = response.interact_pointer_pos() else { continue };
            let point = Point { x: pos.x as i32, y: pos.y as i32 };

            if response.drag_started() {
                self.state.apply(Action::Grab(handle));
            } else if response.dragged() {
                self.state.apply(Action::Drag(point));
            } else if response.drag_stopped() {
                self.state.apply(Action::Commit);
            }
        }
    }

    fn control_panel(&mut self, ctx: &egui::Context) {
        egui::Area::new(egui::Id::unique("controls"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 24.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 20, 230))
                    .corner_radius(6.0)
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("New freeform").clicked() {
                                self.state.apply(Action::New);
                            }
                            if ui.button("Edit Left").clicked() {
                                self.state.apply(Action::EditPreset {
                                    pinned: PinnedEdges::LEFT_HALF,
                                    zone: Zone {
                                        x: 0,
                                        y: 0,
                                        width: self.state.monitor.width / 2,
                                        height: self.state.monitor.height,
                                    },
                                });
                            }
                            if ui.button("Edit Right").clicked() {
                                let half = self.state.monitor.width / 2;
                                self.state.apply(Action::EditPreset {
                                    pinned: PinnedEdges::RIGHT_HALF,
                                    zone: Zone {
                                        x: half,
                                        y: 0,
                                        width: self.state.monitor.width - half,
                                        height: self.state.monitor.height,
                                    },
                                });
                            }
                            if ui.button("Edit Custom").clicked() {
                                self.state.apply(Action::EditPreset {
                                    pinned: PinnedEdges::NONE,
                                    zone: Zone { x: 200, y: 200, width: 600, height: 500 },
                                });
                            }
                            ui.separator();
                            if ui.button("Save").clicked() {
                                self.state.apply(Action::Save);
                            }
                            if ui.button("Cancel").clicked() {
                                self.state.apply(Action::Cancel);
                                self.state.apply(Action::New);
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                        ui.label(
                            egui::RichText::new(&self.state.last_message).color(egui::Color32::LIGHT_GRAY).small(),
                        );
                        ui.label(
                            egui::RichText::new(match self.state.taskbar {
                                TaskbarState::AlwaysVisible => "taskbar: AlwaysVisible",
                                TaskbarState::AutoHidden => "taskbar: AutoHidden (simulated)",
                            })
                            .color(egui::Color32::LIGHT_GRAY)
                            .small(),
                        );
                    });
            });
    }
}

fn rect_from_points(a: Point, b: Point) -> egui::Rect {
    egui::Rect::from_two_pos(egui::pos2(a.x as f32, a.y as f32), egui::pos2(b.x as f32, b.y as f32))
}

fn rect_from_zone(zone: &Zone) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(zone.x as f32, zone.y as f32),
        egui::vec2(zone.width as f32, zone.height as f32),
    )
}

fn handle_center(zone: &Zone, handle: Handle) -> egui::Pos2 {
    let (x, y, w, h) = (zone.x as f32, zone.y as f32, zone.width as f32, zone.height as f32);
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
