//! The Cursor tab: the "Game cursor" section and its rendered previews.

use super::SettingsApp;
use crate::cursor_style;
use crate::cursor_style::CursorShape;
use crate::profile_store::CursorSettings;

/// The rendered game-cursor preview and variant icons, plus the settings
/// they were rendered from.
pub(super) struct CursorPreviews {
    settings: CursorSettings,
    preview: egui::TextureHandle,
    shapes: Vec<(CursorShape, egui::TextureHandle)>,
}

impl SettingsApp<'_> {
    /// Re-renders the variant icons and the preview whenever the cursor
    /// settings changed since the last frame -- with the same rasterizer the
    /// overlay uses, so the preview is exactly what appears in game.
    fn refresh_cursor_previews(&mut self, ctx: &egui::Context) {
        let settings = self.state.cursor.sanitized();
        if self
            .cursor_previews
            .as_ref()
            .is_some_and(|p| p.settings == settings)
        {
            return;
        }
        let game_cursor = crate::cursor_overlay::last_game_cursor();
        let to_texture = |name: String, s: &CursorSettings| {
            let bitmap = cursor_style::render_preview(s, game_cursor.as_ref());
            let size = [bitmap.width as usize, bitmap.height as usize];
            let image = egui::ColorImage::from_rgba_premultiplied(size, &bitmap.pixels);
            ctx.load_texture(name, image, egui::TextureOptions::LINEAR)
        };
        let shapes = CursorShape::ALL
            .into_iter()
            .map(|shape| {
                let icon = CursorSettings {
                    shape,
                    size: 30,
                    thickness: settings.thickness.clamp(2, 4),
                    border_width: settings.border_width.min(2),
                    glow_size: settings.glow_size.min(5),
                    ..settings.clone()
                };
                (shape, to_texture(format!("cursor-icon-{shape:?}"), &icon))
            })
            .collect();
        let preview = to_texture("cursor-preview".to_string(), &settings);
        self.cursor_previews = Some(CursorPreviews {
            settings,
            preview,
            shapes,
        });
    }

    /// Renders the "Cursor" tab: pointer variant, size, colors, border,
    /// glow, a live preview on a dark and a light background, and the
    /// pulse/hide behavior.
    pub(super) fn cursor_section(&mut self, ui: &mut egui::Ui) {
        self.refresh_cursor_previews(ui.ctx());
        let Some(previews) = self.cursor_previews.as_ref() else {
            return;
        };
        let c = &mut self.state.cursor;

        ui.checkbox(&mut c.enabled, "Show the game cursor");
        ui.label(
            egui::RichText::new(
                "A big, high-contrast pointer for games whose own cursor gets lost in the action. \
                 Shake the mouse to make it pulse.",
            )
            .weak(),
        );

        ui.label("Pointer:");
        ui.horizontal_wrapped(|ui| {
            for (shape, texture) in &previews.shapes {
                let image = egui::Image::from_texture(egui::load::SizedTexture::new(
                    texture.id(),
                    texture.size_vec2(),
                ))
                .fit_to_exact_size(egui::vec2(32.0, 32.0));
                let button =
                    egui::Button::image_and_text(image, shape.label()).selected(c.shape == *shape);
                if ui.add(button).clicked() {
                    c.shape = *shape;
                }
            }
        });
        if c.shape == CursorShape::GameOwn {
            ui.label(
                egui::RichText::new(
                    "Builds on the game's own cursor: an enlarged copy behind it (Size; at 32 px or less, \
                     only the border and glow), following its outline and switching with it. The \
                     preview shows the last cursor seen in a game (an arrow until then).",
                )
                .weak(),
            );
        }

        egui::Grid::new("game_cursor_style")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Size");
                ui.add(egui::Slider::new(&mut c.size, cursor_style::SIZE_RANGE).suffix(" px"));
                ui.end_row();

                if c.shape.uses_thickness() {
                    ui.label("Thickness");
                    ui.add(
                        egui::Slider::new(&mut c.thickness, cursor_style::THICKNESS_RANGE)
                            .suffix(" px"),
                    );
                    ui.end_row();
                }

                if c.uses_color() {
                    ui.label("Color");
                    ui.color_edit_button_srgb(&mut c.color);
                    ui.end_row();
                }

                ui.label("Opacity");
                ui.add(egui::Slider::new(&mut c.opacity, cursor_style::OPACITY_RANGE).suffix(" %"));
                ui.end_row();

                ui.checkbox(&mut c.border, "Border");
                ui.add_enabled_ui(c.border, |ui| {
                    ui.horizontal(|ui| {
                        ui.color_edit_button_srgb(&mut c.border_color);
                        ui.add(
                            egui::Slider::new(
                                &mut c.border_width,
                                cursor_style::BORDER_WIDTH_RANGE,
                            )
                            .suffix(" px"),
                        );
                    });
                });
                ui.end_row();

                ui.checkbox(&mut c.glow, "Glow");
                ui.add_enabled_ui(c.glow, |ui| {
                    ui.horizontal(|ui| {
                        ui.color_edit_button_srgb(&mut c.glow_color);
                        ui.add(
                            egui::Slider::new(&mut c.glow_size, cursor_style::GLOW_SIZE_RANGE)
                                .suffix(" px"),
                        );
                    });
                });
                ui.end_row();

                ui.label("");
                ui.add_enabled_ui(c.glow, |ui| {
                    ui.add(
                        egui::Slider::new(&mut c.glow_strength, cursor_style::GLOW_STRENGTH_RANGE)
                            .text("glow strength %"),
                    );
                });
                ui.end_row();
            });

        ui.horizontal(|ui| {
            for background in [egui::Color32::from_gray(24), egui::Color32::from_gray(225)] {
                const BOX: f32 = 150.0;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(BOX, BOX), egui::Sense::hover());
                ui.painter().rect_filled(rect, 4.0, background);
                ui.painter().rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(1.0, egui::Color32::GRAY),
                    egui::StrokeKind::Inside,
                );
                let size = previews.preview.size_vec2();
                let scale = (BOX - 10.0) / size.x.max(size.y);
                let shown = if scale < 1.0 { size * scale } else { size };
                let image_rect = egui::Rect::from_center_size(rect.center(), shown);
                ui.painter().image(
                    previews.preview.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        });

        ui.label(
            egui::RichText::new(
                "Shown only while the mouse is over a game with \"Game cursor\" ticked on the Games tab.",
            )
            .weak(),
        );
        ui.checkbox(
            &mut c.shake_to_find,
            "Shake the mouse to make the cursor pulse",
        );
        ui.checkbox(&mut c.hide_when_cursor_hidden, "Hide when the game hides its cursor")
            .on_hover_text("Leave off for games that draw their own cursor -- they hide the Windows one all the time.");
    }
}
