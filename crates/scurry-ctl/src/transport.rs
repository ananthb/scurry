//! Getting frames to the dongle.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use scurry_proto::{Frame, Payload, FRAME_LEN};

/// A sink for scurry frames.
pub trait Transport {
    fn send(&mut self, frame: &Frame) -> Result<()>;
}

/// Frames over the dongle's USB CDC link.
pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    seq: u16,
}

impl SerialTransport {
    /// Open the dongle by path, e.g. `/dev/cu.usbmodem2101`.
    ///
    /// The port is opened in raw mode by the serialport crate. That matters:
    /// under a cooked tty the line discipline mangles bytes that happen to look
    /// like control characters, and our frames carry arbitrary binary in the
    /// coordinate fields.
    pub fn open(path: &str) -> Result<Self> {
        let port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(50))
            .open()
            .with_context(|| format!("opening dongle at {path}"))?;
        Ok(Self { port, seq: 0 })
    }

    /// Guess the dongle's path. Espressif's USB Serial/JTAG shows up as a
    /// usbmodem on macOS and ttyACM on Linux.
    pub fn autodetect() -> Result<String> {
        let ports = serialport::available_ports().context("enumerating serial ports")?;
        let mut candidates: Vec<String> = ports
            .into_iter()
            .map(|p| p.port_name)
            .filter(|n| n.contains("usbmodem") || n.contains("ttyACM"))
            .collect();

        // macOS exposes the same device twice: /dev/cu.* and /dev/tty.*. The
        // tty side is the callout-blocking variant -- opening it waits for
        // carrier detect and hangs forever on a USB CDC device. Always prefer
        // cu.*, and never fall back to a tty.* that has a cu.* twin.
        candidates.sort_by_key(|n| !n.contains("/cu."));
        candidates
            .into_iter()
            .find(|n| !n.contains("/tty.")) // keep Linux ttyACM, drop macOS tty.*
            .context("no dongle found (looked for usbmodem/ttyACM)")
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    /// Build and send a frame addressed to `node`, allocating the sequence
    /// number so callers cannot accidentally reuse one.
    pub fn send_to(&mut self, node: u8, payload: Payload) -> Result<()> {
        let seq = self.next_seq();
        self.send(&Frame::new(node, seq, payload))
    }
}

impl Transport for SerialTransport {
    fn send(&mut self, frame: &Frame) -> Result<()> {
        let bytes = frame.encode();
        debug_assert_eq!(bytes.len(), FRAME_LEN);
        self.port.write_all(&bytes).context("writing frame")?;
        Ok(())
    }
}

/// Records frames instead of sending them. For tests and `--dry-run`.
#[derive(Default)]
pub struct NullTransport {
    pub sent: Vec<Frame>,
}

impl Transport for NullTransport {
    fn send(&mut self, frame: &Frame) -> Result<()> {
        self.sent.push(*frame);
        Ok(())
    }
}
