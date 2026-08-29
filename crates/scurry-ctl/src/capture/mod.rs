//! Reading local input, per platform.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub use macos::{accessibility_trusted, install, run, CaptureHandle};

#[cfg(not(target_os = "macos"))]
pub fn run(
    _dongle: std::sync::Arc<std::sync::Mutex<crate::transport::Dongle>>,
    _state: std::sync::Arc<crate::ipc::DaemonState>,
) -> anyhow::Result<()> {
    anyhow::bail!("input capture is only implemented for macOS so far")
}
