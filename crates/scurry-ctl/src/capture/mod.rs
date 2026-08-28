//! Reading local input, per platform.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub use macos::run;

#[cfg(not(target_os = "macos"))]
pub fn run(
    _layout: crate::layout::Layout,
    _transport: crate::transport::SerialTransport,
) -> anyhow::Result<()> {
    anyhow::bail!("input capture is only implemented for macOS so far")
}
