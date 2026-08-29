//! scurry-tray: menu bar status and settings for scurry.
//!
//! Three modes in one binary, so the packaged artifact is a single file:
//!   (default)              the tray icon
//!   --settings             the settings window, launched by the tray
//!   --print-desktop-entry  regenerate packaging/scurry-tray.desktop

mod daemon;
mod packaging;
mod settings;
mod status;
mod tray;

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--settings") => settings::run(),
        Some("--print-desktop-entry") => {
            print!("{}", packaging::desktop_entry());
            Ok(())
        }
        Some("--help" | "-h") => {
            println!(
                "scurry-tray

  (no arguments)         run the tray icon
  --settings             open the settings window
  --print-desktop-entry  print the XDG desktop entry

The tray talks to `scurry-ctl run` over a local socket. The daemon holds the
dongle's serial port exclusively, so nothing else can open it directly."
            );
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown argument: {other}");
            std::process::exit(2)
        }
        None => tray::run(),
    }
}
