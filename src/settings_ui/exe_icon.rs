//! Executable icons for the Games tab: the Shell icon of an exe, extracted on
//! a worker thread, uploaded as an egui texture once per path and cached for
//! the session.

use crate::cursor_style::Bitmap;
use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
use windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon;

type Extracted = (PathBuf, Option<egui::ColorImage>);

struct Worker {
    requests: Sender<PathBuf>,
    results: Receiver<Extracted>,
}

/// Per-session icon cache. Extraction runs on one worker thread, once per
/// path; a path whose extraction failed is cached as `None`, so it is never
/// retried (or re-read from disk) on later frames.
#[derive(Default)]
pub(super) struct ExeIcons {
    /// `None` value: extraction failed (or is still in flight -- see `requested`).
    textures: HashMap<PathBuf, Option<egui::TextureHandle>>,
    requested: std::collections::HashSet<PathBuf>,
    worker: Option<Worker>,
}

impl ExeIcons {
    /// The icon texture for `exe_path`; `None` while it is being extracted
    /// or when the exe has no readable icon (the caller draws a generic one).
    pub(super) fn get(
        &mut self,
        ctx: &egui::Context,
        exe_path: &Path,
    ) -> Option<&egui::TextureHandle> {
        self.receive(ctx);
        if !self.requested.contains(exe_path) {
            self.requested.insert(exe_path.to_path_buf());
            let worker = self.worker.get_or_insert_with(|| spawn_worker(ctx));
            // A send failure means the worker died; the icon stays generic.
            let _ = worker.requests.send(exe_path.to_path_buf());
        }
        self.textures.get(exe_path)?.as_ref()
    }

    /// Uploads every image the worker has finished since the last frame.
    fn receive(&mut self, ctx: &egui::Context) {
        let Some(worker) = &self.worker else { return };
        while let Ok((path, image)) = worker.results.try_recv() {
            let texture = image.map(|image| {
                ctx.load_texture(
                    format!("exe_icon:{}", path.display()),
                    image,
                    egui::TextureOptions::LINEAR,
                )
            });
            self.textures.insert(path, texture);
        }
    }
}

fn spawn_worker(ctx: &egui::Context) -> Worker {
    let (request_tx, request_rx) = channel::<PathBuf>();
    let (result_tx, result_rx) = channel::<Extracted>();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        // SAFETY: plain COM init for this thread, paired with the uninit below.
        let com = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        while let Ok(path) = request_rx.recv() {
            let image = load(&path).and_then(|bitmap| to_color_image(&bitmap));
            if result_tx.send((path, image)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
        if com >= 0 {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    });
    Worker {
        requests: request_tx,
        results: result_rx,
    }
}

/// Converts an icon bitmap (premultiplied RGBA) to an egui image, or `None`
/// when its pixel buffer does not match its dimensions.
fn to_color_image(bitmap: &Bitmap) -> Option<egui::ColorImage> {
    let (width, height) = (bitmap.width as usize, bitmap.height as usize);
    (width > 0 && height > 0 && bitmap.pixels.len() == width * height * 4)
        .then(|| egui::ColorImage::from_rgba_premultiplied([width, height], &bitmap.pixels))
}

/// Reads the large Shell icon of the file at `path`. Never panics; `None` if
/// the file is missing or the icon can't be read.
fn load(path: &Path) -> Option<Bitmap> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    // SAFETY: `wide` is NUL-terminated and `info` is a valid out-pointer of
    // the size passed.
    let ok = unsafe {
        SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut info,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if ok == 0 || info.hIcon.is_null() {
        return None;
    }
    let image = crate::cursor_overlay::read_cursor_image(info.hIcon);
    // SAFETY: SHGetFileInfoW handed us ownership of this icon.
    unsafe { DestroyIcon(info.hIcon) };
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_a_matching_bitmap() {
        let mut bitmap = Bitmap::blank(2, 1, 0, 0);
        bitmap.pixels = vec![1, 2, 3, 255, 0, 0, 0, 0];
        let image = to_color_image(&bitmap).expect("valid bitmap");
        assert_eq!(image.size, [2, 1]);
    }

    #[test]
    fn rejects_a_mismatched_pixel_buffer() {
        let mut bitmap = Bitmap::blank(2, 2, 0, 0);
        bitmap.pixels.truncate(4);
        assert!(to_color_image(&bitmap).is_none());
    }

    #[test]
    fn a_missing_exe_yields_no_icon() {
        assert!(load(Path::new(r"C:\definitely\not\here.exe")).is_none());
    }
}
