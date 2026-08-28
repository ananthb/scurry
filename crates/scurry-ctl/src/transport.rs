//! The link to the dongle.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use scurry_proto::{Header, HEADER_LEN, MAGIC, MAX_PAYLOAD, VERSION};

/// One decoded message from the dongle.
#[derive(Debug, Clone)]
pub struct Message {
    pub kind: u8,
    pub payload: Vec<u8>,
}

/// The dongle's USB CDC link.
pub struct Dongle {
    port: Box<dyn serialport::SerialPort>,
    seq: u16,
    /// Bytes read but not yet consumed, so a partial message survives a read.
    buf: Vec<u8>,
    /// Log text seen so far without a terminating newline. The dongle's output
    /// arrives in whatever chunks the USB reads land in, so a line routinely
    /// straddles two reads and would otherwise be printed in fragments.
    log_partial: String,
}

impl Dongle {
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
        Ok(Self { port, seq: 0, buf: Vec::with_capacity(4096), log_partial: String::new() })
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

    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            port: self.port.try_clone().context("cloning serial port")?,
            seq: 0,
            buf: Vec::with_capacity(4096),
            log_partial: String::new(),
        })
    }

    pub fn send(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_PAYLOAD {
            bail!("payload of {} bytes exceeds the dongle's buffer", payload.len());
        }
        let header = Header::encode(kind, self.seq, payload.len() as u16);
        self.seq = self.seq.wrapping_add(1);

        // One write: a split header and payload can interleave with the log
        // text the dongle emits on the same pipe.
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(payload);
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
            if let Some(msg) = self.take_message(on_log) {
                return Ok(Some(msg));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.port.read(&mut scratch) {
                Ok(0) => {}
                Ok(n) => self.buf.extend_from_slice(&scratch[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e).context("reading from dongle"),
            }
        }
    }

    fn take_message(&mut self, on_log: &mut dyn FnMut(&str)) -> Option<Message> {
        let mut skipped = 0usize;
        loop {
            if self.buf.len() - skipped < HEADER_LEN {
                break;
            }
            let head = &self.buf[skipped..];
            if head[0] != MAGIC || head[1] != VERSION {
                skipped += 1;
                continue;
            }
            match Header::decode(head) {
                Ok(h) => {
                    let total = HEADER_LEN + h.len as usize;
                    if self.buf.len() - skipped < total {
                        break; // payload still arriving
                    }
                    let start = skipped + HEADER_LEN;
                    let payload = self.buf[start..start + h.len as usize].to_vec();
                    self.flush_log(skipped, on_log);
                    // Drain the skipped log bytes AND the message. Draining
                    // only `total` left the skipped prefix in place and ate an
                    // equal number of bytes off the message instead, so the
                    // stream stayed misaligned from the first message that
                    // happened to follow log output -- which is most of them.
                    self.buf.drain(..skipped + total);
                    return Some(Message { kind: h.kind, payload });
                }
                Err(_) => {
                    skipped += 1;
                }
            }
        }
        // Keep the last few bytes: a header may be split across reads.
        if skipped > 0 {
            self.flush_log(skipped, on_log);
            self.buf.drain(..skipped);
        }
        None
    }

    fn flush_log(&mut self, upto: usize, on_log: &mut dyn FnMut(&str)) {
        if upto == 0 {
            return;
        }
        self.log_partial
            .push_str(&String::from_utf8_lossy(&self.buf[..upto]));

        // Emit only complete lines; hold anything after the last newline until
        // the rest of it arrives.
        while let Some(nl) = self.log_partial.find('\n') {
            let line: String = self.log_partial.drain(..=nl).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if !line.is_empty() {
                on_log(line);
            }
        }
    }
}
