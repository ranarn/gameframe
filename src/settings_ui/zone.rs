//! The Zone section: the pure helpers that turn the Zone into what a
//! thumbnail painter needs, and the section itself.

use super::{SettingsApp, SettingsExit, persist_zone};
use crate::profile_store::Zone;
use crate::region_calc::{MonitorGeometry, Region, resolve_region};

/// A resolved [`Region`] expressed as proportions (`0.0..=1.0`) of the
/// monitor it was resolved against, so a fixed-size thumbnail area can
/// render it without knowing real monitor pixel dimensions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

fn normalize_region(region: &Region, monitor: &MonitorGeometry) -> NormalizedRegion {
    NormalizedRegion {
        x: (region.x - monitor.x) as f32 / monitor.width as f32,
        y: (region.y - monitor.y) as f32 / monitor.height as f32,
        width: region.width as f32 / monitor.width as f32,
        height: region.height as f32 / monitor.height as f32,
    }
}

/// What the Settings "Zone" section shows for the current Zone against the
/// current monitor.
#[derive(Debug, Clone, PartialEq)]
pub enum ZoneView {
    /// No Zone is set: the section shows "No Zone set" and a "Draw Zone"
    /// button.
    NotSet,
    /// The Zone resolves on this monitor: draw it, normalized, on the
    /// monitor thumbnail.
    Resolved(NormalizedRegion),
    /// A Zone is set but can't be resolved on the current monitor (e.g.
    /// coordinates persisted while running on a different monitor
    /// configuration). Carries the red reason to show.
    Unresolvable(String),
}

/// Whether the Games and Cursor tabs' controls are usable: only once a Zone
/// exists (even one that doesn't fit this monitor -- the user already made
/// it). Without one they are greyed out and the Zone section says why.
pub fn zone_dependent_tabs_enabled(zone: Option<&Zone>) -> bool {
    zone.is_some()
}

/// Shown on the greyed-out tabs while no Zone exists.
pub(super) const ZONE_REQUIRED_NOTE: &str = "A Zone must be configured first. Open the \"Zone, hotkeys & taskbar\" tab and click \"Draw Zone\".";

/// Shown in the Zone section itself while no Zone exists.
const ZONE_SECTION_REQUIRED_NOTE: &str = "A Zone must be configured first: click \"Draw Zone\" to set the screen area games are placed into.";

/// Decides what the Zone section shows for `zone` against `monitor` -- the
/// pure data behind the thumbnail painter and the red unresolvable reason.
pub fn zone_view(zone: Option<&Zone>, monitor: &MonitorGeometry) -> ZoneView {
    match zone {
        None => ZoneView::NotSet,
        Some(zone) => match resolve_region(zone, monitor) {
            Ok(region) => ZoneView::Resolved(normalize_region(&region, monitor)),
            Err(e) => ZoneView::Unresolvable(e.to_string()),
        },
    }
}

impl SettingsApp<'_> {
    /// Renders the Zone section: a monitor-outline thumbnail with the Zone
    /// drawn on it (or "No Zone set"), a red reason when the Zone can't be
    /// resolved on the current monitor (see [`zone_view`]), and a
    /// "Draw Zone"/"Edit Zone" button that closes Settings to open the
    /// zone-editor overlay (blank or pre-filled).
    pub(super) fn zone_section(&mut self, ui: &mut egui::Ui) {
        const THUMBNAIL_SIZE: egui::Vec2 = egui::Vec2::new(128.0, 72.0);
        let fill_color = egui::Color32::from_rgb(90, 170, 250);
        let view = zone_view(self.state.zone.as_ref(), &self.monitor);

        ui.horizontal(|ui| {
            let (outline_rect, _response) =
                ui.allocate_exact_size(THUMBNAIL_SIZE, egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_stroke(
                outline_rect,
                2.0,
                egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                egui::StrokeKind::Outside,
            );
            match &view {
                ZoneView::Resolved(region) => {
                    let highlight_rect = egui::Rect::from_min_size(
                        outline_rect.min
                            + egui::vec2(region.x * THUMBNAIL_SIZE.x, region.y * THUMBNAIL_SIZE.y),
                        egui::vec2(
                            region.width * THUMBNAIL_SIZE.x,
                            region.height * THUMBNAIL_SIZE.y,
                        ),
                    );
                    painter.rect_filled(highlight_rect, 0.0, fill_color);
                }
                ZoneView::Unresolvable(_) => {
                    // Cross out the thumbnail in red so an unresolvable Zone
                    // is visible rather than silently drawing nothing.
                    painter.line_segment(
                        [outline_rect.left_top(), outline_rect.right_bottom()],
                        egui::Stroke::new(1.5, egui::Color32::RED),
                    );
                    painter.line_segment(
                        [outline_rect.right_top(), outline_rect.left_bottom()],
                        egui::Stroke::new(1.5, egui::Color32::RED),
                    );
                }
                ZoneView::NotSet => {}
            }

            ui.vertical(|ui| {
                match &self.state.zone {
                    Some(zone) => {
                        ui.monospace(format!(
                            "({}, {}, {}x{})",
                            zone.x, zone.y, zone.width, zone.height
                        ));
                    }
                    None => {
                        ui.label("No Zone set");
                        ui.colored_label(ui.visuals().warn_fg_color, ZONE_SECTION_REQUIRED_NOTE);
                    }
                }
                if let ZoneView::Unresolvable(reason) = &view {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Unavailable: {reason}"),
                    )
                    .on_hover_text("The Zone doesn't fit the current monitor. Edit it to fix.");
                }
                ui.horizontal(|ui| {
                    let label = if self.state.zone.is_some() {
                        "Edit Zone"
                    } else {
                        "Draw Zone"
                    };
                    if ui.button(label).clicked() {
                        *self.exit.borrow_mut() = Some(SettingsExit::EditZone(
                            self.state.clone(),
                            self.pending_status_messages.clone(),
                        ));
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if self.state.zone.is_some() && ui.button("Clear").clicked() {
                        // Like an overlay Save, Clear persists immediately so
                        // disk and Settings never disagree about the Zone.
                        match persist_zone(&self.config_dir, None) {
                            Ok(_) => self.state.clear_zone(),
                            Err(e) => {
                                log!("failed to clear the zone: {e}");
                                self.pending_status_messages =
                                    vec![format!("couldn't clear the Zone: {e}")];
                            }
                        }
                    }
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_monitor() -> MonitorGeometry {
        MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn normalize_region_expresses_a_region_as_proportions_of_the_monitor() {
        let monitor = test_monitor();
        let region = Region {
            x: 960,
            y: 0,
            width: 960,
            height: 1080,
        };

        let normalized = normalize_region(&region, &monitor);

        assert_eq!(
            normalized,
            NormalizedRegion {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 1.0
            }
        );
    }

    #[test]
    fn normalize_region_accounts_for_a_monitor_with_a_non_zero_origin() {
        let monitor = MonitorGeometry {
            x: -2560,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let region = Region {
            x: -1280,
            y: 0,
            width: 1280,
            height: 1440,
        };

        let normalized = normalize_region(&region, &monitor);

        assert_eq!(
            normalized,
            NormalizedRegion {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 1.0
            }
        );
    }

    #[test]
    fn zone_dependent_tabs_are_disabled_without_a_zone_and_enabled_with_one() {
        assert!(!zone_dependent_tabs_enabled(None));
        let zone = Zone {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(zone_dependent_tabs_enabled(Some(&zone)));
    }

    #[test]
    fn zone_view_is_not_set_when_there_is_no_zone() {
        assert_eq!(zone_view(None, &test_monitor()), ZoneView::NotSet);
    }

    #[test]
    fn zone_view_normalizes_a_zone_that_fits_the_monitor() {
        let zone = Zone {
            x: 960,
            y: 0,
            width: 960,
            height: 1080,
        };

        let view = zone_view(Some(&zone), &test_monitor());

        assert_eq!(
            view,
            ZoneView::Resolved(NormalizedRegion {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 1.0
            })
        );
    }

    #[test]
    fn zone_view_surfaces_a_reason_for_a_zone_that_does_not_fit_the_monitor() {
        // Simulates a Zone persisted while GameFrame ran on a different
        // (e.g. 4K) monitor: its origin lies outside `test_monitor`'s bounds.
        let zone = Zone {
            x: 5000,
            y: 5000,
            width: 200,
            height: 100,
        };

        let view = zone_view(Some(&zone), &test_monitor());

        assert!(
            matches!(view, ZoneView::Unresolvable(ref reason) if !reason.is_empty()),
            "got {view:?}"
        );
    }
}
