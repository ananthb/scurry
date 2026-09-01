//! The cable. USB CDC to the dongle's built-in USB Serial/JTAG.
//!
//! Always available, always the fallback, and the only path that can authorise
//! a wireless controller.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use scurry_proto::MAX_PAYLOAD;

use super::{encode, Framer, Message};

pub struct Serial {
    port: Box<dyn serialport::SerialPort>,
    path: String,
    seq: u16,
    framer: Framer,
}

impl Serial {
    /// Open by path, e.g. `/dev/cu.usbmodem2101`.
    ///
    /// Raw mode matters: under a cooked tty the line discipline rewrites bytes
    /// that look like control characters, and our payloads carry arbitrary
    /// binary in the coordinate fields.
    pub fn open(path: &str) -> Result<Self> {
        let port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(20))
            .open()
            .with_context(|| format!("opening dongle at {path}"))?;
        Ok(Self {
            port,
            path: path.to_string(),
            seq: 0,
            framer: Framer::with_capacity(4096),
        })
    }

    /// Espressif's USB Serial/JTAG appears as usbmodem on macOS, ttyACM on Linux.
    pub fn autodetect() -> Result<String> {
        let ports = serialport::available_ports().context("enumerating serial ports")?;
        let mut candidates: Vec<String> = ports
            .into_iter()
            .map(|p| p.port_name)
            .filter(|n| n.contains("usbmodem") || n.contains("ttyACM"))
            .collect();

        // macOS exposes the same device twice, /dev/cu.* and /dev/tty.*. Opening
        // the tty side waits for carrier detect and hangs forever on USB CDC,
        // so prefer cu.* and never fall back to a tty.* twin.
        candidates.sort_by_key(|n| !n.contains("/cu."));
        candidates
            .into_iter()
            .find(|n| !n.contains("/tty."))
            .context("no dongle found (looked for usbmodem/ttyACM)")
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            port: self.port.try_clone().context("cloning serial port")?,
            path: self.path.clone(),
            seq: 0,
            framer: Framer::with_capacity(4096),
        })
    }

    pub fn send(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_PAYLOAD {
            bail!("payload of {} bytes exceeds the dongle's buffer", payload.len());
        }
        let out = encode(kind, self.seq, payload);
        self.seq = self.seq.wrapping_add(1);
        // One write: a split header and payload can interleave with the log
        // text the dongle emits on the same pipe.
        self.port.write_all(&out).context("writing to dongle")?;
        Ok(())
    }

    /// Read until a message arrives or `timeout` elapses.
    ///
    /// The dongle's log output shares this pipe in the same direction, so bytes
    /// that are not a valid header are not garbage -- they are log text. They
    /// are handed to `on_log` rather than silently dropped, which is the only
    /// window into the firmware while the port is held open.
    pub fn recv(
        &mut self,
        timeout: Duration,
        on_log: &mut dyn FnMut(&str),
    ) -> Result<Option<Message>> {
        let deadline = Instant::now() + timeout;
        let mut scratch = [0u8; 1024];

        loop {
            if let Some(msg) = self.framer.take(on_log) {
                return Ok(Some(msg));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.port.read(&mut scratch) {
                Ok(0) => {}
                Ok(n) => self.framer.push(&scratch[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e).context("reading from dongle"),
            }
        }
    }
}
