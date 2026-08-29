//! Start and stop the scurry daemon from the tray.
//!
//! The tray and daemon are separate processes. The daemon holds the dongle's
//! serial port exclusively, so the tray drives the platform's service manager
//! rather than owning the process itself — that way the daemon's autostart and
//! restart policy survive the tray being closed.

use std::process::{Command, Stdio};

use anyhow::{bail, Result};

/// macOS LaunchAgent label. Shared with packaging/com.ananthb.scurry.plist.
#[cfg(target_os = "macos")]
pub const LAUNCHD_LABEL: &str = "com.ananthb.scurry";

/// systemd --user unit name on Linux.
#[cfg(target_os = "linux")]
pub const SYSTEMD_UNIT: &str = "scurry";

/// Whether daemon controls do anything on this platform.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "linux"));

#[cfg(target_os = "macos")]
fn launchd_domain() -> String {
    // gui/<uid>, not user/<uid>: the tray is a GUI agent and the daemon needs a
    // session it can tap input from.
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "macos")]
fn plist_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    p.exists().then_some(p)
}

/// Whether the service manager has a unit for us to drive.
///
/// Installing it is the installer's job -- Install Daemon.command on macOS, or
/// dropping the unit from the tarball on Linux. The tray only reports the
/// state, so it can say "not installed" instead of failing with a path the user
/// then has to interpret.
pub fn installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        return plist_path().is_some();
    }
    #[cfg(target_os = "linux")]
    {
        let Some(home) = std::env::var_os("HOME") else { return false };
        return std::path::PathBuf::from(home)
            .join(".config/systemd/user")
            .join(format!("{SYSTEMD_UNIT}.service"))
            .exists();
    }
    #[allow(unreachable_code)]
    false
}

/// Where the installer puts the unit, for the "not installed" hint.
pub fn install_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        return "Run “Install Daemon.command” from the scurry disk image";
    }
    #[cfg(target_os = "linux")]
    {
        return "Install scurry.service to ~/.config/systemd/user, then enable it";
    }
    #[allow(unreachable_code)]
    "Daemon installation is not supported on this platform"
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("{program} {} failed: {status}", args.join(" "));
    }
    Ok(())
}

pub fn start() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let target = format!("{}/{}", launchd_domain(), LAUNCHD_LABEL);
        // kickstart restarts an already-bootstrapped agent; bootstrap is only
        // needed the first time, and errors if it is already loaded.
        if run("launchctl", &["kickstart", &target]).is_ok() {
            return Ok(());
        }
        let Some(plist) = plist_path() else {
            bail!("no LaunchAgent at ~/Library/LaunchAgents/{LAUNCHD_LABEL}.plist");
        };
        return run("launchctl", &["bootstrap", &launchd_domain(), &plist.to_string_lossy()]);
    }
    #[cfg(target_os = "linux")]
    {
        return run("systemctl", &["--user", "start", SYSTEMD_UNIT]);
    }
    #[allow(unreachable_code)]
    {
        bail!("daemon control is not implemented on this platform")
    }
}

pub fn stop() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let target = format!("{}/{}", launchd_domain(), LAUNCHD_LABEL);
        return run("launchctl", &["bootout", &target]);
    }
    #[cfg(target_os = "linux")]
    {
        return run("systemctl", &["--user", "stop", SYSTEMD_UNIT]);
    }
    #[allow(unreachable_code)]
    {
        bail!("daemon control is not implemented on this platform")
    }
}

pub fn restart() -> Result<()> {
    let _ = stop();
    start()
}
