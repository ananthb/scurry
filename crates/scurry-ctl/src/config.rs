//! The virtual desktop, as declared on disk.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::layout::{Layout, Screen};

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Dongle device path. Autodetected when absent.
    pub device: Option<String>,
    #[serde(rename = "screen", default)]
    pub screens: Vec<ScreenConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ScreenConfig {
    pub name: String,
    /// 0 is the controller's own display and is never transmitted. Remote
    /// screens are 1..=4 and map onto the dongle's bonded connection slots in
    /// bond order.
    pub node: u8,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {path}"))?;
        let cfg: Config = toml::from_str(&text).context("parsing config")?;
        if cfg.screens.is_empty() {
            bail!("config declares no screens");
        }
        Ok(cfg)
    }

    pub fn into_layout(self) -> Result<Layout> {
        let screens = self
            .screens
            .into_iter()
            .map(|s| Screen {
                node: s.node,
                name: s.name,
                x: s.x,
                y: s.y,
                width: s.width,
                height: s.height,
            })
            .collect();
        Layout::new(screens).map_err(|e| anyhow::anyhow!("{e}"))
    }
}
