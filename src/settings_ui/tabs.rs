//! The Games, Zone, General and About tabs.

use super::{HotkeyField, SettingsApp};
use crate::profile_store::{TaskbarHideMode, UpdateMode};
use crate::ui_helpers::section_separator;

/// Games table geometry, in points.
const ICON_SIZE: f32 = 24.0;
const AUTO_APPLY_WIDTH: f32 = 100.0;
const GAME_CURSOR_WIDTH: f32 = 110.0;
const REMOVE_WIDTH: f32 = 70.0;
const ROW_SPACING: f32 = 8.0;
/// Height of one Games row: the icon plus the old grid row spacing.
const ROW_HEIGHT: f32 = ICON_SIZE + ROW_SPACING;

/// Stand-in for an exe whose icon couldn't be read: a rounded outline.
fn paint_generic_icon(ui: &egui::Ui, rect: egui::Rect) {
    let stroke = egui::Stroke::new(1.5, ui.visuals().weak_text_color());
    let frame = rect.shrink(2.0);
    ui.painter()
        .rect_stroke(frame, 3.0, stroke, egui::StrokeKind::Inside);
    // A title bar line makes it read as a window.
    let bar_y = frame.top() + frame.height() * 0.3;
    ui.painter().line_segment(
        [
            egui::pos2(frame.left(), bar_y),
            egui::pos2(frame.right(), bar_y),
        ],
        stroke,
    );
}

impl SettingsApp<'_> {
    /// Renders the "Games" section: a striped table with one row per tracked
    /// game -- exe icon, display name, an auto-apply checkbox, a game cursor
    /// checkbox, and an immediate no-confirmation "Remove" button (see
    /// [`super::SettingsState::remove_tracked_game`]).
    ///
    /// Removal is deferred until after the loop (rather than mutating
    /// `self.state.tracked_games` while iterating it) to avoid mutating the
    /// `Vec` out from under `iter_mut`'s in-progress borrow.
    pub(super) fn games_section(&mut self, ui: &mut egui::Ui) {
        if let Some(window) = self.window_chooser.show(ui, &self.config_dir) {
            crate::settings_link::request_apply_to_window(window);
        }
        ui.add_space(ROW_SPACING);

        if self.state.tracked_games.is_empty() {
            ui.label(
                "No tracked games yet -- use the \"Apply to Window\" button above or the tray menu to add one.",
            );
            return;
        }

        let mut to_remove: Option<usize> = None;
        let spacing = ui.spacing().item_spacing.x;
        // The fixed columns sit flush right; the name column takes what is
        // left, so a narrow window shortens the name (it truncates) instead of
        // pushing "Remove" past the edge.
        // Three controls, so two gaps between them.
        let fixed_width = AUTO_APPLY_WIDTH + GAME_CURSOR_WIDTH + REMOVE_WIDTH + 2.0 * spacing;
        let row_gap = ui.spacing().item_spacing.y;
        let ctx = ui.ctx().clone();
        for (index, game) in self.state.tracked_games.iter_mut().enumerate() {
            // One row is a full-width rect split into a left (icon, name) and a
            // right (fixed columns) child, each centred vertically, so every
            // control shares the row's centre line.
            let (row, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), ROW_HEIGHT),
                egui::Sense::hover(),
            );
            if index % 2 == 0 {
                // Cover the gap the parent puts between rows so stripes are
                // contiguous.
                ui.painter().rect_filled(
                    row.expand2(egui::vec2(0.0, row_gap / 2.0)),
                    0.0,
                    ui.visuals().faint_bg_color,
                );
            }
            // The fixed columns stay flush right even in a window narrower than
            // they are: then the name column is empty and the overflow is on
            // the left, never past the right edge.
            let split = row.right() - fixed_width;
            let left =
                egui::Rect::from_min_max(row.min, egui::pos2(split.max(row.left()), row.max.y));
            let right = egui::Rect::from_min_max(egui::pos2(split, row.min.y), row.max);

            let mut left_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(left)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            let icon_rect = left_ui
                .allocate_exact_size(egui::Vec2::splat(ICON_SIZE), egui::Sense::hover())
                .0;
            match self.exe_icons.get(&ctx, &game.exe_path) {
                Some(texture) => {
                    egui::Image::new((texture.id(), egui::Vec2::splat(ICON_SIZE)))
                        .paint_at(&left_ui, icon_rect);
                }
                None => paint_generic_icon(&left_ui, icon_rect),
            }
            left_ui
                .add(egui::Label::new(&game.display_name).truncate())
                .on_hover_text(game.exe_path.display().to_string());

            let mut right_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(right)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            right_ui.scope(|ui| {
                ui.set_width(AUTO_APPLY_WIDTH);
                ui.checkbox(&mut game.auto_apply, "Auto-apply");
            });
            right_ui.scope(|ui| {
                ui.set_width(GAME_CURSOR_WIDTH);
                ui.checkbox(&mut game.game_cursor, "Game cursor");
            });
            if right_ui
                .add(egui::Button::new("Remove").min_size(egui::vec2(REMOVE_WIDTH, 0.0)))
                .clicked()
            {
                to_remove = Some(index);
            }
        }
        if let Some(index) = to_remove {
            self.state.remove_tracked_game(index);
        }
    }

    /// Renders the "Zone, hotkeys & taskbar" tab: Zone, global hotkeys and
    /// taskbar hiding.
    pub(super) fn zone_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Zone");
        self.zone_section(ui);
        section_separator(ui);
        ui.heading("Global hotkeys");
        self.hotkey_row(ui, "Apply / restore Zone:", HotkeyField::ToggleBorderless);
        self.hotkey_row(
            ui,
            "Toggle taskbar auto-hide:",
            HotkeyField::ToggleTaskbarAutohide,
        );
        self.hotkey_row(ui, "Toggle game cursor:", HotkeyField::ToggleGameCursor);
        // Re-sync immediately after the hotkey rows: a "Set hotkey" click
        // just above sets `state.capturing = Some(..)` within this same
        // `ui()` call, but the sync at the top of `ui()` already ran against
        // the *old* `None` state. Without this second call the hook wouldn't
        // install until the following frame, leaving a one-frame window
        // where physical Ctrl+Escape still reaches Windows' Start-menu
        // shortcut.
        self.sync_keyboard_hook();

        section_separator(ui);
        ui.heading("Taskbar hiding");
        ui.checkbox(
            &mut self.state.hide_taskbar_on_apply,
            "Hide taskbar when the Zone is applied",
        );
        if self.state.missing_taskbar_hotkey_warning() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "No hotkey set for Toggle taskbar auto-hide \u{2014} set one so you can always bring the taskbar back",
                    )
                    .color(ui.visuals().warn_fg_color),
                )
                .wrap(),
            );
        }
        ui.add_enabled_ui(self.state.hide_taskbar_on_apply, |ui| {
            ui.radio_value(
                &mut self.state.taskbar_hide_mode,
                TaskbarHideMode::FullyHidden,
                "Fully hidden (no hover-reveal)",
            );
            ui.radio_value(
                &mut self.state.taskbar_hide_mode,
                TaskbarHideMode::HoverReveal,
                "Hover to reveal (classic Windows behavior)",
            );
        });
    }

    /// Renders the "General" tab: Startup and Diagnostics.
    pub(super) fn general_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Startup");
        ui.checkbox(&mut self.state.start_with_windows, "Start with Windows");

        section_separator(ui);
        ui.heading("Diagnostics");
        ui.checkbox(&mut self.state.log_ui_hangs, "Log UI hangs (diagnostics)");
        ui.label(
            "Writes a line to the log when GameFrame stops responding for a few seconds. Takes effect when you apply or save your changes; Close keeps the old state.",
        );
    }

    /// Renders the "About" tab: the about info, then the Updates section.
    pub(super) fn about_tab(&mut self, ui: &mut egui::Ui) {
        crate::about_ui::about_section(ui);

        section_separator(ui);
        ui.heading("Updates");
        ui.radio_value(
            &mut self.state.update_mode,
            UpdateMode::Auto,
            "Install updates automatically at startup",
        );
        ui.radio_value(
            &mut self.state.update_mode,
            UpdateMode::Ask,
            "Ask me (adds a tray item when an update is available)",
        );
        self.update_check_row(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_store::{Config, TrackedGame};
    use crate::settings_ui::SettingsState;
    use std::cell::RefCell;
    use std::rc::Rc;

    const LONG_NAME: &str = "An Extremely Long Game Title That Cannot Possibly Fit In The Name Column Of A Narrow Window";

    /// Where text starting with `prefix` was painted: (left, right, vertical
    /// centre).
    fn text_rects(output: &egui::FullOutput, prefix: &str) -> Vec<(f32, f32, f32)> {
        let mut found = Vec::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, prefix, &mut found);
        }
        found
    }

    fn collect_text(shape: &egui::Shape, prefix: &str, found: &mut Vec<(f32, f32, f32)>) {
        match shape {
            egui::Shape::Text(t) if t.galley.text().starts_with(prefix) => {
                let rect = t.galley.rect.translate(t.pos.to_vec2());
                found.push((rect.left(), rect.right(), rect.center().y));
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_text(s, prefix, found)),
            _ => {}
        }
    }

    /// Rectangles filled with the faint stripe colour: (top, bottom).
    fn stripes(output: &egui::FullOutput, faint: egui::Color32) -> Vec<(f32, f32)> {
        fn walk(shape: &egui::Shape, faint: egui::Color32, found: &mut Vec<(f32, f32)>) {
            match shape {
                egui::Shape::Rect(r) if r.fill == faint => {
                    found.push((r.rect.top(), r.rect.bottom()))
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, faint, found)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, faint, &mut found);
        }
        found
    }

    /// Runs the real Games section in a window `width` points wide and returns
    /// the output of the last of a few frames, plus the right edge of the
    /// scroll area's content (the real inset the rows must stay inside).
    fn render_games(width: f32) -> (egui::FullOutput, f32) {
        let mut config = Config::default();
        for (name, cursor) in [
            ("Last Epoch", true),
            ("World of Warcraft", false),
            (LONG_NAME, false),
        ] {
            config
                .tracked_games
                .push(TrackedGame::new_for_cursor_setting(
                    std::path::PathBuf::from(format!("C:\\Games\\{name}.exe")),
                    name.to_string(),
                    cursor,
                ));
        }
        let mut on_tick = |_: crate::settings_ui::TickContext| None;
        let mut app = SettingsApp {
            on_tick: &mut on_tick,
            config_dir: std::path::PathBuf::new(),
            state: SettingsState::from_config(&config),
            monitor: crate::settings_ui::MonitorGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            exit: Rc::new(RefCell::new(None)),
            baseline: Rc::new(RefCell::new(crate::settings_ui::save::Baseline {
                config: config.clone(),
                start_with_windows: false,
            })),
            applied_note: false,
            apply_error: None,
            pending_status_messages: Vec::new(),
            keyboard_hook: None,
            cursor_previews: None,
            update_check: crate::settings_ui::UpdateCheck::Idle,
            update_install: crate::settings_ui::UpdateInstall::Idle,
            exe_icons: Default::default(),
            window_chooser: Default::default(),
        };
        let ctx = egui::Context::default();
        let mut output: Option<egui::FullOutput> = None;
        let mut content_right = 0.0;
        for _ in 0..4 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 400.0),
                )),
                ..Default::default()
            };
            if let Some(previous) = output.as_mut() {
                previous.textures_delta.clear();
            }
            output = Some(ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            content_right = ui.max_rect().right();
                            app.games_section(ui);
                        });
                });
            }));
        }
        let mut last = output.unwrap();
        last.textures_delta.clear();
        (last, content_right)
    }

    #[test]
    fn remove_buttons_stay_inside_the_window_and_centred_with_their_row() {
        // 300 is narrower than the fixed columns plus the icon.
        for width in [640.0, 560.0, 300.0] {
            let (output, content_right) = render_games(width);
            let removes = text_rects(&output, "Remove");
            let names = [
                text_rects(&output, "Last Epoch")[0],
                text_rects(&output, "World of Warcraft")[0],
                text_rects(&output, "An Extremely")[0],
            ];
            let checks = text_rects(&output, "Auto-apply");
            assert_eq!(removes.len(), 3, "{width}");
            for (_, right, _) in &removes {
                assert!(
                    *right <= content_right,
                    "Remove overflows at {width}: {right} > {content_right}"
                );
            }
            for (remove, name) in removes.iter().zip(names) {
                assert!(
                    (remove.2 - name.2).abs() < 1.0,
                    "{width}: {remove:?} vs {name:?}"
                );
            }
            assert!((removes[0].2 - checks[0].2).abs() < 1.0);
        }
    }

    #[test]
    fn a_long_game_name_is_truncated_before_the_checkboxes() {
        let (output, _) = render_games(560.0);
        let long = text_rects(&output, "An Extremely")[0];
        let auto_apply = text_rects(&output, "Auto-apply")[2];
        assert!(
            long.1 <= auto_apply.0,
            "name ends at {} but the checkbox label starts at {}",
            long.1,
            auto_apply.0
        );
    }

    #[test]
    fn row_stripes_are_contiguous() {
        let (output, _) = render_games(640.0);
        let faint = egui::Visuals::default().faint_bg_color;
        let stripes = stripes(&output, faint);
        assert_eq!(stripes.len(), 2, "rows 0 and 2 are striped");
        let pitch =
            text_rects(&output, "World of Warcraft")[0].2 - text_rects(&output, "Last Epoch")[0].2;
        for (top, bottom) in &stripes {
            assert!(
                (bottom - top - pitch).abs() < 0.5,
                "stripe is {} tall, row pitch is {pitch}",
                bottom - top
            );
        }
        assert!(
            (stripes[1].0 - stripes[0].1 - pitch).abs() < 0.5,
            "the unstriped row between them leaves a different gap"
        );
    }
}
