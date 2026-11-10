//! Small egui helpers shared by the Settings tabs and the About section.

/// Points of empty space above and below a section separator.
const SECTION_SPACING: f32 = 8.0;

/// A separator with visible space on both sides.
pub(crate) fn section_separator(ui: &mut egui::Ui) {
    ui.add_space(SECTION_SPACING);
    ui.separator();
    ui.add_space(SECTION_SPACING);
}
