//! A minimal blocking HTTPS GET on WinHTTP.
//!
//! WinHTTP ships with Windows, so it adds no crate (and no bundled root
//! certificate list or TLS stack) to the binary: the system certificate store
//! and proxy settings apply. The request is OS-facing and not exercised by the
//! automated tests.

use std::ffi::c_void;
use windows_sys::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
    WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};

/// GitHub rejects requests without a `User-Agent`.
const USER_AGENT: &str = concat!("GameFrame/", env!("CARGO_PKG_VERSION"));

/// Applied to name resolution, connect, send and receive each.
const TIMEOUT_MS: i32 = 10_000;

/// Larger responses are refused rather than buffered.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// A WinHTTP handle, closed on drop.
struct Handle(*mut c_void);

impl Handle {
    /// `None` when `raw` is the null handle a failed call returns.
    fn new(raw: *mut c_void) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn failed(what: &str) -> HttpError {
    HttpError::Failed(format!("{what} failed (Windows error {})", unsafe {
        windows_sys::Win32::Foundation::GetLastError()
    }))
}

/// Why a GET did not produce a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpError {
    /// The server answered with a status other than 200.
    Status(u32),
    /// Transport failure, timeout, oversized or undecodable body, with a
    /// short description.
    Failed(String),
}

/// GETs `https://{host}{path}` and returns the body as text.
pub(crate) fn get_text(host: &str, path: &str) -> Result<String, HttpError> {
    let agent = wide(USER_AGENT);
    let session = Handle::new(unsafe {
        WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    })
    .ok_or_else(|| failed("WinHttpOpen"))?;
    if unsafe { WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS) } == 0
    {
        return Err(failed("WinHttpSetTimeouts"));
    }

    let host = wide(host);
    let connection = Handle::new(unsafe {
        WinHttpConnect(session.0, host.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0)
    })
    .ok_or_else(|| failed("WinHttpConnect"))?;

    let verb = wide("GET");
    let path = wide(path);
    let accept_type = wide("application/vnd.github+json");
    let accept_types = [accept_type.as_ptr(), std::ptr::null()];
    let request = Handle::new(unsafe {
        WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            accept_types.as_ptr(),
            WINHTTP_FLAG_SECURE,
        )
    })
    .ok_or_else(|| failed("WinHttpOpenRequest"))?;

    if unsafe { WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) } == 0
    {
        return Err(failed("sending the request"));
    }
    if unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) } == 0 {
        return Err(failed("receiving the response"));
    }

    let mut status: u32 = 0;
    let mut status_len = std::mem::size_of::<u32>() as u32;
    if unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut status as *mut u32 as *mut c_void,
            &mut status_len,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(failed("reading the status code"));
    }
    if status != 200 {
        return Err(HttpError::Status(status));
    }

    let mut body = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let mut read: u32 = 0;
        if unsafe {
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                &mut read,
            )
        } == 0
        {
            return Err(failed("reading the response body"));
        }
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read as usize]);
        if body.len() > MAX_BODY_BYTES {
            return Err(HttpError::Failed("the response is too large".to_string()));
        }
    }
    String::from_utf8(body)
        .map_err(|_| HttpError::Failed("the response is not valid UTF-8".to_string()))
}
