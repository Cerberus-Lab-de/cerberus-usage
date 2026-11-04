//! Single-instance guard via a named mutex.
//!
//! The mutex handle is intentionally never closed: it must live as long as the process,
//! and Windows releases it on exit.

use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};

/// Mutex name of the real app (per logon session).
pub const APP_MUTEX_NAME: &str = "Local\\CerberusUsage.SingleInstance";

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Returns `true` if this call created the mutex (first instance), `false` if it already existed.
/// If the mutex cannot be created at all, the app still starts (returns `true`).
pub fn acquire(name: &str) -> bool {
    let name = wide(name);
    // SAFETY: `name` is a valid NUL-terminated UTF-16 string; no security attributes needed.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return true;
    }
    // SAFETY: reads the thread's last error right after the call above.
    unsafe { GetLastError() != ERROR_ALREADY_EXISTS }
}

/// Acquire the real app mutex.
pub fn acquire_app() -> bool {
    acquire(APP_MUTEX_NAME)
}

/// Message box for the second instance.
pub fn show_already_running_notice() {
    let text = wide("Cerberus Usage läuft bereits.");
    let title = wide("Cerberus Usage");
    // SAFETY: both strings are valid NUL-terminated UTF-16; no owner window.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_is_detected() {
        let name = format!("Local\\CerberusUsage.Test.{}", std::process::id());
        assert!(acquire(&name));
        assert!(!acquire(&name));
    }
}
