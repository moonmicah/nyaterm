use std::io;

/// Stable, presentation-free error surface for hdc-backed operations.
///
/// Desktop maps every variant to a localized message and a stable code for
/// redacted logging (mirroring `AdbError`).
#[derive(Debug, thiserror::Error)]
pub enum HdcError {
    #[error("HarmonyOS file does not exist")]
    FileMissing,
    #[error("HDC I/O: {0}")]
    Io(#[from] io::Error),
    #[error("HDC executable is missing; install hdc or configure its path in settings")]
    ExecutableMissing,
    #[error("HarmonyOS device is offline or missing")]
    Offline,
    #[error("hdc rejected the request")]
    Rejected,
    #[error("Invalid hdc result: {0}")]
    Protocol(&'static str),
    #[error("HarmonyOS operation was cancelled")]
    Cancelled,
    #[error("HarmonyOS operation timed out")]
    Timeout,
    #[error("hdc command failed (exit {0})")]
    CommandFailed(i32),
}

pub type HdcResult<T> = Result<T, HdcError>;
