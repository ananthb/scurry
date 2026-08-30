//! Local control socket.
//!
//! The daemon holds the dongle's serial port exclusively, so nothing else can
//! open it — a tray or settings window that tried would simply fail with
//! "Device or resource busy". Instead the daemon exposes a Unix socket and
//! proxies requests through to the dongle, which keeps one owner of the port
//! and gives every other process a way in.
//!
//! The framing is the same [`scurry_proto`] header used on the wire, so a
//! request that is really destined for the dongle can be forwarded verbatim
//! rather than translated into a second protocol that could drift.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use scurry_proto::{ack, kind, Header, HEADER_LEN, MAGIC, MAX_PAYLOAD, VERSION};

use crate::transport::Dongle;

/// Daemon-local message kinds, above the dongle's range so a forwarded request
/// can never be confused with one the daemon answers itself.
pub mod local {
    /// Tray -> daemon: which node holds the pointer, and is the link up.
    pub const GET_DAEMON_STATUS: u8 = 0x40;
    /// Daemon -> tray: `[focus_node, link_ok]`.
    pub const DAEMON_STATUS: u8 = 0x41;
}

/// Where the socket lives.
///
/// `XDG_RUNTIME_DIR` when the session provides one (it is cleaned up on logout
/// and is not world-readable); otherwise a dotfile in `$HOME`, which is the
/// only portable fallback on macOS, where no such directory exists.
pub fn socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("scurry.sock");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".scurry.sock")
}

/// Shared state, and the rendezvous for replies from the dongle.
///
/// # Why replies come through here
///
/// Exactly one thread may read the serial port, and that is the capture reader
/// -- it has to be, since FOCUS arrives unsolicited. A caller that wanted a
/// reply could not simply read the port itself, and holding the link lock while
/// waiting for one would stall pointer motion for as long as the timeout. So
/// requests are written under a brief lock and the reader hands the answer back
/// here.
#[derive(Debug, Default)]
pub struct DaemonState {
    /// Node currently holding the pointer; 0 is this machine.
    pub focus: AtomicU8,
    /// The most recent reply that was not a FOCUS announcement.
    reply: Mutex<Option<(u8, Vec<u8>)>>,
    arrived: Condvar,
}

impl DaemonState {
    /// Called by the reader thread for anything that is not FOCUS.
    pub fn deliver(&self, kind: u8, payload: Vec<u8>) {
        if let Ok(mut slot) = self.reply.lock() {
            *slot = Some((kind, payload));
            self.arrived.notify_all();
        }
    }

    /// Send a request and wait for a reply of the kind it asked for.
    ///
    /// `want` is not decoration. There are two independent queriers -- the
    /// tray's status poller and the control socket -- and a reply that arrives
    /// after its own request timed out would otherwise be handed to whoever is
    /// waiting next. That is exactly what happened: a SET_CONFIG came back with
    /// 0x14 STATUS, the poller's late answer. Replies whose kind was not asked
    /// for are discarded rather than returned.
    ///
    /// ACK is always accepted, since it is how the dongle refuses anything.
    pub fn request(
        &self,
        link: &Mutex<Dongle>,
        kind: u8,
        payload: &[u8],
        want: u8,
        timeout: Duration,
    ) -> Result<(u8, Vec<u8>)> {
        let mut slot = self
            .reply
            .lock()
            .map_err(|_| anyhow::anyhow!("reply slot poisoned"))?;
        *slot = None;

        {
            // Held only for the write, so pointer motion never waits on a
            // config query.
            let mut guard = link.lock().map_err(|_| anyhow::anyhow!("link poisoned"))?;
            guard.send(kind, payload)?;
        }

        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("the dongle did not answer a {kind:#04x} within {timeout:?}");
            }
            let (mut got, timed_out) = self
                .arrived
                .wait_timeout_while(slot, remaining, |s| s.is_none())
                .map_err(|_| anyhow::anyhow!("reply slot poisoned"))?;
            if timed_out.timed_out() {
                anyhow::bail!("the dongle did not answer a {kind:#04x} within {timeout:?}");
            }
            match got.take() {
                Some((k, p)) if k == want || k == kind::ACK => return Ok((k, p)),
                // Somebody else's answer, arriving late. Keep waiting for ours.
                Some(_) => slot = got,
                None => slot = got,
            }
        }
    }
}

/// Serve the control socket until the process exits.
///
/// Each connection is handled inline: requests are rare, they are answered in
/// microseconds, and a thread per client would be more machinery than the
/// traffic justifies.
pub fn serve(dongle: Arc<Mutex<Dongle>>, state: Arc<DaemonState>) -> Result<()> {
    let path = socket_path();
    // A socket left behind by a crashed daemon would make bind fail with
    // "Address already in use" forever. Removing it first is safe because the
    // daemon is the only writer, and a live one still holds the port anyway.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding control socket at {}", path.display()))?;
    eprintln!("control socket: {}", path.display());

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // On error the connection used to just close, so the client saw EAGAIN
        // reading a reply header that was never coming -- "Resource temporarily
        // unavailable", which says nothing about what went wrong. Answer with a
        // refusal instead, so the caller gets a reason.
        if let Err(e) = handle(&stream, &dongle, &state) {
            eprintln!("control socket client: {e}");
            let mut s = &stream;
            let _ = reply(&mut s, kind::ACK, &[ack::BAD_REQUEST]);
        }
    }
    Ok(())
}

fn handle(stream: &UnixStream, dongle: &Arc<Mutex<Dongle>>, state: &DaemonState) -> Result<()> {
    let mut stream = stream;
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).context("reading request header")?;
    let h = Header::decode(&header).map_err(|e| anyhow::anyhow!("bad request header: {e:?}"))?;

    let mut payload = vec![0u8; h.len as usize];
    if h.len > 0 {
        stream.read_exact(&mut payload).context("reading request payload")?;
    }

    match h.kind {
        local::GET_DAEMON_STATUS => {
            let body = [state.focus.load(Ordering::Relaxed), 1];
            reply(&mut stream, local::DAEMON_STATUS, &body)
        }
        // Anything else is for the dongle. Forward it and relay the answer.
        _ => {
            let want = match h.kind {
                kind::GET_CONFIG => kind::CONFIG,
                kind::GET_STATUS => kind::STATUS,
                kind::PING => kind::PONG,
                _ => kind::ACK,
            };
            let (k, p) = state.request(dongle, h.kind, &payload, want, Duration::from_secs(3))?;
            reply(&mut stream, k, &p)
        }
    }
}

fn reply(stream: &mut &UnixStream, kind: u8, payload: &[u8]) -> Result<()> {
    let header = Header::encode(kind, 0, payload.len() as u16);
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

/// Client side: one request, one reply.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn connect() -> Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path).with_context(|| {
            format!("connecting to {} (is `scurry-ctl run` going?)", path.display())
        })?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        Ok(Self { stream })
    }

    pub fn request(&mut self, kind: u8, payload: &[u8]) -> Result<(u8, Vec<u8>)> {
        let header = Header::encode(kind, 0, payload.len() as u16);
        self.stream.write_all(&header)?;
        self.stream.write_all(payload)?;
        self.stream.flush()?;

        let mut head = [0u8; HEADER_LEN];
        self.stream.read_exact(&mut head).context("reading reply header")?;
        if head[0] != MAGIC || head[1] != VERSION {
            anyhow::bail!("reply is not a scurry message");
        }
        let len = u16::from_le_bytes([head[6], head[7]]) as usize;
        if len > MAX_PAYLOAD {
            anyhow::bail!("reply payload of {len} bytes is implausible");
        }
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).context("reading reply payload")?;
        Ok((head[2], payload))
    }
}
