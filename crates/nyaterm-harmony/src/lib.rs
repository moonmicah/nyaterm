//! UI-independent HarmonyOS/OpenHarmony device backend.
//!
//! Owns the `hdc` (Harmony Device Connector) integration: executable discovery,
//! device discovery, bounded command execution, long-lived shell processes and
//! file transfer. Everything here is presentation-free; desktop owns lifecycle
//! coordination and error localization, mirroring the `nyaterm-android` split.
//!
//! Unlike the ADB backend, all device traffic goes through the user's `hdc`
//! executable as child processes: the hdc client/server protocol is unstable
//! and undocumented, so every operation delegates protocol details to the
//! official tool.

pub mod client;
pub mod error;

pub use client::{HdcClient, HdcDevice, resolve_executable};
pub use error::{HdcError, HdcResult};
