//! scurry-ctl: captures local input and forwards it to the dongle.
//!
//! The dongle holds the layout and does the routing. Everything here is either
//! input capture or a thin client for the dongle's config API.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use scurry_ctl::config::Config;
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
survives moving to another controller machine."
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

fn open() -> Result<Dongle> {
    let path = Dongle::autodetect()?;
    eprintln!("dongle: {path}");
    Dongle::open(&path)
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
                let code = msg.payload.first().copied().unwrap_or(ack::BAD_REQUEST);
                bail!("dongle refused the request: {}", ack_name(code));
            }
        }
    }
    bail!("no reply from the dongle after 3s")
}

fn ack_name(code: u8) -> &'static str {
    match code {
        ack::OK => "ok",
        ack::BAD_REQUEST => "bad request",
        ack::INVALID_LAYOUT => "invalid layout",
        ack::STORAGE_FAILED => "storage failed",
        _ => "unknown error",
    }
}

fn run() -> Result<()> {
    scurry_ctl::capture::run(open()?)
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
    let mut d = open()?;
    request(&mut d, kind::PING, &[], kind::PONG)?;
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
    let mut d = open()?;
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
    let mut d = open()?;
    let msg = request(&mut d, kind::GET_STATUS, &[], kind::STATUS)?;
    if msg.payload.is_empty() {
        bail!("empty status payload");
    }
    let count = msg.payload[0] as usize;
    // "connected", not "bonded": this is the dongle's live connection
    // table. Bonds persist in NVS across a reboot, connections do not, so a
    // freshly reset dongle shows nothing here until a host reconnects.
    println!("{:<6} {:<11} {}", "node", "connected", "address");
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
    let mut d = open()?;
    let msg = request(&mut d, kind::GET_CONFIG, &[], kind::CONFIG)?;
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
    let mut d = open()?;
    let msg = request(&mut d, kind::SET_CONFIG, &payload, kind::ACK)?;
    let code = msg.payload.first().copied().unwrap_or(ack::BAD_REQUEST);
    if code != ack::OK {
        bail!("dongle rejected the layout: {}", ack_name(code));
    }
    println!("stored {} screens on the dongle", cfg.screens.len());
    Ok(())
}
