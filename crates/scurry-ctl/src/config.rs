//! Layout as data: TOML on the way in, wire payload on the way out.
//!
//! The dongle is the only place a layout is stored. This module exists to turn
//! something a human wrote into a `SET_CONFIG` payload, and to render a
//! `CONFIG` payload back for inspection. Nothing here persists.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use scurry_layout::{Layout, Screen};
use scurry_proto::{ScreenWire, MAX_SCREENS, SCREEN_WIRE_LEN};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(rename = "screen", default)]
    pub screens: Vec<ScreenConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScreenConfig {
    pub name: String,
    /// 0 is the controller's own display and is never transmitted. Remote
    /// screens are 1..=4, mapping onto the dongle's bonded connection slots.
    pub node: u8,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Bluetooth address of the machine at this screen, "aa:bb:cc:dd:ee:ff".
    ///
    /// Optional. Without it the node is whichever machine connects into that
    /// slot first, which reverses two screens whenever a reboot resolves the
    /// race the other way. `scurry-ctl status` prints the addresses to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// Parse "aa:bb:cc:dd:ee:ff". Also accepts bare hex, since that is how the
/// dongle's own logs print addresses.
pub fn parse_bda(text: &str) -> Result<[u8; 6]> {
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        bail!("{text:?} is not a Bluetooth address (need 12 hex digits, got {})", hex.len());
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("parsing {text:?}"))?;
    }
    Ok(out)
}

pub fn format_bda(bda: [u8; 6]) -> String {
    bda.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading layout at {path}"))?;
        let cfg: Config = toml::from_str(&text).context("parsing layout")?;
        if cfg.screens.is_empty() {
            bail!("layout declares no screens");
        }
        Ok(cfg)
    }

    /// Check the layout the same way the dongle will.
    ///
    /// The dongle validates before storing, so this is not load-bearing for
    /// correctness -- but failing here gives a specific message instead of a
    /// bare INVALID_LAYOUT ack from across the wire.
    pub fn validate(&self) -> Result<()> {
        if self.screens.len() > MAX_SCREENS {
            bail!("{} screens declared, the dongle holds at most {MAX_SCREENS}", self.screens.len());
        }
        let screens: Vec<Screen> = self
            .screens
            .iter()
            .map(|s| Screen::new(s.node, &s.name, s.x, s.y, s.width, s.height))
            .collect();
        for s in &self.screens {
            if s.width <= 0 || s.height <= 0 {
                bail!("screen {:?} has a non-positive size", s.name);
            }
        }
        Layout::new(&screens).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Encode as a `SET_CONFIG` payload: a count byte then that many screens.
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out = vec![self.screens.len() as u8];
        out.resize(1 + self.screens.len() * SCREEN_WIRE_LEN, 0);
        for (i, s) in self.screens.iter().enumerate() {
            let bda = match &s.address {
                Some(a) => parse_bda(a)?,
                None => [0u8; 6],
            };
            let w = ScreenWire::pinned(s.node, &s.name, s.x, s.y, s.width, s.height, bda);
            w.encode_into(&mut out[1 + i * SCREEN_WIRE_LEN..]);
        }
        Ok(out)
    }

    /// Decode a `CONFIG` payload.
    pub fn from_payload(p: &[u8]) -> Result<Self> {
        if p.is_empty() {
            bail!("empty config payload");
        }
        let count = p[0] as usize;
        if count > MAX_SCREENS {
            bail!("dongle reported {count} screens, more than the {MAX_SCREENS} maximum");
        }
        if p.len() < 1 + count * SCREEN_WIRE_LEN {
            bail!("config payload is short: {} bytes for {count} screens", p.len());
        }
        let mut screens = Vec::with_capacity(count);
        for i in 0..count {
            let w = ScreenWire::decode(&p[1 + i * SCREEN_WIRE_LEN..])
                .context("decoding a screen")?;
            screens.push(ScreenConfig {
                name: w.name_str().to_string(),
                node: w.node,
                x: w.x,
                y: w.y,
                width: w.width,
                height: w.height,
                address: w.is_pinned().then(|| format_bda(w.bda)),
            });
        }
        Ok(Config { screens })
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serialising layout")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            screens: vec![
                ScreenConfig {
                    name: "mac".into(),
                    node: 0,
                    x: 0,
                    y: 0,
                    width: 1512,
                    height: 982,
                    address: None,
                },
                ScreenConfig {
                    name: "chromebook".into(),
                    node: 1,
                    x: 1512,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    address: Some("f0:68:e3:e5:d1:b1".into()),
                },
            ],
        }
    }

    #[test]
    fn payload_roundtrips() {
        let cfg = sample();
        let back = Config::from_payload(&cfg.to_payload().unwrap()).unwrap();
        assert_eq!(back.screens.len(), 2);
        assert_eq!(back.screens[1].name, "chromebook");
        assert_eq!(back.screens[1].width, 1920);
        assert_eq!(back.screens[0].node, 0);
    }

    #[test]
    fn rejects_layouts_the_dongle_would_reject() {
        // Overlapping screens: containment would be ambiguous, so handoff would
        // be non-deterministic. Caught here rather than as a bare ack.
        let mut cfg = sample();
        cfg.screens[1].x = 1000;
        assert!(cfg.to_payload().is_err());

        // No local screen at all.
        let mut cfg = sample();
        cfg.screens[0].node = 2;
        assert!(cfg.to_payload().is_err());

        // A zero-size screen would make the ratio arithmetic meaningless.
        let mut cfg = sample();
        cfg.screens[1].height = 0;
        assert!(cfg.to_payload().is_err());
    }

    #[test]
    fn addresses_survive_the_round_trip() {
        let cfg = sample();
        let back = Config::from_payload(&cfg.to_payload().unwrap()).unwrap();
        assert_eq!(back.screens[1].address.as_deref(), Some("f0:68:e3:e5:d1:b1"));
        // An unpinned screen must come back unpinned, not as all-zeroes text.
        assert_eq!(back.screens[0].address, None);
    }

    #[test]
    fn address_parsing_accepts_both_spellings() {
        let want = [0xf0, 0x68, 0xe3, 0xe5, 0xd1, 0xb1];
        assert_eq!(parse_bda("f0:68:e3:e5:d1:b1").unwrap(), want);
        // The dongle's own logs print addresses unseparated.
        assert_eq!(parse_bda("f068e3e5d1b1").unwrap(), want);
        assert!(parse_bda("f0:68:e3").is_err());
        assert!(parse_bda("zz:68:e3:e5:d1:b1").is_err());
    }

    #[test]
    fn short_payload_is_an_error_not_a_panic() {
        assert!(Config::from_payload(&[]).is_err());
        assert!(Config::from_payload(&[2, 0, 0]).is_err(), "claims 2 screens, carries none");
    }
}
