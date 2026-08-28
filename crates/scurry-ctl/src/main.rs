//! scurry-ctl: captures local input and routes it across the virtual desktop.

use anyhow::{Context, Result};
use scurry_proto::{button, MouseState, Payload};
use scurry_ctl::transport::SerialTransport;

fn usage() -> ! {
    eprintln!(
        "usage: scurry-ctl <command>

commands:
  run [config]        capture local input and route it (default scurry.toml)
  probe               list serial ports and identify the dongle
  test-move [node]    send synthetic pointer motion to a node (default 1)
  test-click [node]   send a left click to a node (default 1)

`test-move` exists to prove the path end to end -- frame over USB CDC, through
the dongle, out as BLE HID -- without involving input capture or macOS
Accessibility permissions."
    );
    std::process::exit(2)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or_else(|| usage());

    // Parsed per command: `run` takes a config path in the same position that
    // the test commands take a node id, so parsing it eagerly would reject
    // `run scurry.toml` before doing anything.
    let node = || -> Result<u8> {
        Ok(args
            .get(1)
            .map(|s| s.parse())
            .transpose()
            .context("node must be a number 1..=4")?
            .unwrap_or(1))
    };

    match cmd {
        "run" => run(args.get(1).map(String::as_str).unwrap_or("scurry.toml")),
        "probe" => probe(),
        "test-move" => test_move(node()?),
        "test-click" => test_click(node()?),
        _ => usage(),
    }
}

fn run(config_path: &str) -> Result<()> {
    let cfg = scurry_ctl::config::Config::load(config_path)?;
    let device = cfg.device.clone();
    let layout = cfg.into_layout()?;

    let path = match device {
        Some(d) => d,
        None => SerialTransport::autodetect()?,
    };
    eprintln!("dongle: {path}");
    let transport = SerialTransport::open(&path)?;

    scurry_ctl::capture::run(layout, transport)
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
        let marker = if is_dongle { " <- dongle" } else { "" };
        println!("{}{}", p.port_name, marker);
    }
    Ok(())
}

fn open() -> Result<SerialTransport> {
    let path = SerialTransport::autodetect()?;
    eprintln!("dongle: {path}");
    SerialTransport::open(&path)
}

/// Walk the pointer around a square, slowly enough to watch.
fn test_move(node: u8) -> Result<()> {
    let mut t = open()?;
    eprintln!("moving pointer on node {node}; Ctrl-C to stop");

    // 40 steps of 10px a side. Deltas stay small so the motion is visible
    // rather than teleporting, and so we exercise the ordinary case rather
    // than the 16-bit extremes.
    let sides: [(i16, i16); 4] = [(10, 0), (0, 10), (-10, 0), (0, -10)];
    loop {
        for (dx, dy) in sides {
            for _ in 0..40 {
                t.send_to(
                    node,
                    Payload::Mouse(MouseState { buttons: 0, dx, dy, wheel: 0, pan: 0 }),
                )?;
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        }
    }
}

fn test_click(node: u8) -> Result<()> {
    let mut t = open()?;
    eprintln!("clicking on node {node}");
    t.send_to(
        node,
        Payload::Mouse(MouseState { buttons: button::LEFT, dx: 0, dy: 0, wheel: 0, pan: 0 }),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(60));
    // Buttons are absolute state, so releasing is just a report with none held.
    t.send_to(node, Payload::Mouse(MouseState::default()))?;
    Ok(())
}
