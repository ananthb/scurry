//! "Open at login", handled by the app rather than by the user.
//!
//! Writes a LaunchAgent pointing at wherever this binary actually is, so the
//! same toggle works for an app in /Applications and for a `cargo run` build.
//! There is deliberately no installer step and no file for anyone to place by
//! hand: dragging the app across is the whole installation.

use std::path::PathBuf;

use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
pub const LABEL: &str = "com.ananthb.scurry";

#[cfg(target_os = "macos")]
fn plist_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "linux")]
fn autostart_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("scurry-tray.desktop"))
}

pub fn enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        return plist_path().is_some_and(|p| p.exists());
    }
    #[cfg(target_os = "linux")]
    {
        return autostart_path().is_some_and(|p| p.exists());
    }
    #[allow(unreachable_code)]
    false
}

/// The binary to launch at login.
///
/// Inside a bundle this resolves to scurry.app/Contents/MacOS/scurry-tray,
/// which is what makes the toggle survive the app being moved: the plist is
/// rewritten from wherever the app is running now.
fn program() -> Result<PathBuf> {
    std::env::current_exe().context("locating own binary")
}

pub fn set(enable: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let path = plist_path().context("no HOME to place a LaunchAgent in")?;
        if !enable {
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("gui/{}/{LABEL}", unsafe { libc::getuid() })])
                .status();
            if path.exists() {
                std::fs::remove_file(&path).context("removing the LaunchAgent")?;
            }
            return Ok(());
        }

        let exe = program()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        // KeepAlive is deliberately absent. This is a UI app: if the user quits
        // it from the menu, relaunching it immediately would make it impossible
        // to stop.
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
"#,
            exe.display()
        );
        std::fs::write(&path, plist).context("writing the LaunchAgent")?;
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let path = autostart_path().context("no config directory for an autostart entry")?;
        if !enable {
            if path.exists() {
                std::fs::remove_file(&path).context("removing the autostart entry")?;
            }
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        let exe = program()?;
        let entry = crate::packaging::desktop_entry()
            .replace("Exec=scurry-tray", &format!("Exec={}", exe.display()));
        std::fs::write(&path, entry).context("writing the autostart entry")?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        let _ = enable;
        anyhow::bail!("open at login is not supported on this platform")
    }
}

pub fn toggle() -> Result<()> {
    set(!enabled())
}
