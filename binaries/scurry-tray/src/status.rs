//! What the tray knows about the daemon and the dongle.

use anyhow::Result;
use scurry_ctl::config::Config;
use scurry_ctl::ipc::{local, Client};
use scurry_proto::{kind, SlotStatus};

#[derive(Debug, Default, Clone)]
pub struct Status {
    /// False when the daemon is not running, which is the common case and not
    /// an error worth shouting about.
    pub daemon_running: bool,
    /// Node currently holding the pointer; 0 is this machine.
    pub focus: u8,
    pub slots: Vec<SlotStatus>,
    pub screens: Vec<ScreenInfo>,
}

#[derive(Debug, Clone)]
pub struct ScreenInfo {
    pub name: String,
    pub node: u8,
    pub width: i32,
    pub height: i32,
}

impl Status {
    /// Ask the daemon for everything the menu needs.
    ///
    /// A daemon that is not running is reported as such rather than as a
    /// failure: the tray is expected to sit there quietly until one starts.
    pub fn fetch() -> Self {
        let mut s = Status::default();
        let Ok(mut c) = Client::connect() else {
            return s;
        };
        s.daemon_running = true;

        if let Ok((k, p)) = c.request(local::GET_DAEMON_STATUS, &[]) {
            if k == local::DAEMON_STATUS {
                s.focus = p.first().copied().unwrap_or(0);
            }
        }
        if let Ok((k, p)) = c.request(kind::GET_STATUS, &[]) {
            if k == kind::STATUS && !p.is_empty() {
                let count = p[0] as usize;
                for i in 0..count {
                    let off = 1 + i * SlotStatus::WIRE_LEN;
                    if let Some(slot) = p.get(off..).and_then(SlotStatus::decode) {
                        s.slots.push(slot);
                    }
                }
            }
        }
        if let Ok((k, p)) = c.request(kind::GET_CONFIG, &[]) {
            if k == kind::CONFIG {
                if let Ok(cfg) = Config::from_payload(&p) {
                    s.screens = cfg
                        .screens
                        .into_iter()
                        .map(|sc| ScreenInfo {
                            name: sc.name,
                            node: sc.node,
                            width: sc.width,
                            height: sc.height,
                        })
                        .collect();
                }
            }
        }
        s
    }

    /// Name for a node id, falling back to the id when the layout has no entry.
    pub fn screen_name(&self, node: u8) -> String {
        self.screens
            .iter()
            .find(|s| s.node == node)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("node {node}"))
    }
}

/// Read the layout as a `Config`, for the settings pane.
pub fn load_config() -> Result<Config> {
    let mut c = Client::connect()?;
    let (k, p) = c.request(kind::GET_CONFIG, &[])?;
    if k != kind::CONFIG {
        anyhow::bail!("unexpected reply {k:#04x} to a config request");
    }
    Config::from_payload(&p)
}

/// Push a layout back to the dongle through the daemon.
pub fn save_config(cfg: &Config) -> Result<()> {
    let payload = cfg.to_payload()?;
    let mut c = Client::connect()?;
    let (k, p) = c.request(kind::SET_CONFIG, &payload)?;
    if k != kind::ACK {
        anyhow::bail!("unexpected reply {k:#04x} to a config write");
    }
    match p.first().copied().unwrap_or(scurry_proto::ack::BAD_REQUEST) {
        scurry_proto::ack::OK => Ok(()),
        scurry_proto::ack::INVALID_LAYOUT => anyhow::bail!("the dongle rejected the layout"),
        scurry_proto::ack::STORAGE_FAILED => anyhow::bail!("the dongle could not store the layout"),
        _ => anyhow::bail!("the dongle refused the request"),
    }
}
