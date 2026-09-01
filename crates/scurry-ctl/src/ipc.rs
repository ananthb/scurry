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
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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

/// One request waiting for its answer.
#[derive(Debug)]
struct Waiter {
    id: u64,
    /// The reply kind this request asked for.
    want: u8,
    /// Filled in by [`DaemonState::deliver`] once this waiter's answer arrives.
    reply: Option<(u8, Vec<u8>)>,
}

#[derive(Debug, Default)]
struct Waiters {
    /// Oldest first, which is also the order the dongle answers in: requests
    /// are serialised by the link lock on the way out.
    list: Vec<Waiter>,
    next_id: u64,
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
    /// Everyone currently blocked on an answer.
    waiters: Mutex<Waiters>,
    arrived: Condvar,
}

/// A registered request, removed from the waiter table when it is dropped.
///
/// Registration happens *before* the write, so a dongle that answers faster
/// than the requester can get back to the condvar still finds somebody to hand
/// the reply to.
struct Pending<'a> {
    state: &'a DaemonState,
    id: u64,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Ok(mut w) = self.state.waiters.lock() {
            w.list.retain(|x| x.id != self.id);
        }
    }
}

impl Pending<'_> {
    /// Block until this waiter's reply is filled in, or `timeout` elapses.
    fn wait(&self, timeout: Duration) -> Result<Option<(u8, Vec<u8>)>> {
        let waiting = |w: &mut Waiters| w.list.iter().any(|x| x.id == self.id && x.reply.is_none());
        let (mut waiters, _) = self
            .state
            .arrived
            .wait_timeout_while(self.state.table()?, timeout, waiting)
            .map_err(|_| anyhow::anyhow!("reply table poisoned"))?;
        // Deliberately not gated on `timed_out`: a reply that landed in the
        // same instant the wait expired is still ours, and dropping it would
        // bring back the starvation this table exists to prevent.
        let mine = waiters.list.iter_mut().find(|x| x.id == self.id);
        Ok(mine.and_then(|x| x.reply.take()))
    }
}

impl DaemonState {
    /// Called by the reader thread for anything that is not FOCUS.
    ///
    /// The wire carries no request id we could match on -- the header's `seq`
    /// is the sender's, and the dongle does not echo it -- so a reply is routed
    /// by the kind somebody asked for, oldest waiter first. An ACK matches any
    /// request, since it is how the dongle refuses anything, but it is offered
    /// to a waiter that expects one before it is treated as a refusal of the
    /// oldest outstanding request. A reply nobody is waiting for is dropped;
    /// that is the late answer to a request that already timed out.
    pub fn deliver(&self, kind: u8, payload: Vec<u8>) {
        let Ok(mut waiters) = self.waiters.lock() else { return };
        let mut target = waiters.list.iter().position(|w| w.want == kind && w.reply.is_none());
        if target.is_none() && kind == kind::ACK {
            // Nobody asked for an ACK, so this one refuses a request that
            // wanted something else -- the oldest still outstanding.
            target = waiters.list.iter().position(|w| w.reply.is_none());
        }
        if let Some(i) = target {
            waiters.list[i].reply = Some((kind, payload));
            self.arrived.notify_all();
        }
    }

    fn table(&self) -> Result<MutexGuard<'_, Waiters>> {
        self.waiters.lock().map_err(|_| anyhow::anyhow!("reply table poisoned"))
    }

    /// Register a waiter for `want` and return its handle.
    fn register(&self, want: u8) -> Result<Pending<'_>> {
        let mut waiters = self.table()?;
        let id = waiters.next_id;
        waiters.next_id += 1;
        waiters.list.push(Waiter { id, want, reply: None });
        Ok(Pending { state: self, id })
    }

    /// Send a request and wait for the reply that answers it.
    ///
    /// Three threads share this path -- the tray's status poller, the control
    /// socket, and the CLI behind it -- so the answer has to be routed, not
    /// merely waited for. Each request registers a waiter first and the reader
    /// hands each reply to the waiter that asked for it; see [`Self::deliver`].
    /// A single shared slot used to lose whichever reply arrived second, and
    /// both requesters then sat out their full timeout.
    pub fn request(
        &self,
        link: &Mutex<Dongle>,
        kind: u8,
        payload: &[u8],
        want: u8,
        timeout: Duration,
    ) -> Result<(u8, Vec<u8>)> {
        let pending = self.register(want)?;

        {
            // Held only for the write, so pointer motion never waits on a
            // config query.
            let mut guard = link.lock().map_err(|_| anyhow::anyhow!("link poisoned"))?;
            guard.send(kind, payload)?;
        }

        let Some(reply) = pending.wait(timeout)? else {
            anyhow::bail!("the dongle did not answer {} within {timeout:?}", kind_name(kind));
        };
        Ok(reply)
    }
}

/// A request kind in words, for messages a user reads.
fn kind_name(kind: u8) -> String {
    match kind {
        kind::GET_CONFIG => "a request for the layout".into(),
        kind::SET_CONFIG => "a layout write".into(),
        kind::GET_STATUS => "a status request".into(),
        kind::PING => "a ping".into(),
        k => format!("a {k:#04x} request"),
    }
}

/// Serve the control socket until the process exits.
///
/// A connection gets its own thread. Handling them inline meant one client
/// waiting out a three-second dongle timeout blocked every other client for
/// that long -- the settings window sat there while the tray's poller had the
/// listener. Traffic is a handful of short-lived connections every couple of
/// seconds, so a thread each costs nothing and needs no pool to manage.
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
        let dongle = Arc::clone(&dongle);
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            // On error the connection used to just close, so the client saw
            // EAGAIN reading a reply header that was never coming -- "Resource
            // temporarily unavailable", which says nothing about what went
            // wrong. Answer with a refusal instead, so the caller gets a
            // reason.
            if let Err(e) = handle(&stream, &dongle, &state) {
                eprintln!("control socket client: {e}");
                let mut s = &stream;
                // Alternate form, so the client is told the whole chain rather
                // than just the outermost "reading request payload".
                let _ = reply(&mut s, kind::ACK, &refusal(&format!("{e:#}")));
            }
        });
    }
    Ok(())
}

/// An ACK the daemon itself produced, with the reason spelled out after the
/// code.
///
/// The dongle's own ACKs are one byte, and a bare `BAD_REQUEST` is all the
/// settings window used to get whether the dongle had refused the request or
/// nothing had answered at all -- it could only report "unexpected reply 0x15".
/// Readers that only look at the code are unaffected; see [`ack_message`].
fn refusal(reason: &str) -> Vec<u8> {
    let mut out = vec![ack::BAD_REQUEST];
    let room = MAX_PAYLOAD - out.len();
    let cut = reason
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= room)
        .last()
        .unwrap_or(0);
    out.extend_from_slice(&reason.as_bytes()[..cut]);
    out
}

/// What an ACK payload means, in words.
pub fn ack_message(payload: &[u8]) -> String {
    match payload.split_first() {
        // A reason the daemon attached; see `refusal`.
        Some((_, rest)) if !rest.is_empty() => String::from_utf8_lossy(rest).into_owned(),
        Some((&code, _)) => ack_name(code).to_string(),
        None => ack_name(ack::BAD_REQUEST).to_string(),
    }
}

/// An [`ack`] code in words.
pub fn ack_name(code: u8) -> &'static str {
    match code {
        ack::OK => "ok",
        ack::BAD_REQUEST => "bad request",
        ack::INVALID_LAYOUT => "invalid layout",
        ack::STORAGE_FAILED => "storage failed",
        _ => "unknown error",
    }
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
                kind::GET_WIRELESS => kind::WIRELESS,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Long enough that a loaded machine cannot mistake scheduling for a lost
    /// reply; nothing waits this long when the code is right.
    const PATIENT: Duration = Duration::from_secs(5);
    /// Long enough to prove a waiter is genuinely not being answered.
    const BRIEF: Duration = Duration::from_millis(100);

    /// The bug: two requesters sharing one slot lost whichever reply arrived
    /// second, and both then sat out their full timeout.
    #[test]
    fn concurrent_requests_each_get_their_own_reply() {
        let state = DaemonState::default();
        let cfg = state.register(kind::CONFIG).unwrap();
        let status = state.register(kind::STATUS).unwrap();

        std::thread::scope(|s| {
            let a = s.spawn(move || cfg.wait(PATIENT).unwrap());
            let b = s.spawn(move || status.wait(PATIENT).unwrap());
            // Out of order on purpose: the second requester is answered first.
            state.deliver(kind::STATUS, vec![7]);
            state.deliver(kind::CONFIG, vec![9]);
            assert_eq!(a.join().unwrap(), Some((kind::CONFIG, vec![9])));
            assert_eq!(b.join().unwrap(), Some((kind::STATUS, vec![7])));
        });
    }

    #[test]
    fn an_ack_goes_to_the_request_that_expects_one() {
        let state = DaemonState::default();
        let cfg = state.register(kind::CONFIG).unwrap();
        let write = state.register(kind::ACK).unwrap();

        state.deliver(kind::ACK, vec![ack::INVALID_LAYOUT]);
        let refused = Some((kind::ACK, vec![ack::INVALID_LAYOUT]));
        assert_eq!(write.wait(PATIENT).unwrap(), refused);
        assert_eq!(cfg.wait(BRIEF).unwrap(), None, "the layout query is still outstanding");
    }

    #[test]
    fn an_unexpected_ack_refuses_the_oldest_request() {
        let state = DaemonState::default();
        let first = state.register(kind::CONFIG).unwrap();
        let second = state.register(kind::STATUS).unwrap();

        state.deliver(kind::ACK, vec![ack::BAD_REQUEST]);
        assert_eq!(first.wait(PATIENT).unwrap(), Some((kind::ACK, vec![ack::BAD_REQUEST])));
        assert_eq!(second.wait(BRIEF).unwrap(), None);
    }

    #[test]
    fn a_late_reply_is_dropped_rather_than_handed_to_the_next_requester() {
        let state = DaemonState::default();
        // A layout query that has already given up.
        drop(state.register(kind::CONFIG).unwrap());
        let table = state.waiters.lock().unwrap();
        assert!(table.list.is_empty(), "a finished request is deregistered");
        drop(table);

        let status = state.register(kind::STATUS).unwrap();
        state.deliver(kind::CONFIG, vec![9]);
        assert_eq!(status.wait(BRIEF).unwrap(), None, "the status request wants a status");
        state.deliver(kind::STATUS, vec![7]);
        assert_eq!(status.wait(PATIENT).unwrap(), Some((kind::STATUS, vec![7])));
    }

    #[test]
    fn a_daemon_refusal_carries_its_reason_past_readers_that_want_a_code() {
        let reason = "the dongle did not answer a status request within 3s";
        let body = refusal(reason);
        assert_eq!(body[0], ack::BAD_REQUEST);
        assert_eq!(ack_message(&body), reason);
        // The dongle's own one-byte ACKs still read as their code.
        assert_eq!(ack_message(&[ack::INVALID_LAYOUT]), "invalid layout");
        assert_eq!(ack_message(&[]), "bad request");
    }

    #[test]
    fn a_long_refusal_is_cut_to_fit_the_wire_without_splitting_a_character() {
        let body = refusal(&"é".repeat(MAX_PAYLOAD));
        assert!(body.len() <= MAX_PAYLOAD);
        assert!(std::str::from_utf8(&body[1..]).is_ok());
    }
}
