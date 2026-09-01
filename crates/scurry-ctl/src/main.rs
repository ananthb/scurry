//! scurry-ctl: captures local input and forwards it to the dongle.
//!
//! The dongle holds the layout and does the routing. Everything here is either
//! input capture or a thin client for the dongle's config API.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use scurry_ctl::config::Config;
use scurry_ctl::ipc::{ack_message, Client};
use scurry_ctl::transport::{Dongle, Message};
use scurry_proto::{ack, kind, SlotStatus};

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

All configuration lives on the dongle. Nothing is stored locally, so a layout
survives moving to another controller machine.

Commands reach the dongle through the running app when there is one, since it
holds the serial port exclusively, and open the port directly otherwise."
    );
    std::process::exit(2)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or_else(|| usage()) {
        "run" => run(),
        "probe" => probe(),
        "status" => status(),
        "get-config" => get_config(),
        "set-config" => set_config(args.get(1).map(String::as_str).unwrap_or_else(|| usage())),
        "ping" => ping(),
        "test-move" => test_move(),
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

fn open_link() -> Result<Link> {
    if let Ok(client) = Client::connect() {
        eprintln!("via the running app");
        return Ok(Link::Socket(client));
    }
    let path = Dongle::autodetect()?;
    eprintln!("dongle: {path}");
    Ok(Link::Serial(Dongle::open(&path)?))
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

    // Headless capture owns the port itself; there is no app to defer to.
    let path = Dongle::autodetect()?;
    eprintln!("dongle: {path}");
    let dongle = Arc::new(Mutex::new(Dongle::open(&path)?));
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
