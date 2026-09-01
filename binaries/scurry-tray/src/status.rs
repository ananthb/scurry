//! What the menu shows, and how the settings process asks for the same thing.
//!
//! The tray owns the dongle, so it queries directly. The settings window is a
//! separate process and goes through the control socket instead.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use scurry_ctl::config::Config;
use scurry_ctl::ipc::{ack_message, Client, DaemonState};
use scurry_ctl::transport::Dongle;
use scurry_proto::{kind, SlotStatus};

/// How long one poll waits for the dongle before giving up on that round.
const POLL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenInfo {
    pub name: String,
    pub node: u8,
    pub width: i32,
    pub height: i32,
}

/// The three states worth distinguishing in the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    /// Nothing plugged in. An ordinary state, not a failure.
    NoDongle,
    #[cfg(target_os = "macos")]
    /// The dongle is there but macOS has not granted Accessibility, so input
    /// cannot be captured yet.
    NeedsPermission,
    Ready {
        focus: u8,
        screens: Vec<ScreenInfo>,
        slots: Vec<SlotStatus>,
    },
}

impl Snapshot {
    pub fn tooltip(&self) -> String {
        match self {
            Snapshot::NoDongle => "scurry — no dongle".into(),
            #[cfg(target_os = "macos")]
            Snapshot::NeedsPermission => "scurry — needs Accessibility access".into(),
            Snapshot::Ready { focus, screens, .. } => {
                if *focus == 0 {
                    "scurry — pointer here".into()
                } else {
                    let name = screens
                        .iter()
                        .find(|s| s.node == *focus)
                        .map(|s| s.name.as_str())
                        .unwrap_or("another machine");
                    format!("scurry — pointer on {name}")
                }
            }
        }
    }
}

/// Poll the dongle in the background and publish the result.
///
/// The menu is rebuilt on the UI thread, and a dongle query can take up to a
/// few seconds. Doing it inline would freeze the menu for that long, so the UI
/// only ever reads the most recent cached answer.
pub fn spawn_poller(
    link: Arc<Mutex<Dongle>>,
    state: Arc<DaemonState>,
    out: Arc<Mutex<Snapshot>>,
) {
    std::thread::spawn(move || {
        // Carried across polls. A poll that goes unanswered is not news that
        // the layout changed, and publishing an empty list for it emptied the
        // menu until the next round -- the screens flickered in and out. Only
        // a real answer replaces what is shown.
        let mut screens: Vec<ScreenInfo> = Vec::new();
        let mut slots: Vec<SlotStatus> = Vec::new();

        loop {
            let focus = state.focus.load(Ordering::Relaxed);

            let cfg = state.request(&link, kind::GET_CONFIG, &[], kind::CONFIG, POLL_TIMEOUT);
            if let Ok((kind::CONFIG, p)) = cfg {
                if let Ok(cfg) = Config::from_payload(&p) {
                    screens = cfg
                        .screens
                        .into_iter()
                        .map(|s| ScreenInfo {
                            name: s.name,
                            node: s.node,
                            width: s.width,
                            height: s.height,
                        })
                        .collect();
                }
            }

            let st = state.request(&link, kind::GET_STATUS, &[], kind::STATUS, POLL_TIMEOUT);
            if let Ok((kind::STATUS, p)) = st {
                if !p.is_empty() {
                    let count = p[0] as usize;
                    slots = Vec::new();
                    for i in 0..count {
                        let off = 1 + i * SlotStatus::WIRE_LEN;
                        if let Some(slot) = p.get(off..).and_then(SlotStatus::decode) {
                            slots.push(slot);
                        }
                    }
                }
            }

            if let Ok(mut cell) = out.lock() {
                *cell = Snapshot::Ready { focus, screens: screens.clone(), slots: slots.clone() };
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

/// Read the layout through the control socket, for the settings process.
pub fn load_config() -> Result<Config> {
    let mut c = Client::connect()?;
    let (k, p) = c.request(kind::GET_CONFIG, &[])?;
    // An ACK here is a refusal, or the app saying nothing answered in time.
    // Reporting the raw kind ("unexpected reply 0x15") told the user nothing
    // about which of those it was.
    if k == kind::ACK {
        anyhow::bail!("could not read the layout: {}", ack_message(&p));
    }
    if k != kind::CONFIG {
        anyhow::bail!("the app answered a layout request with a {k:#04x} message");
    }
    Config::from_payload(&p)
}

/// Push a layout back through the control socket.
pub fn save_config(cfg: &Config) -> Result<()> {
    let payload = cfg.to_payload()?;
    let mut c = Client::connect()?;
    let (k, p) = c.request(kind::SET_CONFIG, &payload)?;
    if k != kind::ACK {
        anyhow::bail!("the app answered a layout write with a {k:#04x} message");
    }
    match p.first().copied().unwrap_or(scurry_proto::ack::BAD_REQUEST) {
        scurry_proto::ack::OK => Ok(()),
        scurry_proto::ack::INVALID_LAYOUT => anyhow::bail!("the dongle rejected the layout"),
        scurry_proto::ack::STORAGE_FAILED => anyhow::bail!("the dongle could not store the layout"),
        _ => anyhow::bail!("the layout was not stored: {}", ack_message(&p)),
    }
}
