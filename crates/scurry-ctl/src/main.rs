//! scurry-ctl: captures local input and forwards it to the dongle.
//!
//! The dongle holds the layout and does the routing. Everything here is either
//! input capture or a thin client for the dongle's config API.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use scurry_ctl::config::Config;
use scurry_ctl::ipc::{ack_message, Client};
use scurry_ctl::transport::{Dongle, Message};
use scurry_proto::{ack, kind, wireless_op, SlotStatus, WirelessState};

fn usage() -> ! {
    eprintln!(
        "usage: scurry-ctl <command>

commands:
  run                 capture local input and forward it to the dongle
  probe               list serial ports and identify the dongle
  status              show the dongle's bonded targets
  get-config          print the layout stored on the dongle, as TOML
  set-config <file>   store a TOML layout on the dongle
  ping                check the link
  test-move           drive synthetic motion, printing everything the dongle says
  latency             measure round-trip time to the dongle
  wireless            show the state of the wireless control link
  pair [seconds]      open the pairing window so a controller can be authorised
  forget-controller   revoke the authorised wireless controller

options:
  --wireless          reach the dongle over BLE instead of the cable

All configuration lives on the dongle. Nothing is stored locally, so a layout
survives moving to another controller machine.

Commands reach the dongle through the running app when there is one, since it
holds the serial port exclusively, and open the port directly otherwise.

The wireless link is experimental. The cable always works, is the fallback, and
is the only way to authorise a controller -- `pair` is refused over the air.
Pressing the dongle's button three times opens the same window."
    );
    std::process::exit(2)
}

/// Set by `--wireless`: skip the cable and go over the air.
static WIRELESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn wireless_requested() -> bool {
    WIRELESS.load(std::sync::atomic::Ordering::Relaxed)
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--wireless") {
        args.remove(i);
        WIRELESS.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    match args.first().map(String::as_str).unwrap_or_else(|| usage()) {
        "run" => run(),
        "probe" => probe(),
        "status" => status(),
        "get-config" => get_config(),
        "set-config" => set_config(args.get(1).map(String::as_str).unwrap_or_else(|| usage())),
        "ping" => ping(),
        "test-move" => test_move(),
        "latency" => latency(),
        "wireless" => wireless(),
        "pair" => pair(args.get(1).map(String::as_str)),
        "forget-controller" => forget_controller(),
        _ => usage(),
    }
}

/// How the CLI reaches the dongle.
///
/// The app owns the serial port exclusively while it is running, so opening it
/// directly would fail with "Device or resource busy" -- which is the normal
/// state, not an error. Prefer the app's control socket and fall back to the
/// port only when nothing is holding it.
enum Link {
    Socket(Client),
    Serial(Dongle),
}

/// How long to scan before giving up on finding a dongle over the air.
const SCAN_FOR: Duration = Duration::from_secs(8);

fn open_link() -> Result<Link> {
    if wireless_requested() {
        eprintln!("scanning for a dongle over BLE...");
        let d = Dongle::open_wireless(SCAN_FOR)?;
        eprintln!("dongle: {}", d.describe());
        return Ok(Link::Serial(d));
    }
    if let Ok(client) = Client::connect() {
        eprintln!("via the running app");
        return Ok(Link::Socket(client));
    }
    // The cable first: it is faster to open by two orders of magnitude, and it
    // is the path that always works. Wireless is the fallback, not the default,
    // so a dongle sitting on a desk with a cable in it behaves as it always did.
    match Dongle::autodetect() {
        Ok(path) => {
            eprintln!("dongle: {path}");
            Ok(Link::Serial(Dongle::open(&path)?))
        }
        Err(cable) => {
            eprintln!("no cable ({cable}); scanning for a dongle over BLE...");
            let d = Dongle::open_wireless(SCAN_FOR)
                .map_err(|air| anyhow::anyhow!("{air}"))?;
            eprintln!("dongle: {}", d.describe());
            Ok(Link::Serial(d))
        }
    }
}

impl Link {
    /// Send a request and wait for a specific reply kind.
    fn request(&mut self, req: u8, payload: &[u8], want: u8) -> Result<Message> {
        match self {
            Link::Socket(c) => {
                let (kind, payload) = c.request(req, payload)?;
                if kind == want {
                    return Ok(Message { kind, payload });
                }
                // An ACK where one was not asked for is a refusal, and it is
                // also how the app reports that nothing answered in time. Both
                // read as a sentence rather than the "unexpected reply 0x15"
                // this used to print.
                if kind == kind::ACK {
                    bail!("{}", ack_message(&payload));
                }
                bail!("the app answered a {} with a {kind:#04x} message", name(req))
            }
            Link::Serial(d) => request(d, req, payload, want),
        }
    }
}

/// A request kind in words, for messages a user reads.
fn name(kind: u8) -> &'static str {
    match kind {
        kind::GET_CONFIG => "request for the layout",
        kind::SET_CONFIG => "layout write",
        kind::GET_STATUS => "status request",
        kind::PING => "ping",
        _ => "request",
    }
}

/// Send a request and wait for a specific reply kind.
///
/// The dongle's log output shares this pipe, so anything that is not a message
/// is printed rather than discarded -- it is the only view into the firmware
/// while the port is held open.
fn request(d: &mut Dongle, req: u8, payload: &[u8], want: u8) -> Result<Message> {
    d.send(req, payload)?;
    let mut on_log = |line: &str| eprintln!("[dongle] {line}");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if let Some(msg) = d.recv(Duration::from_millis(300), &mut on_log)? {
            if msg.kind == want {
                return Ok(msg);
            }
            if msg.kind == kind::ACK {
                bail!("the dongle refused the request: {}", ack_message(&msg.payload));
            }
        }
    }
    bail!("the dongle did not answer a {} within 3s", name(req))
}

fn run() -> Result<()> {
    use std::sync::{Arc, Mutex};

    // Headless capture owns the link itself; there is no app to defer to.
    let link = if wireless_requested() {
        eprintln!("scanning for a dongle over BLE...");
        Dongle::open_wireless(SCAN_FOR)?
    } else {
        let path = Dongle::autodetect()?;
        Dongle::open(&path)?
    };
    eprintln!("dongle: {}", link.describe());
    let dongle = Arc::new(Mutex::new(link));
    let state = Arc::new(scurry_ctl::ipc::DaemonState::default());

    // The control socket runs alongside capture so the tray can reach the
    // dongle without opening the serial port, which the daemon holds
    // exclusively.
    let socket_link = Arc::clone(&dongle);
    let socket_state = Arc::clone(&state);
    std::thread::spawn(move || {
        if let Err(e) = scurry_ctl::ipc::serve(socket_link, socket_state) {
            eprintln!("control socket: {e}");
        }
    });

    scurry_ctl::capture::run(dongle, state)
}

fn probe() -> Result<()> {
    let ports = serialport::available_ports().context("enumerating serial ports")?;
    if ports.is_empty() {
        println!("no serial ports found");
        return Ok(());
    }
    for p in &ports {
        let is_dongle = (p.port_name.contains("usbmodem") || p.port_name.contains("ttyACM"))
            && !p.port_name.contains("/tty.");
        println!("{}{}", p.port_name, if is_dongle { " <- dongle" } else { "" });
    }
    Ok(())
}

fn ping() -> Result<()> {
    open_link()?.request(kind::PING, &[], kind::PONG)?;
    println!("dongle responded");
    Ok(())
}

/// Drive the pointer with synthetic motion and print every reply.
///
/// Exists to exercise the dongle without an event tap, so the pointer path can
/// be tested without hijacking the real mouse -- and so the dongle's own log
/// output is visible while it happens.
fn test_move() -> Result<()> {
    use scurry_proto::MouseState;
    // Deliberately direct: synthetic motion is for testing the serial path
    // itself, so going through the app would defeat the point.
    let path = Dongle::autodetect()?;
    eprintln!("dongle: {path}");
    let mut d = Dongle::open(&path)?;
    let mut on_log = |line: &str| eprintln!("[dongle] {line}");

    eprintln!("sweeping right, then back left");
    for (label, dx) in [("right", 12i16), ("left", -12i16)] {
        eprintln!("--- {label} ---");
        for _ in 0..160 {
            let st = MouseState { buttons: 0, dx, dy: 0, wheel: 0, pan: 0 };
            d.send(kind::MOUSE, &st.encode())?;
            std::thread::sleep(Duration::from_millis(8));
            while let Some(m) = d.recv(Duration::from_millis(1), &mut on_log)? {
                match m.kind {
                    kind::FOCUS => eprintln!("FOCUS -> node {}", m.payload.first().copied().unwrap_or(255)),
                    k => eprintln!("message kind {k:#04x}, {} bytes", m.payload.len()),
                }
            }
        }
    }
    // Drain whatever is still in flight.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        if let Some(m) = d.recv(Duration::from_millis(100), &mut on_log)? {
            if m.kind == kind::FOCUS {
                eprintln!("FOCUS -> node {}", m.payload.first().copied().unwrap_or(255));
            }
        }
    }
    Ok(())
}

fn status() -> Result<()> {
    let msg = open_link()?.request(kind::GET_STATUS, &[], kind::STATUS)?;
    if msg.payload.is_empty() {
        bail!("empty status payload");
    }
    let count = msg.payload[0] as usize;
    // "connected", not "bonded": this is the dongle's live connection
    // table. Bonds persist in NVS across a reboot, connections do not, so a
    // freshly reset dongle shows nothing here until a host reconnects.
    println!("{:<6} {:<11} address", "node", "connected");
    for i in 0..count {
        let off = 1 + i * SlotStatus::WIRE_LEN;
        let Some(s) = msg.payload.get(off..).and_then(SlotStatus::decode) else {
            break;
        };
        let addr = if s.bda == [0u8; 6] {
            "-".to_string()
        } else {
            s.bda.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
        };
        // Node ids are slot + 1; node 0 is the controller's own screen.
        println!("{:<6} {:<11} {}", s.slot + 1, if s.connected { "yes" } else { "no" }, addr);
    }
    Ok(())
}

fn get_config() -> Result<()> {
    let msg = open_link()?.request(kind::GET_CONFIG, &[], kind::CONFIG)?;
    if msg.payload.first().copied().unwrap_or(0) == 0 {
        eprintln!("the dongle has no layout stored yet");
        return Ok(());
    }
    print!("{}", Config::from_payload(&msg.payload)?.to_toml()?);
    Ok(())
}

fn set_config(path: &str) -> Result<()> {
    let cfg = Config::load(path)?;
    // Validated here for a specific error message; the dongle validates again
    // before it commits anything to storage.
    let payload = cfg.to_payload()?;
    let msg = open_link()?.request(kind::SET_CONFIG, &payload, kind::ACK)?;
    if msg.payload.first().copied().unwrap_or(ack::BAD_REQUEST) != ack::OK {
        bail!("the layout was not stored: {}", ack_message(&msg.payload));
    }
    println!("stored {} screens on the dongle", cfg.screens.len());
    Ok(())
}

fn format_bda(bda: [u8; 6]) -> String {
    bda.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// Show whether a controller is authorised, and whether one is here now.
fn wireless() -> Result<()> {
    let msg = open_link()?.request(kind::GET_WIRELESS, &[], kind::WIRELESS)?;
    let w = WirelessState::decode(&msg.payload)
        .context("the dongle sent a wireless state it could not have meant")?;

    println!("controller:  {}", if w.pinned { format_bda(w.bda) } else { "none authorised".into() });
    println!("connected:   {}", if w.ready { "yes" } else { "no" });
    if w.window_secs > 0 {
        println!("pairing:     open, {}s left", w.window_secs);
    } else {
        println!("pairing:     closed");
    }
    if !w.pinned {
        println!();
        println!("Press the dongle's button three times, or run `scurry-ctl pair`,");
        println!("then start the controller with --wireless while the window is open.");
    }
    Ok(())
}

/// Open the pairing window.
///
/// Refused by the dongle when it arrives over the air, which is the point:
/// authorising a controller that can type on every machine here should take
/// physical access, and over the cable it does. The button is the other way in,
/// and the one that works when the dongle is nowhere near this machine.
fn pair(seconds: Option<&str>) -> Result<()> {
    let secs: u8 = match seconds {
        Some(s) => s.parse().context("pair takes a number of seconds")?,
        None => 60,
    };
    if secs == 0 {
        bail!("a zero-second window would close before anything could use it");
    }
    if wireless_requested() {
        bail!("the dongle refuses to change pairing over the wireless link. \n\
               Use the cable, or press the dongle's button three times.");
    }
    let msg = open_link()?.request(
        kind::SET_WIRELESS,
        &[wireless_op::PAIR, secs],
        kind::ACK,
    )?;
    let code = msg.payload.first().copied().unwrap_or(ack::BAD_REQUEST);
    if code != ack::OK {
        bail!("the dongle refused to open the window: {}", ack_message(&msg.payload));
    }
    println!("pairing window open for {secs}s.");
    println!("Now run: scurry-ctl --wireless ping");
    Ok(())
}

/// Revoke the authorised controller.
fn forget_controller() -> Result<()> {
    if wireless_requested() {
        bail!("this has to go over the cable, or a controller could keep itself authorised");
    }
    let msg = open_link()?.request(kind::SET_WIRELESS, &[wireless_op::FORGET], kind::ACK)?;
    let code = msg.payload.first().copied().unwrap_or(ack::BAD_REQUEST);
    if code != ack::OK {
        bail!("the dongle refused: {}", ack_message(&msg.payload));
    }
    println!("the wireless controller is no longer authorised");
    Ok(())
}

/// Measure round-trip time with pings.
///
/// The README calls latency the design's known weak point and says it is
/// unmeasured. Over the cable this is the cost of a USB round trip and is
/// nothing; over BLE it is bounded by the connection interval, which the host
/// chooses -- Apple's guidelines put the floor at 15ms in 15ms steps, so the
/// number here is mostly a readout of what macOS decided to negotiate.
///
/// This measures the control path, not the pointer path. A real report also
/// waits on the dongle's second radio hop out to the target, which nothing here
/// can see -- so treat this as the optimistic half of the answer.
fn latency() -> Result<()> {
    const ROUNDS: usize = 30;
    let mut link = open_link()?;

    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = std::time::Instant::now();
        link.request(kind::PING, &[], kind::PONG)?;
        samples.push(start.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    samples.sort();

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let total: f64 = samples.iter().map(|d| ms(*d)).sum();
    println!("{ROUNDS} round trips");
    println!("  min    {:.1} ms", ms(samples[0]));
    println!("  median {:.1} ms", ms(samples[ROUNDS / 2]));
    println!("  p90    {:.1} ms", ms(samples[ROUNDS * 9 / 10]));
    println!("  max    {:.1} ms", ms(samples[ROUNDS - 1]));
    println!("  mean   {:.1} ms", total / ROUNDS as f64);
    Ok(())
}
