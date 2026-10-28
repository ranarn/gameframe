//! UTF-16 string helper shared by every module that calls a wide (`*W`)
//! Win32 API. Kept as a dependency-free leaf so modules do not need to
//! import `win32` just for this.

/// Encodes a Rust string as a null-terminated UTF-16 buffer, the string
/// format every wide (`*W`) Win32 API used here expects.
pub(crate) fn encode_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
