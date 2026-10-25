//! A minimal blocking HTTPS GET on WinHTTP: a small text fetch and a streamed
//! file download.
//!
//! WinHTTP ships with Windows, so it adds no crate (and no bundled root
//! certificate list or TLS stack) to the binary: the system certificate store
//! and proxy settings apply. The request is OS-facing and not exercised by the
//! automated tests.

use std::ffi::c_void;
use std::io::Write;
use windows_sys::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption,
    WinHttpSetTimeouts,
};

/// GitHub rejects requests without a `User-Agent`.
const USER_AGENT: &str = concat!("GameFrame/", env!("CARGO_PKG_VERSION"));

/// Applied to name resolution, connect, send and receive each.
const TIMEOUT_MS: i32 = 10_000;

/// Larger text responses are refused rather than buffered.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Redirects a download follows before giving up.
const MAX_REDIRECTS: usize = 5;

/// Room for a Location header, in UTF-16 units (a signed release-asset URL is
/// about 1 000 long).
const LOCATION_CAPACITY: usize = 4096;

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

/// How a request that got a response ended.
enum Reply {
    /// Status 200; the body went to the sink.
    Body,
    /// A 3xx status with this `Location`, which was not followed.
    Redirect(String),
}

type Sink<'a> = &'a mut dyn FnMut(&[u8]) -> Result<(), HttpError>;

/// One GET of `https://{host}{path}`, with the body handed to `sink` chunk by
/// chunk. With `follow_redirects` off, a redirect is reported instead of
/// followed, so the caller can vet where it leads.
fn request(
    host: &str,
    path: &str,
    accept: &str,
    follow_redirects: bool,
    sink: Sink,
) -> Result<Reply, HttpError> {
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
    let accept_type = wide(accept);
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

    if !follow_redirects {
        let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        if unsafe {
            WinHttpSetOption(
                request.0,
                WINHTTP_OPTION_REDIRECT_POLICY,
                &policy as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            )
        } == 0
        {
            return Err(failed("disabling redirects"));
        }
    }

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
    if !follow_redirects && matches!(status, 301 | 302 | 303 | 307 | 308) {
        let mut location = vec![0u16; LOCATION_CAPACITY];
        let mut location_len = (location.len() * 2) as u32;
        if unsafe {
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_LOCATION,
                std::ptr::null(),
                location.as_mut_ptr() as *mut c_void,
                &mut location_len,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(failed("reading the redirect location"));
        }
        let location = String::from_utf16(&location[..location_len as usize / 2])
            .map_err(|_| HttpError::Failed("the redirect location is not valid text".into()))?;
        return Ok(Reply::Redirect(location));
    }
    if status != 200 {
        return Err(HttpError::Status(status));
    }

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
            return Ok(Reply::Body);
        }
        sink(&chunk[..read as usize])?;
    }
}

/// GETs `https://{host}{path}` and returns the body as text.
pub(crate) fn get_text(host: &str, path: &str) -> Result<String, HttpError> {
    let mut body = Vec::new();
    request(
        host,
        path,
        "application/vnd.github+json",
        true,
        &mut |chunk| {
            body.extend_from_slice(chunk);
            if body.len() > MAX_BODY_BYTES {
                return Err(HttpError::Failed("the response is too large".to_string()));
            }
            Ok(())
        },
    )?;
    String::from_utf8(body)
        .map_err(|_| HttpError::Failed("the response is not valid UTF-8".to_string()))
}

/// Splits an `https://` URL into its host and its path with query (`/` for
/// none; a fragment is dropped). The host is everything up to the first `/`,
/// `?` or `#`, so userinfo or a port stays in it, and a caller that compares it
/// with a list of trusted hosts rejects them.
pub(crate) fn split_https_url(url: &str) -> Option<(&str, String)> {
    let rest = url.strip_prefix("https://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(end);
    if host.is_empty() {
        return None;
    }
    let tail = tail.split('#').next().unwrap_or_default();
    let path = if tail.starts_with('/') {
        tail.to_string()
    } else {
        format!("/{tail}")
    };
    Some((host, path))
}

/// The absolute URL a `Location` header leads to from a response by `host`: an
/// absolute-path reference (`/a/b`) stays on `host`; anything else is returned
/// as is, for the caller to vet.
fn resolve_location(host: &str, location: &str) -> String {
    if location.starts_with('/') && !location.starts_with("//") {
        format!("https://{host}{location}")
    } else {
        location.to_string()
    }
}

/// GETs `url` and streams the body to `dest`, refusing more than `max_bytes`.
/// Redirects are followed by hand, at most [`MAX_REDIRECTS`], and `url` and
/// every redirect target must be an HTTPS URL on one of `trusted_hosts`. A
/// redirect to a path on the same host is resolved against it.
pub(crate) fn download(
    url: &str,
    trusted_hosts: &[&str],
    dest: &mut impl Write,
    max_bytes: u64,
) -> Result<(), HttpError> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let (host, path) = split_https_url(&url)
            .filter(|(host, _)| trusted_hosts.contains(host))
            .ok_or_else(|| {
                HttpError::Failed("the download leads to an untrusted address".to_string())
            })?;
        let mut written: u64 = 0;
        let reply = request(
            host,
            &path,
            "application/octet-stream",
            false,
            &mut |chunk| {
                written += chunk.len() as u64;
                if written > max_bytes {
                    return Err(HttpError::Failed("the download is too large".to_string()));
                }
                dest.write_all(chunk)
                    .map_err(|e| HttpError::Failed(format!("writing the file failed: {e}")))
            },
        )?;
        match reply {
            Reply::Body => return Ok(()),
            Reply::Redirect(location) => url = resolve_location(host, &location),
        }
    }
    Err(HttpError::Failed("too many redirects".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_https_url_separates_host_and_path_with_query() {
        assert_eq!(
            split_https_url("https://github.com/a/b.exe?x=1#frag"),
            Some(("github.com", "/a/b.exe?x=1".to_string()))
        );
        assert_eq!(
            split_https_url("https://github.com"),
            Some(("github.com", "/".to_string()))
        );
        assert_eq!(
            split_https_url("https://github.com?x=1"),
            Some(("github.com", "/?x=1".to_string()))
        );
    }

    #[test]
    fn split_https_url_keeps_userinfo_and_port_in_the_host() {
        assert_eq!(
            split_https_url("https://github.com:8443/a").unwrap().0,
            "github.com:8443"
        );
        assert_eq!(
            split_https_url("https://github.com@evil.test/a").unwrap().0,
            "github.com@evil.test"
        );
    }

    #[test]
    fn resolve_location_keeps_a_path_reference_on_the_same_host() {
        assert_eq!(
            resolve_location("github.com", "/a/b.exe?x=1"),
            "https://github.com/a/b.exe?x=1"
        );
    }

    #[test]
    fn resolve_location_leaves_other_references_for_the_caller_to_vet() {
        for location in [
            "https://release-assets.githubusercontent.com/a",
            "//evil.test/a",
            "http://github.com/a",
            "a/b",
            "",
        ] {
            assert_eq!(resolve_location("github.com", location), location);
        }
    }

    #[test]
    fn split_https_url_rejects_other_schemes_and_empty_hosts() {
        for url in [
            "http://github.com/a",
            "ftp://github.com/a",
            "https://",
            "https:///a",
            "github.com/a",
            "",
        ] {
            assert_eq!(split_https_url(url), None, "{url:?}");
        }
    }
}
