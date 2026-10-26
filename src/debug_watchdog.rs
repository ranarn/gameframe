//! [DEBUG-a4f2] throwaway: when the main (UI) thread stops pumping messages,
//! logs the last breadcrumb it passed and its native stack.
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{FALSE, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// The class of the hidden window `instance_channel` creates on the main thread.
const CHANNEL_CLASS: &str = "GameFrame.CommandChannel-5C28E192-0AB5-4FA5-8874-88144D010E3B";

/// Last breadcrumb: (name, when). `begin` before a blocking-capable step.
static CRUMB: Mutex<(&'static str, Option<Instant>)> = Mutex::new(("start", None));

/// Marks entry into a step on the calling thread.
pub fn step(name: &'static str) {
    if let Ok(mut c) = CRUMB.lock() {
        *c = (name, Some(Instant::now()));
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn start() {
    let tid = unsafe { GetCurrentThreadId() };
    std::thread::Builder::new()
        .name("debug-watchdog".into())
        .spawn(move || run(tid))
        .unwrap();
}

fn run(tid: u32) {
    let class = wide(CHANNEL_CLASS);
    let thread: HANDLE = unsafe {
        OpenThread(
            THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
            FALSE,
            tid,
        )
    };
    let process = unsafe { GetCurrentProcess() };
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_default();
    unsafe {
        SymSetOptions(SYMOPT_LOAD_LINES | SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS);
        let path = wide(&exe_dir);
        SymInitializeW(process, path.as_ptr(), 1);
    }
    log!("[DEBUG-a4f2] watchdog started for thread {tid}");
    let mut hung_since: Option<Instant> = None;
    let mut last_dump: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let hwnd = unsafe {
            FindWindowExW(HWND_MESSAGE, std::ptr::null_mut(), class.as_ptr(), std::ptr::null())
        };
        if hwnd.is_null() {
            continue;
        }
        let mut result = 0usize;
        // Answered only while the main thread pumps; 3 s without an answer = stall.
        let ok = unsafe { SendMessageTimeoutW(hwnd, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, 3000, &mut result) };
        if ok != 0 {
            if let Some(since) = hung_since.take() {
                log!("[DEBUG-a4f2] main thread pumping again after ~{:?}", since.elapsed());
            }
            last_dump = None;
            continue;
        }
        let since = *hung_since.get_or_insert_with(Instant::now);
        if last_dump.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            last_dump = Some(Instant::now());
            let crumb = CRUMB.try_lock().map(|c| *c).unwrap_or(("<locked>", None));
            log!(
                "[DEBUG-a4f2] main thread NOT pumping (>=3s, stalled for {:?}); last breadcrumb '{}' entered {:?} ago; stack:",
                since.elapsed(),
                crumb.0,
                crumb.1.map(|t| t.elapsed())
            );
            dump(process, thread);
        }
    }
}

fn dump(process: HANDLE, thread: HANDLE) {
    unsafe {
        // Allocate before suspending: the target may hold the heap lock.
        let mut frames: Vec<u64> = Vec::with_capacity(128);
        if SuspendThread(thread) == u32::MAX {
            log!("[DEBUG-a4f2] SuspendThread failed");
            return;
        }
        #[repr(C, align(16))]
        struct Ctx(CONTEXT);
        let mut ctx: Ctx = std::mem::zeroed();
        ctx.0.ContextFlags = CONTEXT_FULL_AMD64;
        if GetThreadContext(thread, &mut ctx.0) != 0 {
            let mut frame: STACKFRAME64 = std::mem::zeroed();
            frame.AddrPC.Offset = ctx.0.Rip;
            frame.AddrPC.Mode = AddrModeFlat;
            frame.AddrFrame.Offset = ctx.0.Rbp;
            frame.AddrFrame.Mode = AddrModeFlat;
            frame.AddrStack.Offset = ctx.0.Rsp;
            frame.AddrStack.Mode = AddrModeFlat;
            for _ in 0..100 {
                if StackWalk64(
                    0x8664u32,
                    process,
                    thread,
                    &mut frame,
                    &mut ctx.0 as *mut CONTEXT as *mut _,
                    None,
                    Some(SymFunctionTableAccess64),
                    Some(SymGetModuleBase64),
                    None,
                ) == 0
                    || frame.AddrPC.Offset == 0
                {
                    break;
                }
                frames.push(frame.AddrPC.Offset);
            }
        }
        ResumeThread(thread);
        for (i, addr) in frames.iter().enumerate() {
            #[repr(C)]
            struct Sym {
                info: SYMBOL_INFOW,
                name: [u16; 255],
            }
            let mut sym: Sym = std::mem::zeroed();
            sym.info.SizeOfStruct = std::mem::size_of::<SYMBOL_INFOW>() as u32;
            sym.info.MaxNameLen = 256;
            let mut disp = 0u64;
            let name = if SymFromAddrW(process, *addr, &mut disp, &mut sym.info) != 0 {
                let n = std::slice::from_raw_parts(sym.info.Name.as_ptr(), sym.info.NameLen as usize);
                String::from_utf16_lossy(n)
            } else {
                "?".to_string()
            };
            log!("[DEBUG-a4f2]   #{i} {addr:#x} {name}");
        }
    }
}
