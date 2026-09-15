//! First flash of a blank board, over the cable.
//!
//! Distinct from [`crate::update`], which pushes an app image into the
//! inactive slot of firmware that is already running. A board out of the box
//! has no partition table and nothing that speaks the scurry protocol, so
//! there is nothing to ask and nothing to ask it with: the whole layout goes
//! down over the ROM bootloader instead.
//!
//! What goes down is a single merged image at offset 0 — bootloader,
//! partition table, initial otadata and app. That erases NVS too, so a dongle
//! provisioned this way comes up with no bonds and no layout. For a blank
//! board that is what is wanted; for one already in service, use an update.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::Flasher;
use espflash::target::{Chip, ProgressCallbacks};

/// The merged image published with each release.
pub const FACTORY_ASSET: &str = "scurry-dongle-esp32c3-factory.bin";

/// Where the app lives in the layout, and so where the app descriptor sits in
/// a merged image.
const APP_OFFSET: usize = 0x20000;

/// Magic at the head of an `esp_app_desc_t`, little endian.
const APP_DESC_MAGIC: [u8; 4] = [0x32, 0x54, 0xcd, 0xab];

/// Serial timeout for the initial handshake; espflash raises it for erases.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// A port that might have an unprovisioned board on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub port: String,
    /// USB product string, when the OS gives one.
    pub product: Option<String>,
}

/// How long to wait for a port to prove it is already a dongle.
const PROBE_TIMEOUT: Duration = Duration::from_millis(600);

/// Ports with a board on them that is not already running scurry.
///
/// A blank board and a working dongle enumerate identically, so each port is
/// asked: one that answers a ping is a dongle and is left alone, one that does
/// not is a candidate. A port held open by the running app cannot be opened
/// here, which excludes the app's own dongle for the same reason.
pub fn candidates() -> Result<Vec<Candidate>> {
    let ports = serialport::available_ports().context("enumerating serial ports")?;
    let mut out = Vec::new();
    for p in ports {
        // macOS exposes each device twice; the tty side blocks on carrier
        // detect and never opens.
        if p.port_name.contains("/tty.")
            || !(p.port_name.contains("usbmodem") || p.port_name.contains("ttyACM"))
        {
            continue;
        }
        if speaks_scurry(&p.port_name) {
            continue;
        }
        let product = match &p.port_type {
            serialport::SerialPortType::UsbPort(info) => info.product.clone(),
            _ => None,
        };
        out.push(Candidate { port: p.port_name, product });
    }
    Ok(out)
}

/// True when the port answers a scurry ping. A port that cannot be opened is
/// in use by the running app, which means it is a dongle too.
fn speaks_scurry(port: &str) -> bool {
    let Ok(mut dongle) = crate::transport::Dongle::open(port) else {
        return true;
    };
    if dongle.send(scurry_proto::kind::PING, &[]).is_err() {
        return false;
    }
    matches!(
        dongle.recv(PROBE_TIMEOUT, &mut |_| {}),
        Ok(Some(m)) if m.kind == scurry_proto::kind::PONG
    )
}

/// Reject anything that is not a merged image before erasing a chip with it.
///
/// An app image on its own starts with the same 0xE9 as a bootloader, so the
/// first byte proves nothing. What distinguishes the two is the app
/// descriptor: in a merged image it sits at the app's offset, and in a bare
/// app image it sits at the front.
fn check_factory_image(image: &[u8]) -> Result<()> {
    if image.first() != Some(&0xE9) {
        bail!("that is not an ESP32 image (first byte is not 0xe9)");
    }
    if image.get(0x20..0x24) == Some(&APP_DESC_MAGIC) {
        bail!(
            "that is a bare app image, not a factory image. It has no bootloader or partition \
             table, so it cannot provision a blank board — use a firmware update instead, or \
             fetch {FACTORY_ASSET}"
        );
    }
    if image.get(APP_OFFSET + 0x20..APP_OFFSET + 0x24) != Some(&APP_DESC_MAGIC) {
        bail!("that image has no app at {APP_OFFSET:#x}; it is not a scurry factory image");
    }
    Ok(())
}

/// Adapts espflash's progress reporting to the (sent, total) pairs the rest of
/// the update path uses.
struct Progress<'a> {
    total: usize,
    report: &'a mut dyn FnMut(u32, u32),
}

impl ProgressCallbacks for Progress<'_> {
    fn init(&mut self, _addr: u32, total: usize) {
        self.total = total;
        (self.report)(0, total as u32);
    }
    fn update(&mut self, current: usize) {
        (self.report)(current as u32, self.total as u32);
    }
    fn verifying(&mut self) {}
    fn finish(&mut self, _skipped: bool) {
        (self.report)(self.total as u32, self.total as u32);
    }
}

/// Write a factory image to a board over its ROM bootloader.
///
/// The board is reset into the bootloader over USB rather than by holding
/// BOOT, which works on a C3 whose USB Serial/JTAG is intact — that is, on any
/// board that has not had its pins fused. One that will not enter the
/// bootloader on its own still needs BOOT held and RST tapped.
pub fn provision(
    port: &str,
    image: &[u8],
    progress: &mut dyn FnMut(u32, u32),
) -> Result<()> {
    check_factory_image(image)?;

    let serial = serialport::new(port, 115_200)
        .timeout(CONNECT_TIMEOUT)
        .open_native()
        .with_context(|| format!("opening {port}"))?;

    // espflash wants the USB descriptor to pick a reset strategy. An empty one
    // is honest when the OS does not give us the ids, and costs only the
    // USB-specific reset, which is retried anyway.
    let info = serialport::available_ports()
        .ok()
        .and_then(|ports| {
            ports.into_iter().find(|p| p.port_name == port).and_then(|p| match p.port_type {
                serialport::SerialPortType::UsbPort(info) => Some(info),
                _ => None,
            })
        })
        .unwrap_or(serialport::UsbPortInfo {
            vid: 0,
            pid: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
        });

    let connection = Connection::new(
        serial,
        info,
        ResetAfterOperation::HardReset,
        ResetBeforeOperation::DefaultReset,
        115_200,
    );

    let mut flasher = Flasher::connect(connection, true, true, false, Some(Chip::Esp32c3), None)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("connecting to the board on {port}"))?;

    let mut progress = Progress { total: image.len(), report: progress };
    flasher
        .write_bin_to_flash(0, image, &mut progress)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("writing the factory image")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_with_app_desc_at(offset: usize) -> Vec<u8> {
        let mut v = vec![0u8; offset + 0x24];
        v[0] = 0xE9;
        v[offset + 0x20..offset + 0x24].copy_from_slice(&APP_DESC_MAGIC);
        v
    }

    #[test]
    fn a_bare_app_image_is_refused() {
        let err = check_factory_image(&image_with_app_desc_at(0)).unwrap_err().to_string();
        assert!(err.contains("bare app image"), "{err}");
    }

    #[test]
    fn a_factory_image_is_accepted() {
        check_factory_image(&image_with_app_desc_at(APP_OFFSET)).unwrap();
    }

    #[test]
    fn anything_else_is_refused() {
        assert!(check_factory_image(b"<html>not a firmware</html>").is_err());
        assert!(check_factory_image(&[]).is_err());
        // 0xE9 alone is not enough: a bootloader with no app behind it would
        // flash and then boot into nothing.
        let mut truncated = vec![0u8; 0x40];
        truncated[0] = 0xE9;
        assert!(check_factory_image(&truncated).is_err());
    }
}
