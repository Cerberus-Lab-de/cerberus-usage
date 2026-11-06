//! Retry for file operations that fail for a moment on Windows because another process (antivirus,
//! indexer, backup) holds the file: raw os error 5 (access denied) or 32 (sharing violation).

use std::io;
use std::time::Duration;

/// Total attempts, including the first one.
const TRIES: u32 = 5;
/// Pause between attempts.
const PAUSE: Duration = Duration::from_millis(75);

/// True for the two errors that a short wait usually cures.
fn is_transient(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(5 | 32))
}

/// Runs `op` up to [`TRIES`] times, pausing [`PAUSE`] between attempts, while it fails with a
/// transient sharing error. Any other error, and the last transient one, is returned as is.
pub fn retry_transient<T>(op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_with(TRIES, |d| std::thread::sleep(d), op)
}

fn retry_with<T>(
    tries: u32,
    mut sleep: impl FnMut(Duration),
    mut op: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut attempt = 1;
    loop {
        match op() {
            Err(e) if is_transient(&e) && attempt < tries => {
                attempt += 1;
                sleep(PAUSE);
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    #[test]
    fn transient_error_is_retried_until_it_succeeds() {
        let mut calls = 0;
        let mut pauses = Vec::new();
        let r = retry_with(5, |d| pauses.push(d), || {
            calls += 1;
            if calls < 3 { Err(err(32)) } else { Ok(calls) }
        });
        assert_eq!(r.unwrap(), 3);
        assert_eq!(pauses, vec![PAUSE, PAUSE]);
        assert!(PAUSE >= Duration::from_millis(50) && PAUSE <= Duration::from_millis(100));
    }

    #[test]
    fn access_denied_is_retried_too_and_gives_up_after_the_last_try() {
        let mut calls = 0;
        let r: io::Result<()> = retry_with(5, |_| {}, || {
            calls += 1;
            Err(err(5))
        });
        assert_eq!(calls, 5);
        assert_eq!(r.unwrap_err().raw_os_error(), Some(5));
    }

    #[test]
    fn other_errors_are_not_retried() {
        let mut calls = 0;
        let mut slept = false;
        let r: io::Result<()> = retry_with(5, |_| slept = true, || {
            calls += 1;
            Err(err(2)) // file not found
        });
        assert_eq!(calls, 1);
        assert!(!slept);
        assert_eq!(r.unwrap_err().raw_os_error(), Some(2));
        let mut calls = 0;
        let r: io::Result<()> = retry_with(5, |_| {}, || {
            calls += 1;
            Err(io::Error::other("boom"))
        });
        assert_eq!(calls, 1);
        assert!(r.is_err());
    }

    #[test]
    fn success_on_the_first_try_does_not_sleep() {
        let mut slept = false;
        assert_eq!(retry_with(5, |_| slept = true, || Ok(7)).unwrap(), 7);
        assert!(!slept);
    }
}
