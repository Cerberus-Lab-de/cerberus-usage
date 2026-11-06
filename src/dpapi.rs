//! Thin wrapper around Windows DPAPI (`CryptProtectData` / `CryptUnprotectData`).
//!
//! Current-user scope, no entropy, no UI. Errors carry no data. Decrypted bytes are returned in
//! `Zeroizing`, and the buffer Windows allocated is wiped before it is freed.

use std::fmt;
use std::ptr;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use zeroize::Zeroizing;

/// DPAPI failed (wrong user or machine, damaged blob, or an API error). Deliberately payload-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DpapiError;

impl fmt::Display for DpapiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DPAPI operation failed")
    }
}

impl std::error::Error for DpapiError {}

/// Copies and frees an output blob from DPAPI, wiping the OS buffer first.
///
/// # Safety
/// `blob` must come from a successful `CryptProtectData` / `CryptUnprotectData` call.
unsafe fn take_output(blob: CRYPT_INTEGER_BLOB) -> Zeroizing<Vec<u8>> {
    let len = blob.cbData as usize;
    let mut out = Zeroizing::new(Vec::with_capacity(len));
    if !blob.pbData.is_null() {
        // SAFETY: DPAPI guarantees `pbData` points at `cbData` readable and writable bytes.
        unsafe {
            out.extend_from_slice(std::slice::from_raw_parts(blob.pbData, len));
            ptr::write_bytes(blob.pbData, 0, len);
            LocalFree(blob.pbData as *mut core::ffi::c_void);
        }
    }
    out
}

fn input_blob(data: &[u8]) -> Result<CRYPT_INTEGER_BLOB, DpapiError> {
    let cb = u32::try_from(data.len()).map_err(|_| DpapiError)?;
    Ok(CRYPT_INTEGER_BLOB { cbData: cb, pbData: data.as_ptr() as *mut u8 })
}

/// Encrypts for the current Windows user.
pub fn protect(plain: &[u8]) -> Result<Vec<u8>, DpapiError> {
    let input = input_blob(plain)?;
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
    // SAFETY: `input` points at `plain` for the duration of the call; `output` is a valid out-parameter.
    let ok = unsafe {
        CryptProtectData(&input, ptr::null(), ptr::null(), ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
    };
    if ok == 0 {
        return Err(DpapiError);
    }
    // SAFETY: the call succeeded, so `output` was allocated by DPAPI.
    let bytes = unsafe { take_output(output) };
    Ok(bytes.to_vec())
}

/// Decrypts a blob made by [`protect`] for the same user.
pub fn unprotect(blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, DpapiError> {
    let input = input_blob(blob)?;
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
    // SAFETY: as in `protect`.
    let ok = unsafe {
        CryptUnprotectData(&input, ptr::null_mut(), ptr::null(), ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
    };
    if ok == 0 {
        return Err(DpapiError);
    }
    // SAFETY: the call succeeded, so `output` was allocated by DPAPI.
    Ok(unsafe { take_output(output) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip() {
        let plain = b"sk-ant-test-NICHTECHT-1234";
        let blob = protect(plain).unwrap();
        assert!(!blob.windows(plain.len()).any(|w| w == plain), "blob must not contain the plain text");
        assert_eq!(unprotect(&blob).unwrap().as_slice(), plain);
    }

    #[test]
    fn dpapi_rejects_garbage() {
        assert_eq!(unprotect(b"definitely not a dpapi blob").unwrap_err(), DpapiError);
        assert!(unprotect(&[]).is_err());
    }
}
