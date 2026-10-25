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
//! cursor positions and the window are in physical pixels on every monitor.
//! The application manifest already declares Per-Monitor V2 for the whole
//! process, so the call is redundant; it is kept so the thread stays correct
//! even if the manifest changes.
//!
//! Like YoloMouse, the game cursor exists only in and over the game: it is
//! shown while the window *under the cursor* (`WindowFromPoint`, which skips
//! our own click-through window) belongs to a cursor-enabled tracked game --
//! not merely while the game has focus, which would leave it drawn over
//! other windows and other monitors.
//!
//! Not exercised by automated tests (needs a desktop session), like the other
//! OS-facing modules; [`is_cursor_game`] is the pure part.

use crate::cursor_style::{self, Bitmap, CursorSettings, CursorShape, ShakeDetector};
use crate::profile_store::{self, Config};
use crate::win32::encode_wide;
use crate::window_filter;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SelectObject,
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
    CURSOR_SHOWING, CURSORINFO, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GA_ROOT, GetAncestor, GetCursorInfo, GetCursorPos, GetIconInfo, HCURSOR, HTTRANSPARENT,
    HWND_TOPMOST, ICONINFO, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SetWindowPos, ShowWindow, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow,
    WM_NCHITTEST, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP, WindowFromPoint,
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
/// How often to check whether the pointer has entered a game while the game
/// cursor is on but the pointer is elsewhere (~30 Hz: entering a game shows
/// the cursor within ~33 ms).
const SEARCH_INTERVAL: Duration = Duration::from_millis(33);
/// How often to re-assert `HWND_TOPMOST` while shown, in case the game made
/// its own window topmost after ours.
const TOPMOST_INTERVAL: Duration = Duration::from_millis(500);
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
fn is_cursor_game(games: &[PathBuf], exe: &Path) -> bool {
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

/// Handle to the running game-cursor thread. Dropping it stops the thread
/// and hides the window.
pub struct CursorOverlay {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl CursorOverlay {
    /// Starts the thread with the config currently on disk. The thread runs
    /// for the life of the app even with the game cursor off (it then only
    /// wakes every [`IDLE_INTERVAL`] to check), so turning it on is instant.
    pub fn start(config_dir: &Path) -> Self {
        let shared = Arc::new(Shared {
            input: Mutex::new(load_input(config_dir).unwrap_or_default()),
            generation: AtomicU64::new(0),
            pulse: AtomicBool::new(false),
            quit: AtomicBool::new(false),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("game-cursor".to_string())
            .spawn(move || run_overlay_thread(&thread_shared))
            .map_err(|e| log!("failed to start the game cursor thread: {e}"))
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
            log!("game cursor: failed to load config: {e}");
            None
        }
    }
}

fn run_overlay_thread(shared: &Shared) {
    // Safety: plain call; affects only this thread.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let Some(mut window) = LayeredWindow::create() else {
        log!("game cursor: failed to create the overlay window");
        return;
    };
    let timer = FrameTimer::new();
    let clock = Instant::now();

    let mut seen_generation = u64::MAX;
    let mut input = OverlayInput::default();
    // The pointer on its tight canvas, rendered lazily after each reload.
    let mut pointer: Option<Bitmap> = None;
    // The pulse's large canvas with the pointer drawn in.
    let mut pulse_base: Option<Bitmap> = None;
    // Reused pulse frame buffer.
    let mut frame = Bitmap::blank(0, 0, 0, 0);
    let mut last_pulse_render: Option<Instant> = None;
    // What the window currently holds (so a frame without new content only
    // needs a move).
    let mut content = Content::Nothing;
    // The last top-level window under the cursor and whether it is a
    // cursor-enabled game, so the exe lookup runs only when it changes.
    let mut under_cursor: Option<(HWND, bool)> = None;
    let mut shake = ShakeDetector::new();
    let mut pulse_start: Option<Instant> = None;
    let mut last_topmost = Instant::now();
    // The game's cursor handle last read (for `CursorShape::GameOwn`, the
    // one `pointer` was rendered from), and its image per handle (games
    // switch between a handful of cursors, e.g. on hover; `None` =
    // unreadable).
    let mut game_own_handle: HCURSOR = std::ptr::null_mut();
    let mut game_own_cache: HashMap<usize, Option<Bitmap>> = HashMap::new();

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
            under_cursor = None;
            game_own_handle = std::ptr::null_mut();
            game_own_cache.clear();
        }
        let settings = &input.settings;
        let game_own = settings.shape == CursorShape::GameOwn;

        let cursor = read_cursor();
        let over_game = match cursor {
            // Nothing to look up while the game cursor is off.
            Some(c) if settings.enabled => {
                let root = window_under(c.x, c.y);
                match under_cursor {
                    Some((hwnd, is_game)) if hwnd == root => is_game,
                    _ => match (!root.is_null())
                        .then(|| window_filter::exe_path_for_window(root))
                        .flatten()
                    {
                        Some(exe) => {
                            let is_game = is_cursor_game(&input.games, &exe);
                            under_cursor = Some((root, is_game));
                            is_game
                        }
                        // Not cached: a lookup that failed transiently is
                        // retried on the next tick.
                        None => false,
                    },
                }
            }
            _ => false,
        };
        let show =
            cursor.is_some_and(|c| cursor_style::should_show(settings, over_game, c.showing));
        if let Some(c) = cursor.filter(|c| show && c.handle != game_own_handle) {
            // The game switched cursors: read it (also for the Settings
            // preview, whatever the shape) and, for `GameOwn`, follow it.
            game_own_handle = c.handle;
            if game_own_cache.len() > 32 {
                game_own_cache.clear();
            }
            let image = game_own_cache
                .entry(c.handle as usize)
                .or_insert_with(|| read_cursor_image(c.handle));
            if let Some(image) = image {
                *LAST_GAME_CURSOR.lock().unwrap_or_else(|e| e.into_inner()) = Some(image.clone());
            }
            if game_own {
                pointer = image
                    .as_ref()
                    .map(|image| cursor_style::render_game_own(settings, image));
                pulse_base = None;
                content = Content::Nothing;
            }
        }
        // Nothing to build on while the game shows no (readable) cursor.
        let show = show && !(game_own && pointer.is_none());
        let Some(cursor) = cursor.filter(|_| show) else {
            window.hide();
            content = Content::Nothing;
            shake.reset();
            pulse_start = None;
            timer.wait(poll_interval(settings.enabled, over_game, false));
            continue;
        };

        if pulse_requested
            || (settings.shake_to_find && shake.feed(clock.elapsed(), cursor.x, cursor.y))
        {
            pulse_start = Some(Instant::now());
        }

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
                let base = pulse_base.get_or_insert_with(|| match &pointer {
                    Some(own) if game_own => cursor_style::pulse_canvas_for(settings, own),
                    _ => {
                        let (side, hot) = cursor_style::overlay_canvas(settings);
                        cursor_style::render_pointer(settings, side, side, hot, hot)
                    }
                });
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
            None => {
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
        }
        if window.visible && last_topmost.elapsed() >= TOPMOST_INTERVAL {
            window.raise_topmost();
            last_topmost = Instant::now();
        }

        timer.wait(poll_interval(
            settings.enabled,
            over_game,
            pulse_start.is_some(),
        ));
    }

    window.hide();
}

/// How long the overlay thread waits before its next tick. Full rate while
/// the pointer is over a cursor-enabled game (or a pulse is playing), a
/// slower search rate while the game cursor is on but the pointer is
/// elsewhere, and the idle rate while it is off.
fn poll_interval(enabled: bool, over_game: bool, pulsing: bool) -> Duration {
    if !enabled {
        IDLE_INTERVAL
    } else if over_game || pulsing {
        FRAME_INTERVAL
    } else {
        SEARCH_INTERVAL
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
    /// The cursor currently shown (null while hidden).
    handle: HCURSOR,
}

/// The cursor's physical position, whether the foreground app currently
/// shows it and which cursor; `None` if it can't be read (e.g. on the secure
/// desktop).
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
        handle: info.hCursor,
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

/// The top-level window under the screen point `(x, y)`, or null.
/// `WindowFromPoint` skips hidden, disabled and click-through
/// (`WS_EX_LAYERED | WS_EX_TRANSPARENT`) windows, so our own overlay never
/// shadows the game beneath it.
fn window_under(x: i32, y: i32) -> HWND {
    // Safety: plain queries; a null handle is passed through as null.
    unsafe {
        let hwnd = WindowFromPoint(POINT { x, y });
        if hwnd.is_null() {
            return hwnd;
        }
        GetAncestor(hwnd, GA_ROOT)
    }
}

/// The last cursor image read over a game, for the Settings preview of
/// [`CursorShape::GameOwn`].
static LAST_GAME_CURSOR: Mutex<Option<Bitmap>> = Mutex::new(None);

/// The last cursor image read over a game, if any yet.
pub fn last_game_cursor() -> Option<Bitmap> {
    LAST_GAME_CURSOR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Reads the image of the cursor `handle` -- typically a game's own cursor,
/// which belongs to the game process; cursor handles are session-wide, so
/// this works without touching the game. Handles both cursor kinds: 32-bit
/// color with alpha (or without, using the AND mask) and old monochrome
/// cursors (AND mask on top of XOR mask in one double-height bitmap).
fn read_cursor_image(handle: HCURSOR) -> Option<Bitmap> {
    let mut info: ICONINFO = unsafe { std::mem::zeroed() };
    // Safety: valid out-pointer; on success we own (and delete below) the
    // two bitmaps it returns.
    if unsafe { GetIconInfo(handle, &mut info) } == 0 {
        return None;
    }
    let image = (|| {
        let mask = read_bitmap_bgra(info.hbmMask)?;
        let (hot_x, hot_y) = (info.xHotspot as i32, info.yHotspot as i32);
        if info.hbmColor.is_null() {
            // Monochrome: the top half is the AND mask, the bottom the XOR.
            let (width, height) = (mask.0, mask.1 / 2);
            let half = (width * height * 4) as usize;
            let (and, xor) = mask.2.split_at(half);
            Some(cursor_style::cursor_image(
                width,
                height,
                hot_x,
                hot_y,
                xor,
                Some(and),
            ))
        } else {
            let (width, height, bgra) = read_bitmap_bgra(info.hbmColor)?;
            let and = (mask.0 == width && mask.1 >= height)
                .then(|| &mask.2[..(width * height * 4) as usize]);
            Some(cursor_style::cursor_image(
                width, height, hot_x, hot_y, &bgra, and,
            ))
        }
    })();
    // Safety: GetIconInfo handed us these bitmaps to delete.
    unsafe {
        DeleteObject(info.hbmMask as HGDIOBJ);
        if !info.hbmColor.is_null() {
            DeleteObject(info.hbmColor as HGDIOBJ);
        }
    }
    image
}

/// A bitmap's pixels as 32-bit top-down BGRA: `(width, height, bytes)`.
fn read_bitmap_bgra(bitmap: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    let mut bm: BITMAP = unsafe { std::mem::zeroed() };
    // Safety: valid out-pointer sized per the API contract.
    let size = std::mem::size_of::<BITMAP>() as i32;
    if unsafe { GetObjectW(bitmap as HGDIOBJ, size, (&raw mut bm).cast()) } == 0 {
        return None;
    }
    let (width, height) = (bm.bmWidth.max(0) as u32, bm.bmHeight.max(0) as u32);
    if width == 0 || height == 0 || width > 256 || height > 512 {
        return None;
    }
    let mut header: BITMAPINFO = unsafe { std::mem::zeroed() };
    header.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: -(height as i32), // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    // Safety: the screen DC is released below; `pixels` holds exactly
    // `height` rows of 32-bit pixels as `header` describes.
    let rows = unsafe {
        let dc = GetDC(std::ptr::null_mut());
        let rows = GetDIBits(
            dc,
            bitmap,
            0,
            height,
            pixels.as_mut_ptr().cast(),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(std::ptr::null_mut(), dc);
        rows
    };
    (rows == height as i32).then_some((width, height, pixels))
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
        if bitmap.width == 0
            || bitmap.height == 0
            || !self.ensure_surface(bitmap.width, bitmap.height)
        {
            return false;
        }
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let dst = POINT { x: pos.0, y: pos.1 };
        let size = SIZE {
            cx: bitmap.width as i32,
            cy: bitmap.height as i32,
        };
        let src = POINT { x: 0, y: 0 };
        // Safety: `bits` points at a `width * height * 4`-byte DIB owned by
        // `self` (just sized to `bitmap` by `ensure_surface`), the same
        // length as `bitmap.pixels`.
        let ok = unsafe {
            let dib = std::slice::from_raw_parts_mut(self.bits, bitmap.pixels.len());
            // RGBA -> BGRA straight into the DIB, no intermediate buffer.
            for (dst, src) in dib
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(bitmap.pixels.as_chunks::<4>().0)
            {
                *dst = [src[2], src[1], src[0], src[3]];
            }
            UpdateLayeredWindow(
                self.hwnd,
                std::ptr::null_mut(),
                &dst,
                &size,
                self.mem_dc,
                &src,
                0,
                &blend,
                ULW_ALPHA,
            ) != 0
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
        assert!(is_cursor_game(&input.games, Path::new("C:\\Games\\on.exe")));
        assert!(!is_cursor_game(
            &input.games,
            Path::new("C:\\Games\\off.exe")
        ));
    }

    #[test]
    fn poll_interval_is_fast_over_a_game_or_while_pulsing() {
        assert_eq!(poll_interval(true, true, false), FRAME_INTERVAL);
        assert_eq!(poll_interval(true, false, true), FRAME_INTERVAL);
        assert_eq!(poll_interval(true, true, true), FRAME_INTERVAL);
    }

    #[test]
    fn poll_interval_is_slow_while_searching_for_a_game() {
        assert_eq!(poll_interval(true, false, false), SEARCH_INTERVAL);
        assert!(SEARCH_INTERVAL > FRAME_INTERVAL);
        assert!(SEARCH_INTERVAL < IDLE_INTERVAL);
    }

    #[test]
    fn poll_interval_is_idle_when_disabled() {
        for over_game in [false, true] {
            for pulsing in [false, true] {
                assert_eq!(poll_interval(false, over_game, pulsing), IDLE_INTERVAL);
            }
        }
    }

    #[test]
    fn overlay_input_sanitizes_the_settings() {
        let mut config = Config::default();
        config.cursor.size = 100_000;

        let input = OverlayInput::from_config(&config);

        assert_eq!(input.settings.size, *cursor_style::SIZE_RANGE.end());
    }
}
