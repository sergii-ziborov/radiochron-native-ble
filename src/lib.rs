//! Blocking, Tokio-free native BLE advertisement scanning for `RadioChron`.
//!
//! Each supported operating system is implemented directly against its native
//! Bluetooth surface: `WinRT` on Windows, `BlueZ` over the system D-Bus on
//! Linux, and `CoreBluetooth` on macOS. The public API is synchronous so
//! callers do not inherit an async executor.

mod model;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

use std::fmt;
use std::time::Duration;

pub use model::{AdapterReport, ScanReport};
pub use radiochron::ble::{AddressType, Advertisement, ManufacturerData, ServiceData};

/// Cooperative controls observed while a scan is running.
pub trait ScanObserver {
    /// Returns true when the caller no longer needs the scan.
    fn is_cancelled(&self) -> bool {
        false
    }

    /// Reports elapsed and requested scan time.
    fn progress(&self, _elapsed: Duration, _total: Duration) {}
}

/// Observer for callers that do not need cancellation or progress.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

impl ScanObserver for NoopObserver {}

/// Native BLE backend error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Scans for BLE advertisements for at most `duration`.
pub fn scan(duration: Duration) -> Result<ScanReport, Error> {
    scan_with_observer(duration, &NoopObserver)
}

/// Scans for BLE advertisements with cooperative cancellation and progress.
pub fn scan_with_observer(
    duration: Duration,
    observer: &dyn ScanObserver,
) -> Result<ScanReport, Error> {
    platform_scan(duration, observer)
}

#[cfg(windows)]
fn platform_scan(duration: Duration, observer: &dyn ScanObserver) -> Result<ScanReport, Error> {
    windows::scan(duration, observer)
}

#[cfg(target_os = "linux")]
fn platform_scan(duration: Duration, observer: &dyn ScanObserver) -> Result<ScanReport, Error> {
    linux::scan(duration, observer)
}

#[cfg(target_os = "macos")]
fn platform_scan(duration: Duration, observer: &dyn ScanObserver) -> Result<ScanReport, Error> {
    macos::scan(duration, observer)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_scan(_duration: Duration, _observer: &dyn ScanObserver) -> Result<ScanReport, Error> {
    Err(Error::new(
        "native BLE scanning is supported on Windows, Linux, and macOS",
    ))
}
