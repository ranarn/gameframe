//! The Games tab's "Apply to Window" button: lists the same windows as the tray
//! submenu and hands the chosen one to the tray process.
//!
//! The list is the tray picker's own ([`crate::window_picker::list_entries`]),
//! built on a worker thread because it is slow (see that module). Choosing a
//! window only sends it over the link: tracking the game and applying the Zone
//! run in the tray process, which owns the per-window registry and the drift
//! hooks.

use crate::window_picker::PickerEntry;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

/// Tallest the window list gets before it scrolls, in points.
const LIST_MAX_HEIGHT: f32 = 320.0;

/// Requests a repaint when dropped.
struct RepaintOnDrop(egui::Context);

impl Drop for RepaintOnDrop {
    fn drop(&mut self) {
        self.0.request_repaint();
    }
}

/// What the button and its popup are doing.
#[derive(Default)]
pub(super) struct WindowChooser {
    /// The worker building the list, until its result arrives.
    loading: Option<Receiver<Vec<PickerEntry>>>,
    /// Whether the popup is showing `entries`.
    open: bool,
    entries: Vec<PickerEntry>,
}

impl WindowChooser {
    /// Starts building the list on a worker thread.
    fn start(&mut self, config_dir: PathBuf, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("settings-window-list".into())
            .spawn(move || {
                // Wakes the UI on every exit path, a panic included. Locals drop
                // in reverse order, so `tx` (moved into a local declared after
                // `_wake`) drops first and the frame the repaint triggers
                // sees the disconnect rather than an empty channel.
                let _wake = RepaintOnDrop(ctx);
                let tx = tx;
                let _ = tx.send(crate::window_picker::list_entries(&config_dir));
            });
        match spawned {
            Ok(_) => self.loading = Some(rx),
            Err(e) => log!("couldn't start the window list: {e}"),
        }
    }

    /// Takes the finished list, if any, and opens the popup on it.
    fn poll(&mut self) {
        let Some(rx) = &self.loading else { return };
        match rx.try_recv() {
            Ok(entries) => {
                self.entries = entries;
                self.open = true;
                self.loading = None;
            }
            Err(TryRecvError::Empty) => {}
            // The worker died without a list.
            Err(TryRecvError::Disconnected) => self.loading = None,
        }
    }

    /// Draws the button and, once its list is ready, the popup under it.
    /// Returns the window (`HWND`) the user chose, if they did this frame.
    /// Dismissing the popup (a click elsewhere, Escape) returns `None` and
    /// changes nothing.
    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        config_dir: &std::path::Path,
    ) -> Option<isize> {
        self.poll();
        let label = if self.loading.is_some() {
            "Looking for windows..."
        } else {
            "Apply to Window"
        };
        let button = ui.add_enabled(self.loading.is_none(), egui::Button::new(label));
        if button.clicked() {
            self.open = false;
            self.start(config_dir.to_path_buf(), ui.ctx());
        }

        let mut chosen = None;
        if self.open {
            egui::Popup::from_response(&button)
                .open_bool(&mut self.open)
                .width(400.0)
                .layout(egui::Layout::top_down_justified(egui::Align::Min))
                .show(|ui| {
                    if self.entries.is_empty() {
                        ui.label("No windows found.");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(LIST_MAX_HEIGHT)
                        .show(ui, |ui| {
                            for (hwnd, title) in &self.entries {
                                if ui
                                    .add(egui::Button::new(title).truncate())
                                    .on_hover_text(title)
                                    .clicked()
                                {
                                    chosen = Some(*hwnd);
                                }
                            }
                        });
                });
        }
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chooser_loading(rx: Receiver<Vec<PickerEntry>>) -> WindowChooser {
        WindowChooser {
            loading: Some(rx),
            ..WindowChooser::default()
        }
    }

    #[test]
    fn a_finished_list_opens_the_popup() {
        let (tx, rx) = channel();
        let mut chooser = chooser_loading(rx);
        tx.send(vec![(1, "Game".to_string())]).unwrap();

        chooser.poll();

        assert!(chooser.open && chooser.loading.is_none());
        assert_eq!(chooser.entries, vec![(1, "Game".to_string())]);
    }

    #[test]
    fn a_pending_list_keeps_waiting() {
        let (_tx, rx) = channel();
        let mut chooser = chooser_loading(rx);

        chooser.poll();

        assert!(!chooser.open && chooser.loading.is_some());
    }

    #[test]
    fn a_worker_that_died_re_enables_the_button_without_opening_the_popup() {
        let (tx, rx) = channel::<Vec<PickerEntry>>();
        let mut chooser = chooser_loading(rx);
        drop(tx);

        chooser.poll();

        assert!(!chooser.open && chooser.loading.is_none());
    }
}
