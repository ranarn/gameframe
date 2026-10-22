//! OS-facing half of the game cursor (the pure half is
//! [`crate::cursor_style`]).
//!
//! A dedicated thread owns a click-through, always-on-top layered window
//! (`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW |
//! WS_EX_NOACTIVATE`) and, about every 4 ms while the game cursor is shown,
//! moves it so the rendered pointer's hotspot sits on the real cursor. It
//! never touches the game process -- no injection, no hooks into the game --
//! which is what keeps it safe next to anti-cheat. The window is a tool
//! window, so the "Apply to Window" picker's filter already skips it.
//!
//! Why a thread of its own rather than the tray's message loop: that loop
//! blocks for as long as Settings or the zone editor is open (see
//! [`crate::app::run_event_loop`]), and the cursor must keep following the
//! mouse regardless. Win32 windows are thread-affine, so the window is
//! created, moved and destroyed on this one thread; the rest of the app talks
//! to it only through [`CursorOverlay`]'s shared state.
//!
//! The thread is per-monitor-DPI-aware (`SetThreadDpiAwarenessContext`), so
//! cursor positions and the window are in physical pixels on every monitor
//! regardless of the rest of the process's awareness.
//!
//! [`CursorMethod::ReplaceSystemCursor`] additionally swaps the Windows arrow
//! and hand cursors (`SetSystemCursor`) while the game cursor is shown, and
//! puts the user's scheme back (`SPI_SETCURSORS`) the moment it isn't. A
//! marker file records an active swap, so if GameFrame dies mid-swap the next
//! start restores the scheme.
//!
//! Not exercised by automated tests (needs a desktop session), like the other
//! OS-facing modules; [`foreground_has_game_cursor`] is the pure part.

use crate::cursor_style::{self, Bitmap, CursorMethod, CursorSettings, ShakeDetector};
use crate::profile_store::{self, Config};
use crate::win32::encode_wide;
use crate::window_filter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateBitmap,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, SelectObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyCursor,
    DestroyWindow, DispatchMessageW, GetCursorInfo, GetCursorPos, GetForegroundWindow, HCURSOR,
    HTTRANSPARENT, HWND_TOPMOST, ICONINFO, MSG, MsgWaitForMultipleObjects, OCR_HAND, OCR_NORMAL,
    PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassExW, SPI_SETCURSORS, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetSystemCursor,
    SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage, ULW_ALPHA,
    UpdateLayeredWindow, WM_NCHITTEST, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// How often the window follows the cursor while shown (~250 Hz, so it keeps
/// up with high-refresh monitors).
const FRAME_INTERVAL: Duration = Duration::from_millis(4);
/// How often a pulse frame is re-rendered (~60 Hz); between renders the
/// window only moves.
const PULSE_FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// How often to re-check whether the game cursor should be shown while it
/// isn't.
const IDLE_INTERVAL: Duration = Duration::from_millis(50);
/// How often to re-assert `HWND_TOPMOST` while shown, in case the game made
/// its own window topmost after ours.
const TOPMOST_INTERVAL: Duration = Duration::from_millis(500);
/// Marker written while the system cursors are swapped; see module docs.
const SWAP_MARKER_FILE: &str = "cursor-swap.active";
const WINDOW_CLASS: &str = "GameFrameGameCursor";

/// What the overlay thread needs from the config.
#[derive(Debug, Clone, Default)]
struct OverlayInput {
    settings: CursorSettings,
    /// Executable paths of the tracked games with their game-cursor flag on.
    games: Vec<PathBuf>,
}

impl OverlayInput {
    fn from_config(config: &Config) -> Self {
        OverlayInput {
            settings: config.cursor.sanitized(),
            games: config
                .tracked_games
                .iter()
                .filter(|g| g.game_cursor)
                .map(|g| g.exe_path.clone())
                .collect(),
        }
    }
}

/// Whether `exe` is one of `games` -- the tracked games with the game
/// cursor on. Same exact-path matching as
/// [`profile_store::find_tracked_game`].
fn foreground_has_game_cursor(games: &[PathBuf], exe: &Path) -> bool {
    games.iter().any(|g| g == exe)
}

struct Shared {
    input: Mutex<OverlayInput>,
    /// Bumped on every [`CursorOverlay::reload`] so the thread knows to
    /// re-read `input` and re-render.
    generation: AtomicU64,
    pulse: AtomicBool,
    quit: AtomicBool,
}

/// Handle to the running game-cursor thread. Dropping it stops the thread,
/// hides the window and restores swapped system cursors.
pub struct CursorOverlay {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl CursorOverlay {
    /// Starts the thread with the config currently on disk. The thread runs
    /// for the life of the app even with the game cursor off (it then only
    /// wakes every [`IDLE_INTERVAL`] to check), so turning it on is instant.
    pub fn start(config_dir: &Path) -> Self {
        install_panic_restore_hook();
        let shared = Arc::new(Shared {
            input: Mutex::new(load_input(config_dir).unwrap_or_default()),
            generation: AtomicU64::new(0),
            pulse: AtomicBool::new(false),
            quit: AtomicBool::new(false),
        });
        let marker = config_dir.join(SWAP_MARKER_FILE);
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("game-cursor".to_string())
            .spawn(move || run_overlay_thread(&thread_shared, &marker))
            .map_err(|e| eprintln!("failed to start the game cursor thread: {e}"))
            .ok();
        CursorOverlay { shared, thread }
    }

    /// Re-reads the config from disk -- call after anything that may have
    /// changed the cursor settings or the tracked games. A config that fails
    /// to load keeps the last good settings rather than switching the game
    /// cursor off.
    pub fn reload(&self, config_dir: &Path) {
        let Some(input) = load_input(config_dir) else {
            return;
        };
        *self.shared.input.lock().unwrap_or_else(|e| e.into_inner()) = input;
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Plays the find-my-cursor pulse (if the game cursor is currently
    /// shown).
    pub fn pulse(&self) {
        self.shared.pulse.store(true, Ordering::SeqCst);
    }
}

impl Drop for CursorOverlay {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn load_input(config_dir: &Path) -> Option<OverlayInput> {
    match profile_store::load(config_dir) {
        Ok(config) => Some(OverlayInput::from_config(&config)),
        Err(e) => {
            eprintln!("game cursor: failed to load config: {e}");
            None
        }
    }
}

/// The thread body: see the module docs.
fn run_overlay_thread(shared: &Shared, marker: &Path) {
    // Safety: plain call; affects only this thread.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    if marker.exists() {
        restore_system_cursors(marker);
    }
    let Some(mut window) = LayeredWindow::create() else {
        eprintln!("game cursor: failed to create the overlay window");
        return;
    };
    let timer = FrameTimer::new();
    let clock = Instant::now();

    let mut seen_generation = u64::MAX;
    let mut input = OverlayInput::default();
    // The pointer on its tight canvas, rendered lazily after each reload.
    let mut pointer: Option<Bitmap> = None;
    // The pulse's large canvas with (or without, while the system cursor is
    // swapped) the pointer drawn in, keyed by whether it has the pointer.
    let mut pulse_base: Option<(bool, Bitmap)> = None;
    // Reused pulse frame buffer.
    let mut frame = Bitmap::blank(0, 0, 0, 0);
    let mut last_pulse_render: Option<Instant> = None;
    // What the window currently holds (so a frame without new content only
    // needs a move).
    let mut content = Content::Nothing;
    let mut swapped = false;
    let mut swap_failed = false;
    let mut foreground: Option<(HWND, bool)> = None;
    let mut shake = ShakeDetector::new();
    let mut pulse_start: Option<Instant> = None;
    let mut last_topmost = Instant::now();

    while !shared.quit.load(Ordering::SeqCst) {
        pump_messages();

        // Read before `generation`: `pulse()` is called after `reload()`, so
        // seeing the pulse here guarantees the matching reload is seen below
        // (otherwise a hotkey "turn on + pulse" could be consumed against the
        // old, disabled input).
        let pulse_requested = shared.pulse.swap(false, Ordering::SeqCst);
        let generation = shared.generation.load(Ordering::SeqCst);
        if generation != seen_generation {
            seen_generation = generation;
            input = shared
                .input
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            pointer = None;
            pulse_base = None;
            content = Content::Nothing;
            foreground = None;
            swap_failed = false;
            if swapped {
                restore_system_cursors(marker);
                swapped = false;
            }
        }
        let settings = &input.settings;

        // Safety: plain query.
        let fg = unsafe { GetForegroundWindow() };
        let fg_has_game_cursor = match foreground {
            Some((hwnd, has)) if hwnd == fg => has,
            // Nothing to look up while the game cursor is off.
            _ if !settings.enabled => false,
            _ => match (!fg.is_null()).then(|| window_filter::exe_path_for_window(fg)).flatten() {
                Some(exe) => {
                    let has = foreground_has_game_cursor(&input.games, &exe);
                    foreground = Some((fg, has));
                    has
                }
                // Not cached: a lookup that failed transiently is retried on
                // the next tick instead of sticking until focus changes.
                None => false,
            },
        };

        let cursor = read_cursor();
        let show = cursor
            .is_some_and(|c| cursor_style::should_show(settings, fg_has_game_cursor, c.showing));
        let Some(cursor) = cursor.filter(|_| show) else {
            window.hide();
            if swapped {
                restore_system_cursors(marker);
                swapped = false;
            }
            shake.reset();
            pulse_start = None;
            timer.wait(IDLE_INTERVAL);
            continue;
        };

        if pulse_requested
            || (settings.shake_to_find && shake.feed(clock.elapsed(), cursor.x, cursor.y))
        {
            pulse_start = Some(Instant::now());
        }

        if settings.method == CursorMethod::ReplaceSystemCursor && !swapped && !swap_failed {
            swapped = swap_system_cursors(settings, marker);
            swap_failed = !swapped;
        }
        // With the system cursor swapped the overlay only carries the pulse;
        // if the swap failed it falls back to drawing the pointer itself.
        let draw_pointer = !swapped;

        let pulse =
            pulse_start.and_then(|start| cursor_style::pulse_frame(settings, start.elapsed()));
        if pulse.is_none() {
            pulse_start = None;
        }

        match pulse {
            Some((radius, alpha)) => {
                // The pulse needs the large canvas; the pointer is drawn into
                // it once per pulse-canvas render, then each pulse frame only
                // copies that base and adds the ring.
                let base = match &pulse_base {
                    Some((with_pointer, base)) if *with_pointer == draw_pointer => base,
                    _ => {
                        let (side, hot) = cursor_style::overlay_canvas(settings);
                        let base = if draw_pointer {
                            cursor_style::render_pointer(settings, side, side, hot, hot)
                        } else {
                            Bitmap::blank(side, side, hot, hot)
                        };
                        &pulse_base.insert((draw_pointer, base)).1
                    }
                };
                let pos = (cursor.x - base.hotspot_x, cursor.y - base.hotspot_y);
                let due = last_pulse_render.is_none_or(|t| t.elapsed() >= PULSE_FRAME_INTERVAL);
                if content == Content::Pulse && !due {
                    window.move_to(pos);
                } else {
                    if frame.pixels.len() == base.pixels.len() {
                        frame.pixels.copy_from_slice(&base.pixels);
                        frame.width = base.width;
                        frame.height = base.height;
                        frame.hotspot_x = base.hotspot_x;
                        frame.hotspot_y = base.hotspot_y;
                    } else {
                        frame = base.clone();
                    }
                    cursor_style::draw_pulse_ring(&mut frame, settings, radius, alpha);
                    window.present(&frame, pos);
                    content = Content::Pulse;
                    last_pulse_render = Some(Instant::now());
                }
                window.show();
            }
            None if draw_pointer => {
                let pointer = pointer.get_or_insert_with(|| {
                    let (w, h, hx, hy) = cursor_style::cursor_canvas(settings);
                    cursor_style::render_pointer(settings, w, h, hx, hy)
                });
                let pos = (cursor.x - pointer.hotspot_x, cursor.y - pointer.hotspot_y);
                if content == Content::Pointer {
                    window.move_to(pos);
                } else if window.present(pointer, pos) {
                    content = Content::Pointer;
                }
                if content == Content::Pointer {
                    window.show();
                }
            }
            None => {
                window.hide();
                content = Content::Nothing;
            }
        }
        if window.visible && last_topmost.elapsed() >= TOPMOST_INTERVAL {
            window.raise_topmost();
            last_topmost = Instant::now();
        }

        timer.wait(FRAME_INTERVAL);
    }

    window.hide();
    if swapped {
        restore_system_cursors(marker);
    }
}

/// What the overlay window currently displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Content {
    Nothing,
    Pointer,
    Pulse,
}

#[derive(Clone, Copy)]
struct CursorState {
    x: i32,
    y: i32,
    showing: bool,
}

/// The cursor's physical position and whether the foreground app currently
/// shows it; `None` if it can't be read (e.g. on the secure desktop).
fn read_cursor() -> Option<CursorState> {
    let mut pt = POINT { x: 0, y: 0 };
    let mut info: CURSORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
    // Safety: valid out-pointers sized per the API contract.
    unsafe {
        if GetCursorPos(&mut pt) == 0 || GetCursorInfo(&mut info) == 0 {
            return None;
        }
    }
    Some(CursorState {
        x: pt.x,
        y: pt.y,
        showing: info.flags & CURSOR_SHOWING != 0,
    })
}

fn pump_messages() {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // Safety: standard non-blocking message pump on this thread's queue.
    unsafe {
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Sleeps for a short interval with sub-millisecond precision (the default
/// `Sleep` granularity of ~15 ms would make the pointer visibly trail the
/// cursor), waking early for window messages.
struct FrameTimer(HANDLE);

impl FrameTimer {
    fn new() -> Self {
        // Safety: null attributes/name are allowed; failure yields null.
        let handle = unsafe {
            CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        FrameTimer(handle)
    }

    fn wait(&self, interval: Duration) {
        if self.0.is_null() {
            std::thread::sleep(interval);
            return;
        }
        // Negative = relative, in 100 ns units.
        let due = -((interval.as_micros() as i64) * 10);
        // Safety: `self.0` is a live timer handle; no completion routine.
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, std::ptr::null(), 0) == 0 {
                std::thread::sleep(interval);
                return;
            }
            MsgWaitForMultipleObjects(1, &self.0, 0, interval.as_millis() as u32 + 20, QS_ALLINPUT);
        }
    }
}

impl Drop for FrameTimer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safety: we own the handle.
            unsafe { CloseHandle(self.0) };
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCHITTEST {
        // Belt and braces next to WS_EX_TRANSPARENT: never take a click.
        return HTTRANSPARENT as LRESULT;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// The per-pixel-alpha overlay window plus the 32-bit DIB its content is
/// uploaded from.
struct LayeredWindow {
    hwnd: HWND,
    mem_dc: HDC,
    dib: HBITMAP,
    old_object: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
    visible: bool,
    pos: (i32, i32),
}

impl LayeredWindow {
    fn create() -> Option<Self> {
        let class_name = encode_wide(WINDOW_CLASS);
        // Safety: the class/window structs are fully initialized and the
        // wide strings outlive the calls.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let mut class: WNDCLASSEXW = std::mem::zeroed();
            class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            class.lpfnWndProc = Some(window_proc);
            class.hInstance = instance;
            class.lpszClassName = class_name.as_ptr();
            // Fails harmlessly if already registered by an earlier thread.
            RegisterClassExW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                class_name.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return None;
            }
            let mem_dc = CreateCompatibleDC(std::ptr::null_mut());
            if mem_dc.is_null() {
                DestroyWindow(hwnd);
                return None;
            }
            Some(LayeredWindow {
                hwnd,
                mem_dc,
                dib: std::ptr::null_mut(),
                old_object: std::ptr::null_mut(),
                bits: std::ptr::null_mut(),
                width: 0,
                height: 0,
                visible: false,
                pos: (i32::MIN, i32::MIN),
            })
        }
    }

    /// (Re)creates the backing DIB when the content size changes.
    fn ensure_surface(&mut self, width: u32, height: u32) -> bool {
        if !self.dib.is_null() && self.width == width && self.height == height {
            return true;
        }
        // Safety: DC/bitmap handles are owned by `self`; the old bitmap is
        // deselected before being deleted.
        unsafe {
            if !self.dib.is_null() {
                SelectObject(self.mem_dc, self.old_object);
                DeleteObject(self.dib);
                self.dib = std::ptr::null_mut();
            }
            let (dib, bits) = create_dib(width, height);
            if dib.is_null() {
                return false;
            }
            self.old_object = SelectObject(self.mem_dc, dib);
            self.dib = dib;
            self.bits = bits;
            self.width = width;
            self.height = height;
        }
        true
    }

    /// Uploads `bitmap` as the window content and places it at `pos`.
    /// Returns whether the window now shows it.
    fn present(&mut self, bitmap: &Bitmap, pos: (i32, i32)) -> bool {
        if bitmap.width == 0 || bitmap.height == 0 || !self.ensure_surface(bitmap.width, bitmap.height) {
            return false;
        }
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let dst = POINT { x: pos.0, y: pos.1 };
        let size = SIZE { cx: bitmap.width as i32, cy: bitmap.height as i32 };
        let src = POINT { x: 0, y: 0 };
        // Safety: `bits` points at a `width * height * 4`-byte DIB owned by
        // `self` (just sized to `bitmap` by `ensure_surface`), the same
        // length as `bitmap.pixels`.
        let ok = unsafe {
            let dib = std::slice::from_raw_parts_mut(self.bits, bitmap.pixels.len());
            // RGBA -> BGRA straight into the DIB, no intermediate buffer.
            for (dst, src) in dib.as_chunks_mut::<4>().0.iter_mut().zip(bitmap.pixels.as_chunks::<4>().0) {
                *dst = [src[2], src[1], src[0], src[3]];
            }
            UpdateLayeredWindow(self.hwnd, std::ptr::null_mut(), &dst, &size, self.mem_dc, &src, 0, &blend, ULW_ALPHA)
                != 0
        };
        if ok {
            self.pos = pos;
        }
        ok
    }

    fn move_to(&mut self, pos: (i32, i32)) {
        if pos == self.pos {
            return;
        }
        // Safety: `hwnd` is owned by this thread.
        unsafe {
            SetWindowPos(
                self.hwnd,
                std::ptr::null_mut(),
                pos.0,
                pos.1,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.pos = pos;
    }

    fn show(&mut self) {
        if self.visible {
            return;
        }
        // Safety: `hwnd` is owned by this thread.
        unsafe {
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.visible = true;
        self.raise_topmost();
    }

    fn hide(&mut self) {
        if !self.visible {
            return;
        }
        // Safety: `hwnd` is owned by this thread.
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
        self.visible = false;
    }

    fn raise_topmost(&self) {
        // Safety: `hwnd` is owned by this thread.
        unsafe {
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

impl Drop for LayeredWindow {
    fn drop(&mut self) {
        // Safety: every handle here is owned by `self`.
        unsafe {
            if !self.dib.is_null() {
                SelectObject(self.mem_dc, self.old_object);
                DeleteObject(self.dib);
            }
            DeleteDC(self.mem_dc);
            DestroyWindow(self.hwnd);
        }
    }
}

/// A top-down 32-bit DIB section and a pointer to its pixels.
fn create_dib(width: u32, height: u32) -> (HBITMAP, *mut u8) {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        // Negative height = top-down rows, matching `Bitmap`'s layout.
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    // Safety: `info` is a valid header; `bits` receives the pixel pointer.
    let dib = unsafe {
        CreateDIBSection(
            std::ptr::null_mut(),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        )
    };
    (dib, bits as *mut u8)
}

/// Builds a Windows cursor from the pointer rendered on its tight canvas.
fn create_cursor(settings: &CursorSettings) -> Option<HCURSOR> {
    let (width, height, hx, hy) = cursor_style::cursor_canvas(settings);
    let bitmap = cursor_style::render_pointer(settings, width, height, hx, hy);
    let bgra = bitmap.to_straight_bgra();
    let (color, bits) = create_dib(width, height);
    if color.is_null() {
        return None;
    }
    // An all-zero AND mask: the color bitmap's alpha decides everything.
    // Monochrome rows are padded to 16-bit boundaries.
    let mask_stride = width.div_ceil(16) as usize * 2;
    let mask_bits = vec![0u8; mask_stride * height as usize];
    // Safety: `bits` is a `width * height * 4`-byte DIB; the mask buffer
    // matches CreateBitmap's padded-row layout; both bitmaps are deleted
    // after CreateIconIndirect copies them.
    unsafe {
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits, bgra.len());
        let mask = CreateBitmap(width as i32, height as i32, 1, 1, mask_bits.as_ptr().cast());
        let icon_info = ICONINFO {
            fIcon: 0,
            xHotspot: hx as u32,
            yHotspot: hy as u32,
            hbmMask: mask,
            hbmColor: color,
        };
        let cursor = CreateIconIndirect(&icon_info);
        DeleteObject(mask);
        DeleteObject(color);
        if cursor.is_null() { None } else { Some(cursor) }
    }
}

/// Whether the system cursors are currently swapped, for the panic hook.
static SWAP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Restores swapped system cursors if GameFrame panics: the release build
/// aborts on panic (`panic = "abort"`), so no `Drop` would run. The hook
/// still runs before the abort. A kill can't be caught; the marker file
/// covers that on the next start.
fn install_panic_restore_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if SWAP_ACTIVE.load(Ordering::SeqCst) {
                // Safety: SPI_SETCURSORS takes no parameters.
                unsafe {
                    SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0);
                }
            }
            previous(info);
        }));
    });
}

/// Replaces the arrow and hand system cursors with the game cursor. Returns
/// whether every swap succeeded (a partial swap is rolled back).
fn swap_system_cursors(settings: &CursorSettings, marker: &Path) -> bool {
    if let Some(dir) = marker.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    SWAP_ACTIVE.store(true, Ordering::SeqCst);
    let _ = std::fs::write(
        marker,
        b"GameFrame replaced the system cursors; restored on next start.\n",
    );
    for id in [OCR_NORMAL, OCR_HAND] {
        let Some(cursor) = create_cursor(settings) else {
            eprintln!("game cursor: failed to build the replacement cursor");
            restore_system_cursors(marker);
            return false;
        };
        // Safety: on success SetSystemCursor takes ownership of `cursor`; on
        // failure we still own it and destroy it.
        unsafe {
            if SetSystemCursor(cursor, id) == 0 {
                DestroyCursor(cursor);
                eprintln!("game cursor: SetSystemCursor failed");
                restore_system_cursors(marker);
                return false;
            }
        }
    }
    true
}

/// Reloads the user's cursor scheme from the registry, undoing
/// [`swap_system_cursors`].
fn restore_system_cursors(marker: &Path) {
    // Safety: SPI_SETCURSORS takes no parameters.
    unsafe {
        SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0);
    }
    SWAP_ACTIVE.store(false, Ordering::SeqCst);
    let _ = std::fs::remove_file(marker);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_store::TrackedGame;

    #[test]
    fn overlay_input_keeps_only_games_with_the_game_cursor_on() {
        let mut on = TrackedGame::new_default(PathBuf::from("C:\\Games\\on.exe"), "On".to_string());
        on.game_cursor = true;
        let mut off =
            TrackedGame::new_default(PathBuf::from("C:\\Games\\off.exe"), "Off".to_string());
        off.game_cursor = false;
        let config = Config {
            tracked_games: vec![on, off],
            ..Config::default()
        };

        let input = OverlayInput::from_config(&config);

        assert_eq!(input.games, vec![PathBuf::from("C:\\Games\\on.exe")]);
        assert!(foreground_has_game_cursor(
            &input.games,
            Path::new("C:\\Games\\on.exe")
        ));
        assert!(!foreground_has_game_cursor(
            &input.games,
            Path::new("C:\\Games\\off.exe")
        ));
    }

    #[test]
    fn overlay_input_sanitizes_the_settings() {
        let mut config = Config::default();
        config.cursor.size = 100_000;

        let input = OverlayInput::from_config(&config);

        assert_eq!(input.settings.size, *cursor_style::SIZE_RANGE.end());
    }
}
